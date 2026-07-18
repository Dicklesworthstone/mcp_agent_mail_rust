//! MCP tools and resources implementation for MCP Agent Mail
//!
//! This crate provides implementations for all 37 MCP tools:
//! - Infrastructure cluster (4 tools)
//! - Identity cluster (6 tools)
//! - Messaging cluster (5 tools)
//! - Contact cluster (4 tools)
//! - File reservation cluster (4 tools)
//! - Search cluster (2 tools)
//! - Workflow macro cluster (4 tools)
//! - Product bus cluster (5 tools)
//! - Build slot cluster (3 tools)
//!
//! And 25 MCP resources for read-only data access.

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
pub mod degraded_intents;
pub mod identity;
pub mod llm;
pub mod macros;
pub mod messaging;
pub mod metrics;
pub mod products;
pub mod proof_gate;
pub mod reservation_index;
pub mod reservation_parity;
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
pub use reservation_parity::*;
pub use reservations::*;
pub use resources::*;
pub use search::*;

pub mod tool_util {
    use fastmcp::McpErrorCode;
    use fastmcp::prelude::*;
    use mcp_agent_mail_core::Config;
    use mcp_agent_mail_db::{DbError, DbPool, DbPoolConfig, get_cached_pool, get_or_create_pool};
    use serde_json::{Map, Value, json};
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeSet, VecDeque};
    use std::io::Read as _;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::{Duration, Instant, UNIX_EPOCH};

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

    fn resource_busy_message(message: &str) -> String {
        if mcp_agent_mail_db::is_mailbox_ownership_contention(message) {
            format!(
                "Resource is temporarily busy: a running Agent Mail server owns this mailbox. \
                 Route this operation through that server (or stop it) instead of writing \
                 directly. Detail: {message}"
            )
        } else {
            "Resource is temporarily busy. Wait a moment and try again.".to_string()
        }
    }

    fn db_error_classification_data(
        classification: mcp_agent_mail_db::DbErrorClassification,
    ) -> serde_json::Value {
        json!({
            "class": classification.class.as_str(),
            "severity": classification.severity.as_str(),
            "repairable": classification.repairable,
            "safe_to_retry": classification.safe_to_retry,
            "safe_to_continue_read_only": classification.safe_to_continue_read_only,
            "blocks_edits": classification.blocks_edits,
            "recommended_command": classification.recommended_command,
        })
    }

    fn db_failure_envelope_data(envelope: &mcp_agent_mail_db::DbFailureEnvelope) -> Value {
        serde_json::to_value(envelope).unwrap_or_else(|err| {
            json!({
                "schema_version": mcp_agent_mail_db::DB_FAILURE_ENVELOPE_SCHEMA_VERSION,
                "serialization_error": err.to_string(),
            })
        })
    }

    fn db_error_data(
        classification: mcp_agent_mail_db::DbErrorClassification,
        failure_envelope: &mcp_agent_mail_db::DbFailureEnvelope,
        extra: Value,
    ) -> Value {
        let mut object = match extra {
            Value::Object(object) => object,
            _ => Map::new(),
        };
        object.insert(
            "db_error_classification".to_string(),
            db_error_classification_data(classification),
        );
        object.insert(
            "failure_envelope".to_string(),
            db_failure_envelope_data(failure_envelope),
        );
        Value::Object(object)
    }

    /// Build the JSON retry-context block for a spent retry budget (D3).
    fn retry_exhaustion_data(
        operation: &'static str,
        attempts: u32,
        budget: u32,
        elapsed_ms: u64,
    ) -> Value {
        json!({
            "operation": operation,
            "attempts_made": attempts,
            "retry_budget": budget,
            "elapsed_wait_ms": elapsed_ms,
            "budget_exhausted": true,
            "immediate_retry_useful": false,
        })
    }

    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn db_error_to_mcp_error(e: DbError) -> McpError {
        let classification = e.classification();
        let failure_envelope = e.failure_envelope();
        // A5 (br-bvq1x.1.5): record the typed class at the single chokepoint
        // where a DB error is surfaced to a caller, so corruption-class trend
        // counters (and the K3 circuit breaker) see every classified failure
        // exactly once.
        mcp_agent_mail_core::global_metrics()
            .corruption
            .record_class(classification.class.as_str());
        match e {
            // D3 (br-bvq1x.4.3): a bounded retry loop already spent its
            // budget. Render an honest, class-distinct envelope that reports
            // the attempts made and elapsed wait instead of advising another
            // blind retry. Classification delegates to the wrapped error, so
            // `classification.class` is the wrapped error's class.
            DbError::RetryBudgetExhausted {
                operation,
                attempts,
                budget,
                elapsed_ms,
                inner,
            } => {
                let inner_detail = inner.to_string();
                let retry_data = retry_exhaustion_data(operation, attempts, budget, elapsed_ms);
                match classification.class {
                    mcp_agent_mail_db::DbErrorClass::FdExhaustion => {
                        let freed = mcp_agent_mail_db::fd_eviction_freed(&inner_detail);
                        let freed_zero = freed == Some(0);
                        let message = if freed_zero {
                            format!(
                                "File descriptor limit exhausted and repo-cache eviction freed \
                                 nothing after {attempts} attempts. Do NOT retry: close stale \
                                 Agent Mail processes, raise the open-file limit (ulimit -n), \
                                 or restart the owning server, then run `am doctor health`."
                            )
                        } else {
                            format!(
                                "File descriptor limit exhausted ({attempts} attempts over \
                                 {elapsed_ms} ms). Close stale Agent Mail processes or raise \
                                 the open-file limit, then retry once."
                            )
                        };
                        legacy_tool_error(
                            "RESOURCE_BUSY",
                            message,
                            !freed_zero,
                            db_error_data(
                                classification,
                                &failure_envelope,
                                json!({
                                    "error_detail": inner_detail,
                                    "resource_class": "file_descriptors",
                                    "eviction_freed": freed,
                                    "retry_exhaustion": retry_data,
                                }),
                            ),
                        )
                    }
                    mcp_agent_mail_db::DbErrorClass::PoolExhaustion => legacy_tool_error(
                        "DATABASE_POOL_EXHAUSTED",
                        format!(
                            "Database connection pool exhausted; the server already retried \
                             {attempts} times over {elapsed_ms} ms. Reduce concurrent agents \
                             or increase pool settings before retrying."
                        ),
                        true,
                        db_error_data(
                            classification,
                            &failure_envelope,
                            json!({
                                "error_detail": inner_detail,
                                "retry_exhaustion": retry_data,
                            }),
                        ),
                    ),
                    mcp_agent_mail_db::DbErrorClass::LiveOwnerNoActivityLock => legacy_tool_error(
                        "RESOURCE_BUSY",
                        format!(
                            "Resource is busy: a running Agent Mail server owns this mailbox \
                             and {attempts} direct-write attempts over {elapsed_ms} ms were \
                             refused. Route this operation through that server instead of \
                             retrying direct writes. Detail: {inner_detail}"
                        ),
                        true,
                        db_error_data(
                            classification,
                            &failure_envelope,
                            json!({
                                "error_detail": inner_detail,
                                "retry_exhaustion": retry_data,
                            }),
                        ),
                    ),
                    mcp_agent_mail_db::DbErrorClass::BusyRetryable => legacy_tool_error(
                        "RESOURCE_BUSY",
                        format!(
                            "Resource is temporarily busy and the retry budget is exhausted \
                             ({attempts} attempts over {elapsed_ms} ms). Do not immediately \
                             retry: run `am doctor locks --json` to identify the lock holder, \
                             wait for it to clear, then try once more."
                        ),
                        true,
                        db_error_data(
                            classification,
                            &failure_envelope,
                            json!({
                                "error_detail": inner_detail,
                                "retry_exhaustion": retry_data,
                            }),
                        ),
                    ),
                    // Corruption and config classes are never retried by the
                    // bounded loops; if one ever arrives wrapped, fall back to
                    // the wrapped error's own distinct envelope.
                    _ => db_error_to_mcp_error(*inner),
                }
            }
            DbError::InvalidArgument { field, message } => legacy_tool_error(
                "INVALID_ARGUMENT",
                format!(
                    "Invalid argument value: {field}: {message}. Check that all parameters have valid values."
                ),
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "field": field,
                        "error_detail": message,
                    }),
                ),
            ),
            DbError::NotFound { entity, identifier } => legacy_tool_error(
                "NOT_FOUND",
                format!("{entity} not found: {identifier}"),
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "entity": entity,
                        "identifier": identifier,
                    }),
                ),
            ),
            DbError::Duplicate { entity, identifier } => legacy_tool_error(
                "INVALID_ARGUMENT",
                format!("{entity} already exists: {identifier}"),
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "entity": entity,
                        "identifier": identifier,
                    }),
                ),
            ),
            DbError::Sqlite(ref message)
            | DbError::Schema(ref message)
            | DbError::Pool(ref message)
                if e.is_corruption() =>
            {
                let message = message.clone();
                legacy_tool_error(
                    "DATABASE_CORRUPTION",
                    format!(
                        "Database corruption detected: {message}. \
                         Run 'am doctor repair' or 'am doctor reconstruct' to recover."
                    ),
                    false,
                    db_error_data(
                        classification,
                        &failure_envelope,
                        json!({
                            "error_detail": message,
                        }),
                    ),
                )
            }
            DbError::Sqlite(ref message)
            | DbError::Schema(ref message)
            | DbError::Pool(ref message)
            | DbError::Internal(ref message)
                if mcp_agent_mail_db::is_fd_exhaustion_error(message) =>
            {
                let message = message.clone();
                // D3 (br-bvq1x.4.3): when the failing path reported that
                // repo-cache eviction freed nothing, another retry will
                // deterministically fail — stop advising it.
                let freed = mcp_agent_mail_db::fd_eviction_freed(&message);
                let freed_zero = freed == Some(0);
                let (display, action) = if freed_zero {
                    (
                        "File descriptor limit exhausted and repo-cache eviction freed nothing. \
                         Do NOT retry: close stale Agent Mail processes, raise the open-file \
                         limit (ulimit -n), or restart the owning server, then run \
                         `am doctor health`.",
                        "do not retry; close stale Agent Mail processes, raise the open-file \
                         limit, or restart the owning server",
                    )
                } else {
                    (
                        "File descriptor limit exhausted. Close stale Agent Mail processes or raise the open-file limit, then retry.",
                        "close stale Agent Mail processes or raise the open-file limit, then retry",
                    )
                };
                legacy_tool_error(
                    "RESOURCE_BUSY",
                    display,
                    !freed_zero,
                    db_error_data(
                        classification,
                        &failure_envelope,
                        json!({
                            "error_detail": message,
                            "resource_class": "file_descriptors",
                            "eviction_freed": freed,
                            "recommended_action": action,
                        }),
                    ),
                )
            }
            DbError::Sqlite(ref message)
            | DbError::Schema(ref message)
            | DbError::Pool(ref message)
                if mcp_agent_mail_db::is_lock_error(message) =>
            {
                let message = message.clone();
                // #139: mailbox ownership contention (a long-running
                // `am serve-http` daemon holds the activity lock and a direct
                // mutation was refused) is still RESOURCE_BUSY, but the actionable
                // hint differs from a transient SQLITE_BUSY: the caller should route
                // the write through the running server rather than blindly retrying
                // a direct write that will keep losing the ownership race.
                legacy_tool_error(
                    "RESOURCE_BUSY",
                    resource_busy_message(&message),
                    true,
                    db_error_data(
                        classification,
                        &failure_envelope,
                        json!({
                            "error_detail": message,
                        }),
                    ),
                )
            }
            DbError::Pool(message) => legacy_tool_error(
                "DATABASE_POOL_EXHAUSTED",
                "Database connection pool exhausted. Reduce concurrency or increase pool settings.",
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "error_detail": message,
                    }),
                ),
            ),
            DbError::Sqlite(message) | DbError::Schema(message) => legacy_tool_error(
                "DATABASE_ERROR",
                "A database error occurred. This may be a transient issue - try again.",
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "error_detail": message,
                    }),
                ),
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
                    db_error_data(
                        classification,
                        &failure_envelope,
                        json!({ "error_detail": message }),
                    ),
                )
            }
            DbError::Internal(message) if is_retryable_post_commit_visibility_probe(&message) => {
                legacy_tool_error(
                    "RESOURCE_BUSY",
                    "Resource is temporarily busy. Wait a moment and try again.",
                    true,
                    db_error_data(
                        classification,
                        &failure_envelope,
                        json!({
                            "error_detail": message,
                        }),
                    ),
                )
            }
            DbError::Internal(message) => legacy_tool_error(
                "UNHANDLED_EXCEPTION",
                format!("Unexpected error (DbError): {message}"),
                false,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "error_detail": message,
                    }),
                ),
            ),
            DbError::PoolExhausted {
                message,
                pool_size,
                max_overflow,
            } => legacy_tool_error(
                "DATABASE_POOL_EXHAUSTED",
                "Database connection pool exhausted. Reduce concurrency or increase pool settings.",
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "error_detail": message,
                        "pool_size": pool_size,
                        "max_overflow": max_overflow,
                    }),
                ),
            ),
            DbError::ResourceBusy(message) => legacy_tool_error(
                "RESOURCE_BUSY",
                resource_busy_message(&message),
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "error_detail": message,
                    }),
                ),
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
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "error_detail": message,
                        "failures": failures,
                        "reset_after_secs": reset_after_secs,
                    }),
                ),
            ),
            DbError::IntegrityCorruption { message, details }
                if classification.class == mcp_agent_mail_db::DbErrorClass::FtsIndexCorruption =>
            {
                legacy_tool_error(
                    "DATABASE_ERROR",
                    format!(
                        "Search index corruption detected: {message}. \
                         Run 'am doctor fix --list --json' to inspect repair options."
                    ),
                    true,
                    db_error_data(
                        classification,
                        &failure_envelope,
                        json!({
                            "error_detail": message,
                            "corruption_details": details,
                        }),
                    ),
                )
            }
            DbError::IntegrityCorruption { message, details } => legacy_tool_error(
                "DATABASE_CORRUPTION",
                format!(
                    "Database integrity check failed: {message}. \
                     The database may be corrupted; consider restoring from backup."
                ),
                false,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "error_detail": message,
                        "corruption_details": details,
                    }),
                ),
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

    pub(crate) fn get_live_db_pool() -> McpResult<DbPool> {
        let mut cfg = DbPoolConfig::from_env();
        if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&cfg.database_url) {
            return get_or_create_pool(&cfg).map_err(|e| McpError::internal_error(e.to_string()));
        }
        cfg.run_migrations = false;
        cfg.warmup_connections = 0;
        mcp_agent_mail_db::create_query_only_pool(&cfg)
            .map_err(|e| McpError::internal_error(e.to_string()))
    }

    struct ReadSnapshotWriteGuard {
        slot: Option<Arc<ReadSnapshotScopeSlot>>,
        active: bool,
    }

    impl ReadSnapshotWriteGuard {
        fn begin(storage_root: &Path, sqlite_path: Option<&Path>) -> Self {
            let Some(sqlite_path) = sqlite_path else {
                return Self {
                    slot: None,
                    active: false,
                };
            };
            Self {
                slot: begin_read_snapshot_write(storage_root, sqlite_path),
                active: true,
            }
        }
    }

    impl Drop for ReadSnapshotWriteGuard {
        fn drop(&mut self) {
            if self.active {
                end_read_snapshot_write(self.slot.as_deref());
            }
        }
    }

    /// A live pool lease for mutation paths. Dropping the lease advances the
    /// mailbox read-snapshot epoch after the operation has finished, so an
    /// in-flight reconstruction cannot publish across a durable write window.
    pub struct WriteDbPool {
        pool: DbPool,
        _write_guard: ReadSnapshotWriteGuard,
    }

    impl std::ops::Deref for WriteDbPool {
        type Target = DbPool;

        fn deref(&self) -> &Self::Target {
            &self.pool
        }
    }

    pub fn get_db_pool() -> McpResult<WriteDbPool> {
        let cfg = DbPoolConfig::from_env();
        let sqlite_path =
            if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&cfg.database_url) {
                None
            } else {
                let resolved =
                    mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(&cfg.database_url)
                        .map_err(|error| {
                            McpError::internal_error(format!(
                                "resolve live SQLite writer path before pool bootstrap: {error}"
                            ))
                        })?;
                Some(PathBuf::from(resolved.canonical_path))
            };
        let storage_root = cfg
            .storage_root
            .clone()
            .unwrap_or_else(|| Config::from_env().storage_root);
        // Enter the writer epoch before even constructing a cold pool: pool
        // bootstrap can recover, migrate, checkpoint, or create the live DB.
        // The guard closes the epoch on every early-return/error path.
        let write_guard = ReadSnapshotWriteGuard::begin(&storage_root, sqlite_path.as_deref());
        let pool = get_or_create_pool(&cfg).map_err(|e| McpError::internal_error(e.to_string()))?;
        Ok(WriteDbPool {
            pool,
            _write_guard: write_guard,
        })
    }

    /// Return the authoritative live pool for a read-only transaction.
    ///
    /// Hot reads reuse the existing live pool without entering a writer epoch.
    /// A cold pool bootstrap may recover, migrate, checkpoint, or create the
    /// mailbox, so only that bootstrap window is bracketed as a write before the
    /// plain pool is returned to the caller.
    pub(crate) fn get_authoritative_live_db_pool() -> McpResult<DbPool> {
        let cfg = DbPoolConfig::from_env();
        if let Some(pool) = get_cached_pool(&cfg) {
            return Ok(pool);
        }

        let sqlite_path =
            if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&cfg.database_url) {
                None
            } else {
                let resolved =
                    mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(&cfg.database_url)
                        .map_err(|error| {
                            McpError::internal_error(format!(
                                "resolve authoritative SQLite path before pool bootstrap: {error}"
                            ))
                        })?;
                Some(PathBuf::from(resolved.canonical_path))
            };
        let storage_root = cfg
            .storage_root
            .clone()
            .unwrap_or_else(|| Config::from_env().storage_root);
        let bootstrap_guard = ReadSnapshotWriteGuard::begin(&storage_root, sqlite_path.as_deref());
        let pool = get_or_create_pool(&cfg).map_err(|error| {
            McpError::internal_error(format!("open authoritative live database pool: {error}"))
        })?;
        drop(bootstrap_guard);
        Ok(pool)
    }

    #[cfg(test)]
    pub(crate) fn read_snapshot_db_write_epoch_for_test() -> u64 {
        READ_SNAPSHOT_DB_WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire)
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadArchiveSignature {
        storage_root: PathBuf,
        head: git2::Oid,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadArchiveInventoryCacheEntry {
        signature: ReadArchiveSignature,
        inventory: mcp_agent_mail_db::ArchiveMessageInventory,
    }

    const READ_ARCHIVE_INVENTORY_CACHE_CAPACITY: usize = 8;

    // Archive inventory scans parse every canonical message artifact. Cache a
    // small number of clean, committed archive generations so independent
    // mailbox roots do not evict one another on every read.
    static READ_ARCHIVE_INVENTORY_CACHE: LazyLock<Mutex<VecDeque<ReadArchiveInventoryCacheEntry>>> =
        LazyLock::new(|| Mutex::new(VecDeque::new()));

    #[cfg(test)]
    static READ_ARCHIVE_INVENTORY_SCAN_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn read_archive_head(repo: &git2::Repository) -> Option<git2::Oid> {
        repo.head()
            .ok()?
            .peel_to_commit()
            .ok()
            .map(|commit| commit.id())
    }

    /// Return a cacheable archive signature only when `projects/` is fully
    /// represented by one stable commit. Any worktree, index, ignored, or
    /// untracked state bypasses the cache because archive writes are durable in
    /// the worktree before the asynchronous Git coalescer advances `HEAD`.
    fn clean_read_archive_signature(storage_root: &Path) -> Option<ReadArchiveSignature> {
        let canonical_root = storage_root.canonicalize().ok()?;
        let repo = git2::Repository::open(&canonical_root).ok()?;
        let canonical_workdir = repo.workdir()?.canonicalize().ok()?;
        if canonical_workdir != canonical_root {
            return None;
        }

        let head_before = read_archive_head(&repo)?;
        let mut status_options = git2::StatusOptions::new();
        status_options
            .show(git2::StatusShow::IndexAndWorkdir)
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(true)
            .recurse_ignored_dirs(true)
            .include_unmodified(false)
            .exclude_submodules(false)
            .pathspec("projects");
        if !repo.statuses(Some(&mut status_options)).ok()?.is_empty() {
            return None;
        }
        let head_after = read_archive_head(&repo)?;
        if head_before != head_after {
            return None;
        }

        Some(ReadArchiveSignature {
            storage_root: canonical_root,
            head: head_after,
        })
    }

    fn scan_read_archive_inventory(
        storage_root: &Path,
    ) -> mcp_agent_mail_db::ArchiveMessageInventory {
        #[cfg(test)]
        READ_ARCHIVE_INVENTORY_SCAN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        mcp_agent_mail_db::scan_archive_message_inventory(storage_root)
    }

    fn scan_read_archive_inventory_cancellable(
        storage_root: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<mcp_agent_mail_db::ArchiveMessageInventory, ReadSnapshotAcquireError> {
        #[cfg(test)]
        READ_ARCHIVE_INVENTORY_SCAN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        mcp_agent_mail_db::scan_archive_message_inventory_cancellable(storage_root, cancelled)
            .map_err(ReadSnapshotAcquireError::failed)
    }

    fn cached_read_archive_inventory(
        signature: &ReadArchiveSignature,
    ) -> Option<mcp_agent_mail_db::ArchiveMessageInventory> {
        let mut cache = READ_ARCHIVE_INVENTORY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = cache
            .iter()
            .position(|entry| entry.signature == *signature)?;
        let entry = cache.remove(index)?;
        let inventory = entry.inventory.clone();
        cache.push_back(entry);
        drop(cache);
        Some(inventory)
    }

    fn cache_read_archive_inventory(
        signature: ReadArchiveSignature,
        inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
    ) {
        let mut cache = READ_ARCHIVE_INVENTORY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|entry| entry.signature.storage_root != signature.storage_root);
        cache.push_back(ReadArchiveInventoryCacheEntry {
            signature,
            inventory: inventory.clone(),
        });
        while cache.len() > READ_ARCHIVE_INVENTORY_CACHE_CAPACITY {
            cache.pop_front();
        }
        drop(cache);
    }

    pub(crate) fn read_archive_inventory(
        storage_root: &Path,
    ) -> mcp_agent_mail_db::ArchiveMessageInventory {
        let Some(signature_before) = clean_read_archive_signature(storage_root) else {
            return scan_read_archive_inventory(storage_root);
        };
        if let Some(inventory) = cached_read_archive_inventory(&signature_before) {
            return inventory;
        }

        // Do not hold the cache mutex across filesystem I/O. Cache the result
        // only when both HEAD and worktree cleanliness remain unchanged across
        // the full scan; a concurrent writer otherwise gets the conservative
        // uncached behavior on this and subsequent reads.
        let inventory = scan_read_archive_inventory(storage_root);
        if clean_read_archive_signature(storage_root).as_ref() == Some(&signature_before) {
            cache_read_archive_inventory(signature_before, &inventory);
        }
        inventory
    }

    fn read_archive_inventory_cancellable(
        storage_root: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<mcp_agent_mail_db::ArchiveMessageInventory, ReadSnapshotAcquireError> {
        if cancelled() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        let Some(signature_before) = clean_read_archive_signature(storage_root) else {
            return scan_read_archive_inventory_cancellable(storage_root, cancelled);
        };
        if let Some(inventory) = cached_read_archive_inventory(&signature_before) {
            return Ok(inventory);
        }

        let inventory = scan_read_archive_inventory_cancellable(storage_root, cancelled)?;
        if cancelled() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        if clean_read_archive_signature(storage_root).as_ref() == Some(&signature_before) {
            cache_read_archive_inventory(signature_before, &inventory);
        }
        Ok(inventory)
    }

    #[cfg(test)]
    pub(crate) fn reset_read_archive_inventory_cache() {
        READ_ARCHIVE_INVENTORY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        READ_ARCHIVE_INVENTORY_SCAN_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn read_archive_inventory_scan_count() -> usize {
        READ_ARCHIVE_INVENTORY_SCAN_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    fn read_archive_inventory_cache_len() -> usize {
        READ_ARCHIVE_INVENTORY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    const READ_SNAPSHOT_SCOPE_CAPACITY: usize = 8;
    const READ_SNAPSHOT_WORKER_LIMIT: usize = 4;
    // Internal writes explicitly invalidate their mailbox epoch. The TTL is a
    // fallback for external writers; an expired decision is served stale while
    // one background worker validates cheap metadata/epochs. A separately
    // configurable audit interval forces the exact content key periodically.
    const READ_SNAPSHOT_VALIDATION_TTL_DEFAULT: Duration = Duration::from_secs(1);
    const READ_SNAPSHOT_EXACT_AUDIT_INTERVAL_DEFAULT: Duration = Duration::from_secs(30);
    const READ_SNAPSHOT_WAIT_SLICE: Duration = Duration::from_millis(50);
    const READ_SNAPSHOT_BUILD_TIMEOUT: Duration = Duration::from_secs(120);
    const READ_SNAPSHOT_GENERATION_RETRIES: usize = 3;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadSnapshotScope {
        storage_root: PathBuf,
        sqlite_path: PathBuf,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadSnapshotKey {
        archive_digest: [u8; 32],
        live_db_digest: [u8; 32],
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadSnapshotCheapKey {
        archive_metadata_digest: [u8; 32],
        live_db_metadata_digest: [u8; 32],
        live_data_version: Option<i64>,
        db_write_epoch: u64,
        archive_application_epoch: u64,
    }

    pub(crate) struct SharedArchiveReadSnapshot {
        pool: mcp_agent_mail_db::DbPool,
        _snapshot_dir: mcp_agent_mail_db::pool::CanonicalSnapshotTempDir,
    }

    impl SharedArchiveReadSnapshot {
        pub(crate) fn pool(&self) -> mcp_agent_mail_db::DbPool {
            self.pool.clone()
        }
    }

    #[derive(Clone)]
    enum ArchiveReadDecision {
        Live,
        Snapshot(Arc<SharedArchiveReadSnapshot>),
    }

    impl ArchiveReadDecision {
        fn immutable_snapshot(&self) -> Option<Arc<SharedArchiveReadSnapshot>> {
            match self {
                Self::Live => None,
                Self::Snapshot(snapshot) => Some(Arc::clone(snapshot)),
            }
        }
    }

    struct ReadSnapshotCacheEntry {
        key: ReadSnapshotKey,
        cheap_key: ReadSnapshotCheapKey,
        decision: ArchiveReadDecision,
        validated_at: Instant,
        strong_validated_at: Instant,
        invalidation_epoch: u64,
    }

    struct ReadSnapshotBuildOutput {
        key: ReadSnapshotKey,
        cheap_key: ReadSnapshotCheapKey,
        decision: ArchiveReadDecision,
        strong_validated_at: Instant,
    }

    #[derive(Debug)]
    struct ReadSnapshotBuildCompletion {
        result: Mutex<Option<Result<(), ReadSnapshotAcquireError>>>,
    }

    impl ReadSnapshotBuildCompletion {
        fn pending() -> Self {
            Self {
                result: Mutex::new(None),
            }
        }

        fn complete(&self, result: Result<(), ReadSnapshotAcquireError>) {
            let mut completion = self
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if completion.is_none() {
                *completion = Some(result);
            }
        }

        fn result(&self) -> Option<Result<(), ReadSnapshotAcquireError>> {
            self.result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[derive(Debug, Clone)]
    struct ReadSnapshotBuild {
        token: u64,
        invalidation_epoch: u64,
        db_write_epoch: u64,
        archive_application_epoch: u64,
        deadline: Instant,
        completion: Arc<ReadSnapshotBuildCompletion>,
    }

    #[derive(Default)]
    struct ReadSnapshotScopeState {
        ready: Option<ReadSnapshotCacheEntry>,
        building: Option<ReadSnapshotBuild>,
        next_token: u64,
        active_writers: usize,
    }

    struct ReadSnapshotScopeSlot {
        scope: ReadSnapshotScope,
        state: Mutex<ReadSnapshotScopeState>,
        invalidation_epoch: std::sync::atomic::AtomicU64,
        data_version_observer: Mutex<Option<mcp_agent_mail_db::DbConn>>,
        notify: asupersync::sync::Notify,
    }

    impl ReadSnapshotScopeSlot {
        fn new(scope: ReadSnapshotScope) -> Self {
            Self {
                scope,
                state: Mutex::new(ReadSnapshotScopeState::default()),
                invalidation_epoch: std::sync::atomic::AtomicU64::new(0),
                data_version_observer: Mutex::new(None),
                notify: asupersync::sync::Notify::new(),
            }
        }
    }

    #[derive(Default)]
    struct ReadSnapshotRegistry {
        slots: VecDeque<Arc<ReadSnapshotScopeSlot>>,
    }

    static READ_SNAPSHOT_REGISTRY: LazyLock<Mutex<ReadSnapshotRegistry>> =
        LazyLock::new(|| Mutex::new(ReadSnapshotRegistry::default()));
    static READ_SNAPSHOT_BLOCKING_POOL: LazyLock<asupersync::runtime::BlockingPool> =
        LazyLock::new(|| asupersync::runtime::BlockingPool::new(0, READ_SNAPSHOT_WORKER_LIMIT));
    const READ_SNAPSHOT_PENDING_BUILD_LIMIT: usize = READ_SNAPSHOT_SCOPE_CAPACITY;
    static READ_SNAPSHOT_PENDING_BUILDS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static READ_SNAPSHOT_DB_WRITES_ACTIVE: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static READ_SNAPSHOT_DB_WRITE_EPOCH: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    #[cfg(test)]
    pub(crate) static READ_SNAPSHOT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(test)]
    static READ_SNAPSHOT_RECONSTRUCTION_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    static READ_SNAPSHOT_RECONSTRUCTIONS_INFLIGHT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    static READ_SNAPSHOT_RECONSTRUCTIONS_MAX_INFLIGHT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    static READ_SNAPSHOT_GENERATION_SCAN_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    static READ_SNAPSHOT_GENERATION_SCANS_INFLIGHT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    static READ_SNAPSHOT_GENERATION_SCANS_MAX_INFLIGHT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    static READ_SNAPSHOT_TEST_DELAY_MILLIS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    #[cfg(test)]
    type ReadSnapshotPreReconstructionTestHook = Box<dyn FnOnce() + Send + 'static>;

    #[cfg(test)]
    static READ_SNAPSHOT_PRE_RECONSTRUCTION_TEST_HOOK: LazyLock<
        Mutex<Option<ReadSnapshotPreReconstructionTestHook>>,
    > = LazyLock::new(|| Mutex::new(None));

    #[cfg(test)]
    #[derive(Default)]
    struct ReadSnapshotPostValidationTestState {
        armed: bool,
        reached: bool,
        release: bool,
    }

    #[cfg(test)]
    static READ_SNAPSHOT_POST_VALIDATION_TEST_HOOK: LazyLock<(
        Mutex<ReadSnapshotPostValidationTestState>,
        std::sync::Condvar,
    )> = LazyLock::new(|| {
        (
            Mutex::new(ReadSnapshotPostValidationTestState::default()),
            std::sync::Condvar::new(),
        )
    });

    #[cfg(test)]
    fn read_snapshot_test_delay() {
        let millis = READ_SNAPSHOT_TEST_DELAY_MILLIS.load(std::sync::atomic::Ordering::Acquire);
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
        }
    }

    #[cfg(test)]
    pub(crate) fn set_read_snapshot_test_delay(delay: Duration) {
        READ_SNAPSHOT_TEST_DELAY_MILLIS.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Release,
        );
    }

    #[cfg(test)]
    fn set_read_snapshot_pre_reconstruction_test_hook(hook: impl FnOnce() + Send + 'static) {
        *READ_SNAPSHOT_PRE_RECONSTRUCTION_TEST_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn run_read_snapshot_pre_reconstruction_test_hook() {
        let hook = READ_SNAPSHOT_PRE_RECONSTRUCTION_TEST_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn reset_read_snapshot_pre_reconstruction_test_hook() {
        *READ_SNAPSHOT_PRE_RECONSTRUCTION_TEST_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    #[cfg(test)]
    fn arm_read_snapshot_post_validation_test_hook() {
        let (state, _) = &*READ_SNAPSHOT_POST_VALIDATION_TEST_HOOK;
        *state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            ReadSnapshotPostValidationTestState {
                armed: true,
                reached: false,
                release: false,
            };
    }

    #[cfg(test)]
    fn wait_for_read_snapshot_post_validation_test_hook(timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (state, notify) = &*READ_SNAPSHOT_POST_VALIDATION_TEST_HOOK;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.reached {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timed_out) = notify
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timed_out.timed_out() && !state.reached {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    fn release_read_snapshot_post_validation_test_hook() {
        let (state, notify) = &*READ_SNAPSHOT_POST_VALIDATION_TEST_HOOK;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.release = true;
        notify.notify_all();
    }

    #[cfg(test)]
    fn reset_read_snapshot_post_validation_test_hook() {
        let (state, notify) = &*READ_SNAPSHOT_POST_VALIDATION_TEST_HOOK;
        *state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            ReadSnapshotPostValidationTestState::default();
        notify.notify_all();
    }

    fn read_snapshot_test_pause_after_final_validation() {
        #[cfg(test)]
        {
            let (state, notify) = &*READ_SNAPSHOT_POST_VALIDATION_TEST_HOOK;
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.armed {
                return;
            }
            state.reached = true;
            notify.notify_all();
            while !state.release {
                state = notify
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            *state = ReadSnapshotPostValidationTestState::default();
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum ReadSnapshotAcquireError {
        Cancelled,
        Busy(String),
        TimedOut(String),
        Failed(String),
    }

    impl ReadSnapshotAcquireError {
        pub(crate) fn failed(error: impl std::fmt::Display) -> Self {
            Self::Failed(error.to_string())
        }
    }

    fn read_snapshot_duration_from_env(name: &str, default: Duration) -> Duration {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(default)
    }

    fn read_snapshot_validation_ttl() -> Duration {
        read_snapshot_duration_from_env(
            "MCP_AGENT_MAIL_READ_SNAPSHOT_VALIDATION_TTL_MS",
            READ_SNAPSHOT_VALIDATION_TTL_DEFAULT,
        )
    }

    fn read_snapshot_exact_audit_interval() -> Duration {
        read_snapshot_duration_from_env(
            "MCP_AGENT_MAIL_READ_SNAPSHOT_EXACT_AUDIT_INTERVAL_MS",
            READ_SNAPSHOT_EXACT_AUDIT_INTERVAL_DEFAULT,
        )
    }

    struct ReadSnapshotBuildPermit;

    impl ReadSnapshotBuildPermit {
        fn try_acquire() -> Option<Self> {
            let mut pending =
                READ_SNAPSHOT_PENDING_BUILDS.load(std::sync::atomic::Ordering::Acquire);
            loop {
                if pending >= READ_SNAPSHOT_PENDING_BUILD_LIMIT {
                    return None;
                }
                match READ_SNAPSHOT_PENDING_BUILDS.compare_exchange_weak(
                    pending,
                    pending + 1,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                ) {
                    Ok(_) => return Some(Self),
                    Err(observed) => pending = observed,
                }
            }
        }
    }

    impl Drop for ReadSnapshotBuildPermit {
        fn drop(&mut self) {
            READ_SNAPSHOT_PENDING_BUILDS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    struct ReadSnapshotBuildGuard {
        slot: Arc<ReadSnapshotScopeSlot>,
        token: u64,
        active: bool,
    }

    impl ReadSnapshotBuildGuard {
        fn new(slot: Arc<ReadSnapshotScopeSlot>, token: u64) -> Self {
            Self {
                slot,
                token,
                active: true,
            }
        }

        fn publish(
            &mut self,
            build: &ReadSnapshotBuild,
            result: Result<ReadSnapshotBuildOutput, ReadSnapshotAcquireError>,
        ) {
            let mut retired = None;
            mcp_agent_mail_storage::with_archive_publication_barrier(|| {
                let mut state = self
                    .slot
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state
                    .building
                    .as_ref()
                    .is_some_and(|build| build.token == self.token)
                {
                    let completion = match result {
                        Ok(output)
                            if self
                                .slot
                                .invalidation_epoch
                                .load(std::sync::atomic::Ordering::Acquire)
                                == build.invalidation_epoch
                                && state.active_writers == 0
                                && READ_SNAPSHOT_DB_WRITES_ACTIVE
                                    .load(std::sync::atomic::Ordering::Acquire)
                                    == 0
                                && READ_SNAPSHOT_DB_WRITE_EPOCH
                                    .load(std::sync::atomic::Ordering::Acquire)
                                    == build.db_write_epoch
                                && mcp_agent_mail_storage::archive_applications_active() == 0
                                && mcp_agent_mail_storage::archive_application_epoch()
                                    == build.archive_application_epoch =>
                        {
                            retired = state.ready.replace(ReadSnapshotCacheEntry {
                                key: output.key,
                                cheap_key: output.cheap_key,
                                decision: output.decision,
                                validated_at: Instant::now(),
                                strong_validated_at: output.strong_validated_at,
                                invalidation_epoch: build.invalidation_epoch,
                            });
                            Ok(())
                        }
                        Ok(_) => Err(ReadSnapshotAcquireError::Busy(
                            "archive read snapshot build was superseded by a durable write"
                                .to_string(),
                        )),
                        Err(error) => Err(error),
                    };
                    build.completion.complete(completion);
                    state.building = None;
                }
            });
            self.active = false;
            // A retired entry owns a pool and temporary directory. Drop it
            // outside the state mutex so cleanup cannot block admission.
            drop(retired);
        }
    }

    impl Drop for ReadSnapshotBuildGuard {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            let mut state = self
                .slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .building
                .as_ref()
                .is_some_and(|build| build.token == self.token)
            {
                let completion = Arc::clone(
                    &state
                        .building
                        .as_ref()
                        .expect("matching snapshot build must remain present")
                        .completion,
                );
                state.building = None;
                completion.complete(Err(ReadSnapshotAcquireError::Failed(
                    "archive read snapshot blocking worker terminated without publishing"
                        .to_string(),
                )));
            }
            drop(state);
            self.slot.notify.notify_waiters();
        }
    }

    #[cfg(test)]
    struct ReadSnapshotReconstructionGuard;

    #[cfg(test)]
    struct ReadSnapshotGenerationScanGuard;

    #[cfg(test)]
    impl ReadSnapshotReconstructionGuard {
        fn start() -> Self {
            use std::sync::atomic::Ordering;

            READ_SNAPSHOT_RECONSTRUCTION_COUNT.fetch_add(1, Ordering::Relaxed);
            let current = READ_SNAPSHOT_RECONSTRUCTIONS_INFLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
            READ_SNAPSHOT_RECONSTRUCTIONS_MAX_INFLIGHT.fetch_max(current, Ordering::SeqCst);
            Self
        }
    }

    #[cfg(test)]
    impl Drop for ReadSnapshotReconstructionGuard {
        fn drop(&mut self) {
            READ_SNAPSHOT_RECONSTRUCTIONS_INFLIGHT
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    impl ReadSnapshotGenerationScanGuard {
        fn start() -> Self {
            use std::sync::atomic::Ordering;

            READ_SNAPSHOT_GENERATION_SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
            let current =
                READ_SNAPSHOT_GENERATION_SCANS_INFLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
            READ_SNAPSHOT_GENERATION_SCANS_MAX_INFLIGHT.fetch_max(current, Ordering::SeqCst);
            Self
        }
    }

    #[cfg(test)]
    impl Drop for ReadSnapshotGenerationScanGuard {
        fn drop(&mut self) {
            READ_SNAPSHOT_GENERATION_SCANS_INFLIGHT
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn update_path_metadata_digest(
        hasher: &mut Sha256,
        path: &Path,
    ) -> Result<(), ReadSnapshotAcquireError> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                hasher.update([1]);
                hasher.update(metadata.len().to_le_bytes());
                let file_type = metadata.file_type();
                hasher.update([
                    u8::from(file_type.is_file()),
                    u8::from(file_type.is_dir()),
                    u8::from(file_type.is_symlink()),
                ]);
                let modified = metadata
                    .modified()
                    .unwrap_or(UNIX_EPOCH)
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                hasher.update(modified.as_secs().to_le_bytes());
                hasher.update(modified.subsec_nanos().to_le_bytes());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hasher.update([0]);
            }
            Err(error) => return Err(ReadSnapshotAcquireError::failed(error)),
        }
        Ok(())
    }

    fn update_path_shape_digest(
        hasher: &mut Sha256,
        path: &Path,
    ) -> Result<(), ReadSnapshotAcquireError> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                hasher.update([1]);
                hasher.update(metadata.len().to_le_bytes());
                let file_type = metadata.file_type();
                hasher.update([
                    u8::from(file_type.is_file()),
                    u8::from(file_type.is_dir()),
                    u8::from(file_type.is_symlink()),
                ]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hasher.update([0]);
            }
            Err(error) => return Err(ReadSnapshotAcquireError::failed(error)),
        }
        Ok(())
    }

    fn update_file_contents_digest(
        hasher: &mut Sha256,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), ReadSnapshotAcquireError> {
        if cancelled() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        let metadata = std::fs::symlink_metadata(path).map_err(ReadSnapshotAcquireError::failed)?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(path).map_err(ReadSnapshotAcquireError::failed)?;
            hasher.update(target.to_string_lossy().as_bytes());
            return Ok(());
        }
        if !metadata.is_file() {
            return Ok(());
        }

        let mut file = std::fs::File::open(path).map_err(ReadSnapshotAcquireError::failed)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if cancelled() {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            let read = file
                .read(&mut buffer)
                .map_err(ReadSnapshotAcquireError::failed)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(())
    }

    fn collect_archive_tree_paths(
        directory: &Path,
        paths: &mut Vec<PathBuf>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), ReadSnapshotAcquireError> {
        if cancelled() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ReadSnapshotAcquireError::failed(error)),
        };
        for entry in entries {
            if cancelled() {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            let entry = entry.map_err(ReadSnapshotAcquireError::failed)?;
            let path = entry.path();
            let metadata =
                std::fs::symlink_metadata(&path).map_err(ReadSnapshotAcquireError::failed)?;
            paths.push(path.clone());
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                collect_archive_tree_paths(&path, paths, cancelled)?;
            }
        }
        Ok(())
    }

    fn read_archive_generation(
        storage_root: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(PathBuf, [u8; 32]), ReadSnapshotAcquireError> {
        if cancelled() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        let canonical_root = storage_root
            .canonicalize()
            .map_err(ReadSnapshotAcquireError::failed)?;
        let mut hasher = Sha256::new();
        hasher.update(b"agent-mail-read-snapshot-archive-v1");
        hasher.update(canonical_root.to_string_lossy().as_bytes());

        if let Ok(repo) = git2::Repository::open(&canonical_root)
            && repo
                .workdir()
                .and_then(|workdir| workdir.canonicalize().ok())
                .as_ref()
                == Some(&canonical_root)
        {
            let head_before = read_archive_head(&repo);
            hasher.update(b"git");
            if let Some(head) = head_before {
                hasher.update(head.as_bytes());
            }

            let mut status_options = git2::StatusOptions::new();
            status_options
                .show(git2::StatusShow::IndexAndWorkdir)
                .include_untracked(true)
                .recurse_untracked_dirs(true)
                .include_ignored(true)
                .recurse_ignored_dirs(true)
                .include_unmodified(false)
                .exclude_submodules(false)
                .pathspec("projects");
            let statuses = repo
                .statuses(Some(&mut status_options))
                .map_err(ReadSnapshotAcquireError::failed)?;
            if cancelled() {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            let mut dirty_paths = Vec::new();
            for entry in statuses.iter() {
                let path = entry.path().map_err(|error| {
                    ReadSnapshotAcquireError::Failed(format!(
                        "archive Git status path is unavailable or non-UTF-8; refusing an inexact read-snapshot generation: {error}"
                    ))
                })?;
                dirty_paths.push((path.to_string(), entry.status().bits()));
            }
            dirty_paths.sort();
            let mut content_paths = BTreeSet::new();
            for (relative, status) in &dirty_paths {
                if cancelled() {
                    return Err(ReadSnapshotAcquireError::Cancelled);
                }
                hasher.update(relative.as_bytes());
                hasher.update(status.to_le_bytes());
                let path = canonical_root.join(relative);
                let newly_discovered = content_paths.insert(path.clone());
                if newly_discovered && path.exists() {
                    let metadata = std::fs::symlink_metadata(&path)
                        .map_err(ReadSnapshotAcquireError::failed)?;
                    if metadata.is_dir() && !metadata.file_type().is_symlink() {
                        let mut descendants = Vec::new();
                        collect_archive_tree_paths(&path, &mut descendants, cancelled)?;
                        content_paths.extend(descendants);
                    }
                }
            }
            for path in content_paths {
                if cancelled() {
                    return Err(ReadSnapshotAcquireError::Cancelled);
                }
                let relative = path.strip_prefix(&canonical_root).unwrap_or(path.as_path());
                hasher.update(relative.to_string_lossy().as_bytes());
                update_path_metadata_digest(&mut hasher, &path)?;
                if path.exists() {
                    update_file_contents_digest(&mut hasher, &path, cancelled)?;
                }
            }

            if read_archive_head(&repo) != head_before {
                return Err(ReadSnapshotAcquireError::Failed(
                    "archive HEAD changed while computing the read-snapshot generation".to_string(),
                ));
            }
            return Ok((canonical_root, hasher.finalize().into()));
        }

        hasher.update(b"tree");
        let projects = canonical_root.join("projects");
        let mut paths = Vec::new();
        collect_archive_tree_paths(&projects, &mut paths, cancelled)?;
        paths.sort();
        for path in paths {
            if cancelled() {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            let relative = path.strip_prefix(&canonical_root).unwrap_or(path.as_path());
            hasher.update(relative.to_string_lossy().as_bytes());
            update_path_metadata_digest(&mut hasher, &path)?;
            update_file_contents_digest(&mut hasher, &path, cancelled)?;
        }
        Ok((canonical_root, hasher.finalize().into()))
    }

    fn read_live_db_generation(
        sqlite_path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<[u8; 32], ReadSnapshotAcquireError> {
        let mut hasher = Sha256::new();
        hasher.update(b"agent-mail-read-snapshot-live-db-v3-content");
        // The main database plus rollback/WAL journals are durable content.
        // Do not hash `-shm`: read-lock bookkeeping changes there without a
        // durable mailbox write and would make generation validation livelock.
        for path in
            std::iter::once(sqlite_path.to_path_buf()).chain(["-journal", "-wal"].into_iter().map(
                |suffix| mcp_agent_mail_db::pool::sqlite_path_with_suffix(sqlite_path, suffix),
            ))
        {
            if cancelled() {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            hasher.update(path.to_string_lossy().as_bytes());
            // Read-only FrankenSQLite probes can advance the main file's mtime
            // without changing durable bytes. The exact generation is content
            // addressed, so include path shape but exclude incidental mtimes.
            update_path_shape_digest(&mut hasher, &path)?;
            if path.exists() {
                update_file_contents_digest(&mut hasher, &path, cancelled)?;
            }
        }
        Ok(hasher.finalize().into())
    }

    fn read_snapshot_scope(
        storage_root: &Path,
        sqlite_path: &Path,
    ) -> Result<ReadSnapshotScope, ReadSnapshotAcquireError> {
        let storage_root = storage_root
            .canonicalize()
            .map_err(ReadSnapshotAcquireError::failed)?;
        let sqlite_path = sqlite_path
            .canonicalize()
            .unwrap_or_else(|_| sqlite_path.to_path_buf());
        Ok(ReadSnapshotScope {
            storage_root,
            sqlite_path,
        })
    }

    fn read_snapshot_key(
        scope: &ReadSnapshotScope,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ReadSnapshotKey, ReadSnapshotAcquireError> {
        #[cfg(test)]
        let _generation_scan_guard = ReadSnapshotGenerationScanGuard::start();

        let (_, archive_digest) = read_archive_generation(&scope.storage_root, cancelled)?;
        if cancelled() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        let live_db_digest = read_live_db_generation(&scope.sqlite_path, cancelled)?;
        Ok(ReadSnapshotKey {
            archive_digest,
            live_db_digest,
        })
    }

    fn open_read_only_snapshot_probe(
        sqlite_path: &Path,
        phase: &str,
    ) -> Result<mcp_agent_mail_db::DbConn, String> {
        let conn =
            mcp_agent_mail_db::DbConn::open_file_read_only(sqlite_path.to_string_lossy().as_ref())
                .map_err(|error| {
                    format!(
                        "{phase}: open {} read-only without create: {error}",
                        sqlite_path.display()
                    )
                })?;
        conn.execute_raw("PRAGMA query_only = ON;")
            .map_err(|error| format!("{phase}: enforce query-only connection: {error}"))?;
        Ok(conn)
    }

    fn read_snapshot_live_data_version(slot: &ReadSnapshotScopeSlot) -> Option<i64> {
        let mut observer = slot
            .data_version_observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if observer.is_none() {
            let conn = open_read_only_snapshot_probe(
                &slot.scope.sqlite_path,
                "read-snapshot data_version observer",
            )
            .ok()?;
            *observer = Some(conn);
        }
        let value = observer
            .as_ref()
            .and_then(|conn| mcp_agent_mail_db::pool::sqlite_data_version(conn).ok());
        if value.is_none() {
            // Re-open on the next validation after an observer failure. The
            // metadata and exact-audit paths remain conservative meanwhile.
            *observer = None;
        }
        value
    }

    fn read_snapshot_cheap_key(
        slot: &ReadSnapshotScopeSlot,
    ) -> Result<ReadSnapshotCheapKey, ReadSnapshotAcquireError> {
        // Establish the long-lived observer before capturing file metadata:
        // opening a read-only FrankenSQLite connection may create sidecars or
        // advance the main file's mtime even though durable content is stable.
        let live_data_version = read_snapshot_live_data_version(slot);

        let mut archive = Sha256::new();
        archive.update(b"agent-mail-read-snapshot-archive-metadata-v2");
        archive.update(slot.scope.storage_root.to_string_lossy().as_bytes());
        // Pool setup can create and remove transient root-level bookkeeping;
        // only the projects tree and Git metadata define archive generations.
        update_path_shape_digest(&mut archive, &slot.scope.storage_root)?;
        update_path_metadata_digest(&mut archive, &slot.scope.storage_root.join("projects"))?;
        update_path_metadata_digest(&mut archive, &slot.scope.storage_root.join(".git/HEAD"))?;
        update_path_metadata_digest(&mut archive, &slot.scope.storage_root.join(".git/index"))?;
        if let Ok(repo) = git2::Repository::open(&slot.scope.storage_root)
            && let Some(head) = read_archive_head(&repo)
        {
            archive.update(head.as_bytes());
        }

        let mut live = Sha256::new();
        live.update(b"agent-mail-read-snapshot-live-db-metadata-v2");
        live.update(slot.scope.sqlite_path.to_string_lossy().as_bytes());
        update_path_shape_digest(&mut live, &slot.scope.sqlite_path)?;
        for path in ["-journal", "-wal"].into_iter().map(|suffix| {
            mcp_agent_mail_db::pool::sqlite_path_with_suffix(&slot.scope.sqlite_path, suffix)
        }) {
            live.update(path.to_string_lossy().as_bytes());
            update_path_metadata_digest(&mut live, &path)?;
        }

        Ok(ReadSnapshotCheapKey {
            archive_metadata_digest: archive.finalize().into(),
            live_db_metadata_digest: live.finalize().into(),
            live_data_version,
            db_write_epoch: READ_SNAPSHOT_DB_WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire),
            archive_application_epoch: mcp_agent_mail_storage::archive_application_epoch(),
        })
    }

    pub(crate) fn read_snapshot_cancelled(cx: Option<&asupersync::Cx>) -> bool {
        cx.is_some_and(asupersync::Cx::is_cancel_requested)
    }

    fn read_snapshot_key_with_budget(
        scope: &ReadSnapshotScope,
        cancelled: &dyn Fn() -> bool,
        cx: Option<&asupersync::Cx>,
        build_deadline: Instant,
    ) -> Result<ReadSnapshotKey, ReadSnapshotAcquireError> {
        let key = match read_snapshot_key(scope, cancelled) {
            Ok(key) => key,
            Err(_) if read_snapshot_cancelled(cx) => {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            Err(_) if Instant::now() >= build_deadline => {
                return Err(ReadSnapshotAcquireError::TimedOut(format!(
                    "archive read snapshot generation scan exceeded its {}s work budget",
                    READ_SNAPSHOT_BUILD_TIMEOUT.as_secs()
                )));
            }
            Err(error) => return Err(error),
        };
        if read_snapshot_cancelled(cx) {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        if Instant::now() >= build_deadline {
            return Err(ReadSnapshotAcquireError::TimedOut(format!(
                "archive read snapshot generation scan exceeded its {}s work budget",
                READ_SNAPSHOT_BUILD_TIMEOUT.as_secs()
            )));
        }
        Ok(key)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ReadSnapshotMode {
        Canonical,
        ForceSnapshot,
    }

    #[derive(Clone)]
    struct ReadSnapshotBuildRequest {
        database_url: String,
        mode: ReadSnapshotMode,
    }

    fn read_snapshot_slot(
        scope: ReadSnapshotScope,
    ) -> Result<Arc<ReadSnapshotScopeSlot>, ReadSnapshotAcquireError> {
        let mut registry = READ_SNAPSHOT_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = registry.slots.iter().position(|slot| slot.scope == scope) {
            let slot = registry
                .slots
                .remove(index)
                .expect("read snapshot slot index must remain valid");
            registry.slots.push_back(Arc::clone(&slot));
            return Ok(slot);
        }

        let retired = if registry.slots.len() >= READ_SNAPSHOT_SCOPE_CAPACITY {
            let Some(index) = registry.slots.iter().position(|candidate| {
                Arc::strong_count(candidate) == 1 && {
                    let state = candidate
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.building.is_none() && state.active_writers == 0
                }
            }) else {
                return Err(ReadSnapshotAcquireError::Busy(format!(
                    "archive read snapshot scope registry is at its hard capacity of {READ_SNAPSHOT_SCOPE_CAPACITY} active mailboxes"
                )));
            };
            registry.slots.remove(index)
        } else {
            None
        };
        let slot = Arc::new(ReadSnapshotScopeSlot::new(scope));
        registry.slots.push_back(Arc::clone(&slot));
        drop(registry);
        // A retired entry can own a pool and temporary directory. Never run
        // that cleanup while holding the global registry lock.
        drop(retired);
        Ok(slot)
    }

    fn begin_read_snapshot_write(
        storage_root: &Path,
        sqlite_path: &Path,
    ) -> Option<Arc<ReadSnapshotScopeSlot>> {
        READ_SNAPSHOT_DB_WRITES_ACTIVE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        READ_SNAPSHOT_DB_WRITE_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        let slot = read_snapshot_scope(storage_root, sqlite_path)
            .ok()
            .and_then(|scope| read_snapshot_slot(scope).ok());
        if let Some(slot) = &slot {
            let mut state = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active_writers = state.active_writers.saturating_add(1);
            slot.invalidation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            drop(state);
            slot.notify.notify_waiters();
        }
        slot
    }

    fn end_read_snapshot_write(slot: Option<&ReadSnapshotScopeSlot>) {
        if let Some(slot) = slot {
            let mut state = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active_writers = state.active_writers.saturating_sub(1);
            slot.invalidation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            drop(state);
        }
        READ_SNAPSHOT_DB_WRITE_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        READ_SNAPSHOT_DB_WRITES_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        if let Some(slot) = slot {
            slot.notify.notify_waiters();
        }
    }

    fn invalidate_read_snapshot_slot(slot: &ReadSnapshotScopeSlot) {
        slot.invalidation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        slot.notify.notify_waiters();
    }

    pub(crate) fn invalidate_read_snapshots_for_archive(storage_root: &Path) {
        let canonical = storage_root
            .canonicalize()
            .unwrap_or_else(|_| storage_root.to_path_buf());
        let slots = READ_SNAPSHOT_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .slots
            .iter()
            .filter(|slot| slot.scope.storage_root == canonical)
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            invalidate_read_snapshot_slot(&slot);
        }
    }

    fn build_read_snapshot(
        scope: &ReadSnapshotScope,
        inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Arc<SharedArchiveReadSnapshot>, ReadSnapshotAcquireError> {
        #[cfg(test)]
        let _reconstruction_guard = ReadSnapshotReconstructionGuard::start();

        let snapshot_dir =
            mcp_agent_mail_db::pool::CanonicalSnapshotTempDir::new("agent-mail-read-snapshot-")
                .map_err(ReadSnapshotAcquireError::failed)?;
        let snapshot_db = snapshot_dir.path().join("mailbox.sqlite3");
        let stats = if scope.sqlite_path.exists() {
            mcp_agent_mail_db::reconstruct_from_archive_with_salvage_cancellable(
                &snapshot_db,
                &scope.storage_root,
                Some(scope.sqlite_path.as_path()),
                cancelled,
            )
            .map_err(ReadSnapshotAcquireError::failed)?
        } else {
            mcp_agent_mail_db::reconstruct_from_archive_cancellable(
                &snapshot_db,
                &scope.storage_root,
                cancelled,
            )
            .map_err(ReadSnapshotAcquireError::failed)?
        };
        if cancelled() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        validate_read_snapshot_completeness(inventory, &stats)?;
        let pool = mcp_agent_mail_db::create_query_only_pool(&mcp_agent_mail_db::DbPoolConfig {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&snapshot_db),
            storage_root: Some(scope.storage_root.clone()),
            ..Default::default()
        })
        .map_err(ReadSnapshotAcquireError::failed)?;
        Ok(Arc::new(SharedArchiveReadSnapshot {
            pool,
            _snapshot_dir: snapshot_dir,
        }))
    }

    fn validate_read_snapshot_inventory(
        inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
    ) -> Result<(), ReadSnapshotAcquireError> {
        if inventory.parse_errors > 0 {
            return Err(ReadSnapshotAcquireError::Failed(format!(
                "archive read snapshot inventory found {} canonical message file(s) that could not be parsed; refusing an incomplete read decision",
                inventory.parse_errors
            )));
        }
        Ok(())
    }

    fn validate_read_snapshot_completeness(
        inventory: &mcp_agent_mail_db::ArchiveMessageInventory,
        stats: &mcp_agent_mail_db::ReconstructStats,
    ) -> Result<(), ReadSnapshotAcquireError> {
        validate_read_snapshot_inventory(inventory)?;
        if stats.parse_errors > 0 {
            return Err(ReadSnapshotAcquireError::Failed(format!(
                "archive read snapshot reconstruction skipped {} malformed or unreadable archive artifact(s); refusing partial publication",
                stats.parse_errors
            )));
        }

        let reconstructed_message_files = stats
            .messages
            .checked_add(stats.duplicate_canonical_message_files)
            .ok_or_else(|| {
                ReadSnapshotAcquireError::Failed(
                    "archive read snapshot message completeness count overflowed".to_string(),
                )
            })?;
        if stats.projects != inventory.projects
            || stats.agents != inventory.agents
            || reconstructed_message_files != inventory.canonical_message_files
        {
            return Err(ReadSnapshotAcquireError::Failed(format!(
                "archive read snapshot reconstruction was incomplete: inventory projects={} agents={} canonical_message_files={}; reconstructed projects={} agents={} messages={} duplicate_canonical_message_files={} (salvaged projects={} agents={} messages={})",
                inventory.projects,
                inventory.agents,
                inventory.canonical_message_files,
                stats.projects,
                stats.agents,
                stats.messages,
                stats.duplicate_canonical_message_files,
                stats.salvaged_projects,
                stats.salvaged_agents,
                stats.salvaged_messages,
            )));
        }
        Ok(())
    }

    fn read_snapshot_stop_error(
        slot: &ReadSnapshotScopeSlot,
        build: &ReadSnapshotBuild,
        cx: &asupersync::Cx,
        deadline: Instant,
        phase: &str,
    ) -> Option<ReadSnapshotAcquireError> {
        if cx.is_cancel_requested() {
            return Some(ReadSnapshotAcquireError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Some(ReadSnapshotAcquireError::TimedOut(format!(
                "archive read snapshot {phase} exceeded its {}s cold-bootstrap budget",
                READ_SNAPSHOT_BUILD_TIMEOUT.as_secs()
            )));
        }
        if slot
            .invalidation_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            != build.invalidation_epoch
            || slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active_writers
                > 0
            || READ_SNAPSHOT_DB_WRITES_ACTIVE.load(std::sync::atomic::Ordering::Acquire) > 0
            || READ_SNAPSHOT_DB_WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire)
                != build.db_write_epoch
            || mcp_agent_mail_storage::archive_applications_active() > 0
            || mcp_agent_mail_storage::archive_application_epoch()
                != build.archive_application_epoch
        {
            return Some(ReadSnapshotAcquireError::Busy(
                "archive read snapshot build was superseded by a durable write".to_string(),
            ));
        }
        None
    }

    fn normalize_read_snapshot_build_error(
        error: ReadSnapshotAcquireError,
        slot: &ReadSnapshotScopeSlot,
        build: &ReadSnapshotBuild,
        cx: &asupersync::Cx,
        deadline: Instant,
        phase: &str,
    ) -> ReadSnapshotAcquireError {
        read_snapshot_stop_error(slot, build, cx, deadline, phase).unwrap_or(error)
    }

    fn run_read_snapshot_build(
        slot: &ReadSnapshotScopeSlot,
        request: &ReadSnapshotBuildRequest,
        build: &ReadSnapshotBuild,
        cx: &asupersync::Cx,
    ) -> Result<ReadSnapshotBuildOutput, ReadSnapshotAcquireError> {
        #[cfg(test)]
        read_snapshot_test_delay();

        let cancelled = || {
            read_snapshot_stop_error(
                slot,
                build,
                cx,
                build.deadline,
                "generation or reconstruction",
            )
            .is_some()
        };
        let mut archive_inventory = None;
        for _ in 0..READ_SNAPSHOT_GENERATION_RETRIES {
            if let Some(error) =
                read_snapshot_stop_error(slot, build, cx, build.deadline, "generation")
            {
                return Err(error);
            }
            let cheap_key = read_snapshot_cheap_key(slot)?;
            let cheap_cached = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ready
                .as_ref()
                .filter(|entry| {
                    entry.invalidation_epoch == build.invalidation_epoch
                        && entry.cheap_key == cheap_key
                        && Instant::now().duration_since(entry.strong_validated_at)
                            < read_snapshot_exact_audit_interval()
                })
                .map(|entry| {
                    (
                        entry.key.clone(),
                        entry.decision.clone(),
                        entry.strong_validated_at,
                    )
                });
            if let Some((key, decision, strong_validated_at)) = cheap_cached {
                return Ok(ReadSnapshotBuildOutput {
                    key,
                    cheap_key,
                    decision,
                    strong_validated_at,
                });
            }

            let key =
                read_snapshot_key_with_budget(&slot.scope, &cancelled, Some(cx), build.deadline)
                    .map_err(|error| {
                        normalize_read_snapshot_build_error(
                            error,
                            slot,
                            &build,
                            cx,
                            build.deadline,
                            "generation",
                        )
                    })?;

            let cached_decision = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ready
                .as_ref()
                .filter(|entry| {
                    entry.invalidation_epoch == build.invalidation_epoch && entry.key == key
                })
                .map(|entry| entry.decision.clone());
            if let Some(decision) = cached_decision {
                let cheap_after = read_snapshot_cheap_key(slot)?;
                if cheap_after == cheap_key {
                    read_snapshot_test_pause_after_final_validation();
                    return Ok(ReadSnapshotBuildOutput {
                        key,
                        cheap_key: cheap_after,
                        decision,
                        strong_validated_at: Instant::now(),
                    });
                }
                continue;
            }

            if archive_inventory
                .as_ref()
                .is_none_or(|(digest, _)| *digest != key.archive_digest)
            {
                archive_inventory = Some((
                    key.archive_digest,
                    read_archive_inventory_cancellable(&slot.scope.storage_root, &cancelled)
                        .map_err(|error| {
                            normalize_read_snapshot_build_error(
                                error,
                                slot,
                                &build,
                                cx,
                                build.deadline,
                                "archive inventory",
                            )
                        })?,
                ));
            }
            let inventory = &archive_inventory
                .as_ref()
                .expect("archive inventory generation must be initialized")
                .1;
            validate_read_snapshot_inventory(inventory)?;
            let snapshot_required = if request.mode == ReadSnapshotMode::ForceSnapshot {
                true
            } else {
                let archive_has_state = read_archive_inventory_has_state(inventory);
                match open_read_only_snapshot_probe(
                    &slot.scope.sqlite_path,
                    "read-snapshot archive-ahead probe",
                ) {
                    Ok(conn) => {
                        if archive_has_state
                            && live_db_is_suspect(
                                &request.database_url,
                                &slot.scope.storage_root,
                                &slot.scope.sqlite_path,
                            )
                        {
                            true
                        } else {
                            let conn = mcp_agent_mail_db::guard_db_conn(
                                conn,
                                "tool_util::run_read_snapshot_build archive-ahead probe",
                            );
                            let ahead = read_archive_is_ahead(
                                &slot.scope.storage_root,
                                &slot.scope.sqlite_path,
                                &conn,
                                inventory,
                            );
                            drop(conn);
                            match ahead {
                                Ok(ahead) => ahead,
                                Err(error) if archive_has_state => {
                                    tracing::warn!(
                                        source = %slot.scope.sqlite_path.display(),
                                        storage_root = %slot.scope.storage_root.display(),
                                        error = %error,
                                        "using archive snapshot because the live sqlite inventory probe failed"
                                    );
                                    true
                                }
                                Err(_) => false,
                            }
                        }
                    }
                    Err(error) if archive_has_state => {
                        tracing::warn!(
                            source = %slot.scope.sqlite_path.display(),
                            storage_root = %slot.scope.storage_root.display(),
                            error = %error,
                            "using archive snapshot because the live sqlite source could not be opened"
                        );
                        true
                    }
                    Err(_) => false,
                }
            };

            let decision = if snapshot_required {
                #[cfg(test)]
                run_read_snapshot_pre_reconstruction_test_hook();
                ArchiveReadDecision::Snapshot(
                    build_read_snapshot(&slot.scope, inventory, &cancelled).map_err(|error| {
                        normalize_read_snapshot_build_error(
                            error,
                            slot,
                            &build,
                            cx,
                            build.deadline,
                            "reconstruction",
                        )
                    })?,
                )
            } else {
                ArchiveReadDecision::Live
            };

            let after =
                read_snapshot_key_with_budget(&slot.scope, &cancelled, Some(cx), build.deadline)
                    .map_err(|error| {
                        normalize_read_snapshot_build_error(
                            error,
                            slot,
                            &build,
                            cx,
                            build.deadline,
                            "post-build validation",
                        )
                    })?;
            let cheap_after = read_snapshot_cheap_key(slot)?;
            if after == key && cheap_after == cheap_key {
                read_snapshot_test_pause_after_final_validation();
                return Ok(ReadSnapshotBuildOutput {
                    key,
                    cheap_key: cheap_after,
                    decision,
                    strong_validated_at: Instant::now(),
                });
            }
        }

        Err(ReadSnapshotAcquireError::Busy(
            "archive or live SQLite content changed repeatedly during read-snapshot reconstruction"
                .to_string(),
        ))
    }

    fn try_claim_read_snapshot_build(
        slot: &ReadSnapshotScopeSlot,
        deadline: Instant,
    ) -> Result<Option<ReadSnapshotBuild>, ReadSnapshotAcquireError> {
        let mut state = slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active_writers > 0
            || READ_SNAPSHOT_DB_WRITES_ACTIVE.load(std::sync::atomic::Ordering::Acquire) > 0
            || mcp_agent_mail_storage::archive_applications_active() > 0
        {
            return Err(ReadSnapshotAcquireError::Busy(
                "archive read snapshot build is blocked while a durable writer is active"
                    .to_string(),
            ));
        }
        if state.building.is_some() {
            return Ok(None);
        }
        state.next_token = state.next_token.wrapping_add(1).max(1);
        let build = ReadSnapshotBuild {
            token: state.next_token,
            invalidation_epoch: slot
                .invalidation_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            db_write_epoch: READ_SNAPSHOT_DB_WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire),
            archive_application_epoch: mcp_agent_mail_storage::archive_application_epoch(),
            deadline,
            completion: Arc::new(ReadSnapshotBuildCompletion::pending()),
        };
        state.building = Some(build.clone());
        Ok(Some(build))
    }

    fn fail_scheduled_read_snapshot_build(
        slot: &ReadSnapshotScopeSlot,
        build: &ReadSnapshotBuild,
        error: ReadSnapshotAcquireError,
    ) {
        let mut state = slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .building
            .as_ref()
            .is_some_and(|active| active.token == build.token)
        {
            state.building = None;
            build.completion.complete(Err(error));
        }
        drop(state);
        slot.notify.notify_waiters();
    }

    fn schedule_read_snapshot_build(
        slot: Arc<ReadSnapshotScopeSlot>,
        request: ReadSnapshotBuildRequest,
        build: &ReadSnapshotBuild,
        cx: asupersync::Cx,
    ) -> Result<(), ReadSnapshotAcquireError> {
        let Some(pending_permit) = ReadSnapshotBuildPermit::try_acquire() else {
            let error = ReadSnapshotAcquireError::Busy(format!(
                "archive read snapshot build queue is at its hard pending limit of {READ_SNAPSHOT_PENDING_BUILD_LIMIT}"
            ));
            fail_scheduled_read_snapshot_build(&slot, build, error.clone());
            return Err(error);
        };
        let worker_slot = Arc::clone(&slot);
        let worker_build = build.clone();
        let handle = READ_SNAPSHOT_BLOCKING_POOL.spawn(move || {
            let mut guard =
                ReadSnapshotBuildGuard::new(Arc::clone(&worker_slot), worker_build.token);
            let result = run_read_snapshot_build(&worker_slot, &request, &worker_build, &cx);
            guard.publish(&worker_build, result);
            drop(pending_permit);
            worker_slot.notify.notify_waiters();
        });
        if handle.is_cancelled() {
            let error = ReadSnapshotAcquireError::Busy(
                "archive read snapshot blocking pool is unavailable".to_string(),
            );
            fail_scheduled_read_snapshot_build(&slot, build, error.clone());
            return Err(error);
        }
        drop(handle);
        Ok(())
    }

    async fn wait_for_read_snapshot_build(
        slot: &ReadSnapshotScopeSlot,
        build: ReadSnapshotBuild,
        cx: &asupersync::Cx,
        caller_deadline: Instant,
    ) -> Result<(), ReadSnapshotAcquireError> {
        loop {
            if cx.is_cancel_requested() {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            if let Some(completion) = build.completion.result() {
                return match completion {
                    Ok(()) => Ok(()),
                    Err(ReadSnapshotAcquireError::Cancelled) => Ok(()),
                    Err(ReadSnapshotAcquireError::Busy(message))
                        if message.contains("superseded by a durable write") =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
            }

            let now = Instant::now();
            let deadline = build.deadline.min(caller_deadline);
            if now >= deadline {
                return Err(ReadSnapshotAcquireError::TimedOut(format!(
                    "archive read snapshot cold bootstrap exceeded its {}s budget",
                    READ_SNAPSHOT_BUILD_TIMEOUT.as_secs()
                )));
            }
            let wait_for = READ_SNAPSHOT_WAIT_SLICE.min(deadline - now);
            let notified = Box::pin(slot.notify.notified());
            let _ =
                asupersync::time::timeout(asupersync::time::wall_now(), wait_for, notified).await;
        }
    }

    async fn get_archive_read_decision(
        storage_root: &Path,
        sqlite_path: &Path,
        database_url: &str,
        cx: &asupersync::Cx,
        mode: ReadSnapshotMode,
    ) -> Result<ArchiveReadDecision, ReadSnapshotAcquireError> {
        if cx.is_cancel_requested() {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        let slot = read_snapshot_slot(read_snapshot_scope(storage_root, sqlite_path)?)?;
        let request = ReadSnapshotBuildRequest {
            database_url: database_url.to_string(),
            mode,
        };
        let cold_deadline = Instant::now() + READ_SNAPSHOT_BUILD_TIMEOUT;

        loop {
            if cx.is_cancel_requested() {
                return Err(ReadSnapshotAcquireError::Cancelled);
            }
            let epoch = slot
                .invalidation_epoch
                .load(std::sync::atomic::Ordering::Acquire);
            let (ready, active_build) = {
                let state = slot
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let ready = state.ready.as_ref().and_then(|entry| {
                    (entry.invalidation_epoch == epoch
                        && state.active_writers == 0
                        && READ_SNAPSHOT_DB_WRITES_ACTIVE
                            .load(std::sync::atomic::Ordering::Acquire)
                            == 0
                        && entry.cheap_key.db_write_epoch
                            == READ_SNAPSHOT_DB_WRITE_EPOCH
                                .load(std::sync::atomic::Ordering::Acquire)
                        && mcp_agent_mail_storage::archive_applications_active() == 0
                        && entry.cheap_key.archive_application_epoch
                            == mcp_agent_mail_storage::archive_application_epoch())
                    .then(|| {
                        (
                            entry.decision.clone(),
                            Instant::now().duration_since(entry.validated_at)
                                < read_snapshot_validation_ttl(),
                        )
                    })
                });
                (ready, state.building.clone())
            };

            if let Some((decision, true)) = ready {
                return Ok(decision);
            }
            if let Some((decision, false)) = ready {
                if active_build.is_none() {
                    match try_claim_read_snapshot_build(&slot, cold_deadline) {
                        Ok(Some(build)) => {
                            if let Err(error) = schedule_read_snapshot_build(
                                Arc::clone(&slot),
                                request.clone(),
                                &build,
                                cx.clone(),
                            ) {
                                tracing::warn!(error = ?error, "snapshot stale-while-revalidate scheduling failed");
                            }
                        }
                        Ok(None) => {}
                        Err(error) => tracing::debug!(
                            error = ?error,
                            "snapshot stale-while-revalidate deferred while admission is busy"
                        ),
                    }
                }
                return Ok(decision);
            }

            let build = match active_build {
                Some(build) => build,
                None => {
                    let Some(build) = try_claim_read_snapshot_build(&slot, cold_deadline)? else {
                        continue;
                    };
                    schedule_read_snapshot_build(
                        Arc::clone(&slot),
                        request.clone(),
                        &build,
                        cx.clone(),
                    )?;
                    build
                }
            };
            wait_for_read_snapshot_build(&slot, build, cx, cold_deadline).await?;
        }
    }

    pub(crate) async fn get_archive_read_snapshot_if(
        storage_root: &Path,
        sqlite_path: &Path,
        database_url: &str,
        cx: &asupersync::Cx,
    ) -> Result<Option<Arc<SharedArchiveReadSnapshot>>, ReadSnapshotAcquireError> {
        get_archive_read_decision(
            storage_root,
            sqlite_path,
            database_url,
            cx,
            ReadSnapshotMode::Canonical,
        )
        .await
        .map(|decision| decision.immutable_snapshot())
    }

    #[cfg(test)]
    pub(crate) async fn get_or_build_archive_read_snapshot(
        storage_root: &Path,
        sqlite_path: &Path,
        cx: &asupersync::Cx,
    ) -> Result<Arc<SharedArchiveReadSnapshot>, ReadSnapshotAcquireError> {
        get_archive_read_decision(
            storage_root,
            sqlite_path,
            &mcp_agent_mail_core::disk::sqlite_url_from_path(sqlite_path),
            cx,
            ReadSnapshotMode::ForceSnapshot,
        )
        .await?
        .immutable_snapshot()
        .ok_or_else(|| {
            ReadSnapshotAcquireError::Failed(
                "archive snapshot was unexpectedly bypassed by an unconditional build".to_string(),
            )
        })
    }

    pub(crate) fn read_snapshot_acquire_error_to_mcp_error(
        error: ReadSnapshotAcquireError,
    ) -> McpError {
        match error {
            ReadSnapshotAcquireError::Cancelled => McpError::request_cancelled(),
            ReadSnapshotAcquireError::Busy(message) => {
                db_error_to_mcp_error(DbError::ResourceBusy(message))
            }
            ReadSnapshotAcquireError::TimedOut(message) => legacy_tool_error(
                "SNAPSHOT_TIMEOUT",
                message,
                true,
                json!({"timeout_seconds": READ_SNAPSHOT_BUILD_TIMEOUT.as_secs()}),
            ),
            ReadSnapshotAcquireError::Failed(message) => {
                read_pool_setup_error_to_mcp_error(message)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn reset_read_snapshot_cache() {
        use std::sync::atomic::Ordering;

        let mut registry = READ_SNAPSHOT_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            registry.slots.iter().all(|slot| slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .building
                .is_none()),
            "cannot reset the read snapshot cache while a reconstruction is active"
        );
        assert_eq!(
            READ_SNAPSHOT_DB_WRITES_ACTIVE.load(Ordering::SeqCst),
            0,
            "cannot reset the read snapshot cache while a write lease is active"
        );
        registry.slots.clear();
        drop(registry);
        READ_SNAPSHOT_RECONSTRUCTION_COUNT.store(0, Ordering::SeqCst);
        READ_SNAPSHOT_RECONSTRUCTIONS_INFLIGHT.store(0, Ordering::SeqCst);
        READ_SNAPSHOT_RECONSTRUCTIONS_MAX_INFLIGHT.store(0, Ordering::SeqCst);
        READ_SNAPSHOT_GENERATION_SCAN_COUNT.store(0, Ordering::SeqCst);
        READ_SNAPSHOT_GENERATION_SCANS_INFLIGHT.store(0, Ordering::SeqCst);
        READ_SNAPSHOT_GENERATION_SCANS_MAX_INFLIGHT.store(0, Ordering::SeqCst);
        READ_SNAPSHOT_TEST_DELAY_MILLIS.store(0, Ordering::Release);
        reset_read_snapshot_pre_reconstruction_test_hook();
        reset_read_snapshot_post_validation_test_hook();
    }

    #[cfg(test)]
    pub(crate) fn read_snapshot_cache_stats() -> (usize, bool, usize, usize, usize) {
        use std::sync::atomic::Ordering;

        let registry = READ_SNAPSHOT_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ready = 0;
        let mut building = false;
        for slot in &registry.slots {
            let state = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ready += usize::from(state.ready.is_some());
            building |= state.building.is_some();
        }
        (
            ready,
            building,
            READ_SNAPSHOT_RECONSTRUCTION_COUNT.load(Ordering::SeqCst),
            READ_SNAPSHOT_RECONSTRUCTIONS_INFLIGHT.load(Ordering::SeqCst),
            READ_SNAPSHOT_RECONSTRUCTIONS_MAX_INFLIGHT.load(Ordering::SeqCst),
        )
    }

    #[cfg(test)]
    pub(crate) fn read_snapshot_generation_scan_stats() -> (usize, usize, usize) {
        use std::sync::atomic::Ordering;

        (
            READ_SNAPSHOT_GENERATION_SCAN_COUNT.load(Ordering::SeqCst),
            READ_SNAPSHOT_GENERATION_SCANS_INFLIGHT.load(Ordering::SeqCst),
            READ_SNAPSHOT_GENERATION_SCANS_MAX_INFLIGHT.load(Ordering::SeqCst),
        )
    }

    #[cfg(test)]
    fn expire_read_snapshot_validation() {
        let registry = READ_SNAPSHOT_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired_at = Instant::now() - read_snapshot_validation_ttl();
        for slot in &registry.slots {
            if let Some(entry) = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ready
                .as_mut()
            {
                entry.validated_at = expired_at;
            }
        }
    }

    #[cfg(test)]
    fn expire_read_snapshot_exact_audit() {
        let registry = READ_SNAPSHOT_REGISTRY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired_at = Instant::now() - read_snapshot_exact_audit_interval();
        for slot in &registry.slots {
            if let Some(entry) = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ready
                .as_mut()
            {
                entry.validated_at = Instant::now() - read_snapshot_validation_ttl();
                entry.strong_validated_at = expired_at;
            }
        }
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

    fn read_archive_inventory_has_state(
        archive: &mcp_agent_mail_db::ArchiveMessageInventory,
    ) -> bool {
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
        archive: &mcp_agent_mail_db::ArchiveMessageInventory,
    ) -> Result<bool, String> {
        if !archive_storage_root_is_authoritative_for_sqlite_path(storage_root, sqlite_path) {
            return Ok(false);
        }

        if archive.projects == 0 && archive.agents == 0 && archive.unique_message_ids == 0 {
            return Ok(false);
        }

        let db_inventory = query_read_db_inventory(conn)?;
        let archive_message_count = archive.unique_message_ids;
        let archive_max_id = archive.latest_message_id.unwrap_or(0);
        let missing_archive_projects = mcp_agent_mail_db::archive_missing_project_identities(
            archive,
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

    pub struct ToolReadPool {
        pool: mcp_agent_mail_db::DbPool,
        _snapshot: Option<Arc<SharedArchiveReadSnapshot>>,
    }

    impl ToolReadPool {
        const fn live(pool: mcp_agent_mail_db::DbPool) -> Self {
            Self {
                pool,
                _snapshot: None,
            }
        }

        fn snapshot(snapshot: Arc<SharedArchiveReadSnapshot>) -> Self {
            Self {
                pool: snapshot.pool(),
                _snapshot: Some(snapshot),
            }
        }
    }

    impl std::ops::Deref for ToolReadPool {
        type Target = mcp_agent_mail_db::DbPool;

        fn deref(&self) -> &Self::Target {
            &self.pool
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

    async fn open_read_db_pool_with_cx(
        cx: &asupersync::Cx,
    ) -> Result<Option<ToolReadPool>, ReadSnapshotAcquireError> {
        if read_snapshot_cancelled(Some(cx)) {
            return Err(ReadSnapshotAcquireError::Cancelled);
        }
        let config = Config::from_env();
        if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url) {
            return Ok(None);
        }

        let sqlite_path =
            mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(&config.database_url)
                .map_err(ReadSnapshotAcquireError::failed)?
                .canonical_path;
        if sqlite_path == ":memory:" {
            return Ok(None);
        }

        let resolved_path = PathBuf::from(&sqlite_path);
        if !archive_storage_root_is_authoritative_for_sqlite_path(
            &config.storage_root,
            &resolved_path,
        ) {
            return Ok(None);
        }

        get_archive_read_snapshot_if(
            &config.storage_root,
            &resolved_path,
            &config.database_url,
            cx,
        )
        .await
        .map(|snapshot| snapshot.map(ToolReadPool::snapshot))
    }

    #[cfg(test)]
    async fn open_read_db_pool() -> Result<Option<ToolReadPool>, ReadSnapshotAcquireError> {
        open_read_db_pool_with_cx(&asupersync::Cx::for_testing()).await
    }

    #[cfg(test)]
    pub(crate) async fn open_tool_read_snapshot_pointer(
        cx: &asupersync::Cx,
    ) -> Result<Option<usize>, ReadSnapshotAcquireError> {
        open_read_db_pool_with_cx(cx).await.map(|pool| {
            pool.and_then(|pool| {
                pool._snapshot
                    .as_ref()
                    .map(|snapshot| Arc::as_ptr(snapshot) as usize)
            })
        })
    }

    pub async fn get_read_db_pool(cx: &asupersync::Cx) -> McpResult<ToolReadPool> {
        match open_read_db_pool_with_cx(cx).await {
            Ok(Some(pool)) => Ok(pool),
            Ok(None) => get_live_db_pool().map(ToolReadPool::live),
            Err(error) => Err(read_snapshot_acquire_error_to_mcp_error(error)),
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

        // Project lookup caching is owned by the query layer so the cache key is
        // scoped to this pool's SQLite identity.  A process can serve live and
        // reconstructed snapshot databases with the same project slug.
        let is_absolute = std::path::Path::new(raw_identifier).is_absolute();
        let out = if is_absolute {
            mcp_agent_mail_db::queries::ensure_project(ctx.cx(), pool, raw_identifier).await
        } else {
            mcp_agent_mail_db::queries::get_project_by_slug(ctx.cx(), pool, raw_identifier).await
        };

        match db_outcome_to_mcp_result(out) {
            Ok(project) => Ok(project),
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

        // Delegate to queries::get_agent, which is itself cache-first against
        // the *pool-scoped* cache. The previous code path also consulted the
        // unscoped (`scope = ""`) cache here as a pre-check, but the unscoped
        // entries are populated by `register_agent` / `create_agent_identity`
        // against the live write pool, while `fetch_inbox` (and similar reads)
        // run against an archive-aware read pool whose `sqlite_identity_key`
        // can differ. That split-brain caused agent IDs from the live pool to
        // be served for archive-pool reads, returning rows for the wrong
        // recipient (mcp_agent_mail_rust#106). Always go through the scoped
        // path so the cache key matches the pool the SQL will actually run
        // against.
        let out = mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, name).await;

        match db_outcome_to_mcp_result(out) {
            Ok(agent) => Ok(agent),
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
        use std::sync::Mutex;

        fn run_async<F: std::future::Future>(future: F) -> F::Output {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("build test runtime");
            runtime.block_on(future)
        }

        fn wait_for_snapshot_pointer_change(
            storage_root: &Path,
            sqlite_path: &Path,
            old_pointer: usize,
        ) -> Arc<SharedArchiveReadSnapshot> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let cx = asupersync::Cx::for_testing();
                let snapshot = run_async(get_or_build_archive_read_snapshot(
                    storage_root,
                    sqlite_path,
                    &cx,
                ))
                .expect("refreshed archive snapshot");
                if Arc::as_ptr(&snapshot) as usize != old_pointer {
                    return snapshot;
                }
                assert!(
                    Instant::now() < deadline,
                    "background snapshot refresh did not publish a replacement"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn process_env_overrides_bypass_stale_dependency_config_cache() {
            Config::reset_cached();
            assert!(
                !Config::get().worktrees_enabled,
                "default test config should leave worktrees disabled"
            );

            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("WORKTREES_ENABLED", "true")],
                || {
                    assert!(
                        Config::get().worktrees_enabled,
                        "dependency users of with_process_env_overrides_for_test must not reuse stale cached Config"
                    );
                },
            );

            Config::reset_cached();
        }

        #[test]
        fn resolve_project_scopes_cache_to_each_database_generation() {
            let temp = tempfile::tempdir().expect("tempdir");
            let live_path = temp.path().join("live.sqlite3");
            let snapshot_path = temp.path().join("snapshot.sqlite3");
            for (path, id, human_key) in [
                (&live_path, 17_i64, "/live-generation"),
                (&snapshot_path, 42_i64, "/snapshot-generation"),
            ] {
                let conn = mcp_agent_mail_db::DbConn::open_file(path.to_string_lossy().as_ref())
                    .expect("open split-brain fixture");
                conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                    .expect("initialize split-brain fixture");
                conn.query_sync(
                    "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, 'same-project', ?, 0)",
                    &[
                        mcp_agent_mail_db::sqlmodel::Value::BigInt(id),
                        mcp_agent_mail_db::sqlmodel::Value::Text(human_key.to_string()),
                    ],
                )
                .expect("seed split-brain project");
            }
            let make_pool = |path: &Path| {
                mcp_agent_mail_db::create_query_only_pool(&mcp_agent_mail_db::DbPoolConfig {
                    database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(path),
                    storage_root: Some(temp.path().join("archive")),
                    min_connections: 0,
                    max_connections: 1,
                    run_migrations: false,
                    warmup_connections: 0,
                    ..Default::default()
                })
                .expect("create split-brain query pool")
            };
            let live_pool = make_pool(&live_path);
            let snapshot_pool = make_pool(&snapshot_path);
            let cx = asupersync::Cx::for_testing();
            let ctx = McpContext::new(cx, 1);

            let (live, snapshot, live_again) = run_async(async {
                let live = resolve_project(&ctx, &live_pool, "same-project")
                    .await
                    .expect("resolve live generation");
                let snapshot = resolve_project(&ctx, &snapshot_pool, "same-project")
                    .await
                    .expect("resolve snapshot generation");
                let live_again = resolve_project(&ctx, &live_pool, "same-project")
                    .await
                    .expect("resolve live generation again");
                (live, snapshot, live_again)
            });

            assert_eq!(
                (live.id, live.human_key.as_str()),
                (Some(17), "/live-generation")
            );
            assert_eq!(
                (snapshot.id, snapshot.human_key.as_str()),
                (Some(42), "/snapshot-generation")
            );
            assert_eq!(
                (live_again.id, live_again.human_key.as_str()),
                (Some(17), "/live-generation"),
                "snapshot cache population must not replace the live pool identity"
            );
        }

        #[test]
        fn snapshot_probes_are_read_only_from_the_first_open() {
            let temp = tempfile::tempdir().expect("tempdir");
            let absent = temp.path().join("absent.sqlite3");
            assert!(
                open_read_only_snapshot_probe(&absent, "absent probe").is_err(),
                "absent probe must fail closed"
            );
            assert!(!absent.exists(), "absent probe must not create a database");

            let valid = temp.path().join("valid.sqlite3");
            let seed = mcp_agent_mail_db::DbConn::open_file(valid.to_string_lossy().as_ref())
                .expect("open valid probe fixture");
            seed.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize valid probe fixture");
            drop(seed);
            let probe = open_read_only_snapshot_probe(&valid, "valid probe")
                .expect("open valid probe read-only");
            let query_only = probe
                .query_sync("PRAGMA query_only", &[])
                .expect("read query-only pragma")[0]
                .get_as::<i64>(0)
                .expect("decode query-only pragma");
            assert_eq!(query_only, 1);
            assert!(
                probe
                    .query_sync(
                        "INSERT INTO projects (slug, human_key, created_at) VALUES ('forbidden', '/forbidden', 0)",
                        &[],
                    )
                    .is_err(),
                "snapshot probe must reject writes"
            );
            drop(probe);

            let raced = temp.path().join("raced-away.sqlite3");
            let seed = mcp_agent_mail_db::DbConn::open_file(raced.to_string_lossy().as_ref())
                .expect("open raced probe fixture");
            seed.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize raced probe fixture");
            drop(seed);
            assert!(raced.is_file(), "race fixture must exist before unlink");
            std::fs::remove_file(&raced).expect("unlink race fixture");
            assert!(
                open_read_only_snapshot_probe(&raced, "raced probe").is_err(),
                "probe must fail if the source vanishes before its first open"
            );
            assert!(!raced.exists(), "raced probe must not recreate the source");
        }

        #[test]
        fn file_read_fallback_is_query_only_and_never_bootstraps() {
            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            std::fs::create_dir_all(&storage_root).expect("create read fallback archive");
            let db_path = temp.path().join("read-fallback.sqlite3");
            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path);
            let storage_root_text = storage_root.display().to_string();
            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[
                    ("DATABASE_URL", database_url.as_str()),
                    ("STORAGE_ROOT", storage_root_text.as_str()),
                ],
                || {
                    Config::reset_cached();
                    let pool = get_live_db_pool().expect("construct absent read fallback");
                    let cx = asupersync::Cx::for_testing();
                    assert!(
                        !matches!(run_async(pool.acquire(&cx)), Outcome::Ok(_)),
                        "absent file read fallback must fail closed"
                    );
                    Config::reset_cached();
                },
            );
            assert!(
                !db_path.exists(),
                "read fallback must not create the live DB"
            );

            let seed = mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref())
                .expect("open read fallback fixture");
            seed.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize read fallback fixture");
            drop(seed);
            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[
                    ("DATABASE_URL", database_url.as_str()),
                    ("STORAGE_ROOT", storage_root_text.as_str()),
                ],
                || {
                    Config::reset_cached();
                    let pool = get_live_db_pool().expect("construct valid read fallback");
                    let cx = asupersync::Cx::for_testing();
                    let conn = match run_async(pool.acquire(&cx)) {
                        Outcome::Ok(conn) => conn,
                        Outcome::Err(err) => panic!("valid read fallback acquire failed: {err}"),
                        Outcome::Cancelled(_) => panic!("valid read fallback acquire cancelled"),
                        Outcome::Panicked(panic) => {
                            panic!("valid read fallback acquire panicked: {}", panic.message())
                        }
                    };
                    assert!(
                        conn.query_sync(
                            "INSERT INTO projects (slug, human_key, created_at) VALUES ('forbidden', '/forbidden', 0)",
                            &[],
                        )
                        .is_err(),
                        "file read fallback must reject writes"
                    );
                    Config::reset_cached();
                },
            );
        }

        fn write_inventory_message(storage_root: &Path, file_id: i64, message_id: i64) {
            let project_dir = storage_root.join("projects").join("cache-project");
            let message_dir = project_dir.join("messages").join("2026").join("07");
            std::fs::create_dir_all(&message_dir).expect("create message directory");
            std::fs::write(
                project_dir.join("project.json"),
                r#"{"slug":"cache-project","human_key":"/cache-project"}"#,
            )
            .expect("write project metadata");
            std::fs::write(
                message_dir.join(format!("2026-07-14T00-00-00Z__cached__{file_id}.md")),
                format!(
                    r#"---json
{{
  "id": {message_id},
  "from": "Alice",
  "to": [],
  "subject": "Cached",
  "importance": "normal",
  "created_ts": "2026-07-14T00:00:00Z"
}}
---

body
"#
                ),
            )
            .expect("write canonical message");
        }

        fn commit_archive_tree(repo: &git2::Repository, message: &str) {
            let mut index = repo.index().expect("open git index");
            index
                .add_all(["projects"], git2::IndexAddOption::DEFAULT, None)
                .expect("stage archive tree");
            index.write().expect("write git index");
            let tree_id = index.write_tree().expect("write git tree");
            let tree = repo.find_tree(tree_id).expect("load git tree");
            let signature =
                git2::Signature::now("test", "test@example.com").expect("build git signature");
            let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
            let parents = parent.iter().collect::<Vec<_>>();
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents,
            )
            .expect("commit archive tree");
        }

        #[test]
        fn snapshot_completeness_rejects_reconstruction_traversal_loss() {
            let inventory = mcp_agent_mail_db::ArchiveMessageInventory {
                projects: 1,
                agents: 1,
                canonical_message_files: 1,
                ..mcp_agent_mail_db::ArchiveMessageInventory::default()
            };
            let mut stats = mcp_agent_mail_db::ReconstructStats::default();
            stats.projects = 1;

            let error = validate_read_snapshot_completeness(&inventory, &stats)
                .expect_err("a best-effort reconstruction traversal loss must fail publication");
            assert!(
                matches!(
                    error,
                    ReadSnapshotAcquireError::Failed(ref detail)
                        if detail.contains("reconstruction was incomplete")
                            && detail.contains("inventory projects=1 agents=1 canonical_message_files=1")
                            && detail.contains("reconstructed projects=1 agents=0 messages=0")
                ),
                "unexpected reconstruction completeness error: {error:?}"
            );
        }

        #[test]
        fn snapshot_completeness_counts_duplicate_files_but_not_salvage_rows() {
            let inventory = mcp_agent_mail_db::ArchiveMessageInventory {
                projects: 1,
                agents: 1,
                canonical_message_files: 2,
                ..mcp_agent_mail_db::ArchiveMessageInventory::default()
            };
            let mut stats = mcp_agent_mail_db::ReconstructStats::default();
            stats.projects = 1;
            stats.agents = 1;
            stats.messages = 1;
            stats.duplicate_canonical_message_files = 1;
            stats.salvaged_projects = 9;
            stats.salvaged_agents = 9;
            stats.salvaged_messages = 9;

            validate_read_snapshot_completeness(&inventory, &stats).expect(
                "archive completeness is reconstructed messages plus duplicate canonical files; salvage is separate",
            );
        }

        #[cfg(unix)]
        #[test]
        fn hostile_reconstruction_traversal_never_publishes_ready_snapshot() {
            use std::os::unix::fs::PermissionsExt;

            struct PermissionRestore {
                path: PathBuf,
                permissions: std::fs::Permissions,
            }

            impl Drop for PermissionRestore {
                fn drop(&mut self) {
                    let _ = std::fs::set_permissions(&self.path, self.permissions.clone());
                }
            }

            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            let project_dir = storage_root.join("projects").join("stable-project");
            let agents_dir = project_dir.join("agents");
            let agent_dir = agents_dir.join("StableAgent");
            std::fs::create_dir_all(&agent_dir).expect("create stable agent directory");
            std::fs::write(
                project_dir.join("project.json"),
                r#"{"slug":"stable-project","human_key":"/stable-project"}"#,
            )
            .expect("write stable project metadata");
            std::fs::write(agent_dir.join("profile.json"), "{}").expect("write stable profile");
            commit_archive_tree(&repo, "commit stable traversal fixture");

            let permission_restore = PermissionRestore {
                path: agents_dir.clone(),
                permissions: std::fs::metadata(&agents_dir)
                    .expect("inspect agent directory permissions")
                    .permissions(),
            };
            set_read_snapshot_pre_reconstruction_test_hook({
                let agents_dir = agents_dir.clone();
                move || {
                    std::fs::set_permissions(&agents_dir, std::fs::Permissions::from_mode(0))
                        .expect("make reconstruction traversal hostile");
                }
            });

            let sqlite_path = temp.path().join("missing-live.sqlite3");
            let cx = asupersync::Cx::for_testing();
            let error = match run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cx,
            )) {
                Err(error) => error,
                Ok(_) => panic!("a reconstruction traversal loss must fail the snapshot build"),
            };
            assert!(
                matches!(
                    error,
                    ReadSnapshotAcquireError::Failed(ref detail)
                        if detail.contains("reconstruction was incomplete")
                            && detail.contains("inventory projects=1 agents=1")
                            && detail.contains("reconstructed projects=1 agents=0")
                ),
                "unexpected hostile reconstruction error: {error:?}"
            );
            let (ready, building, reconstructions, inflight, _) = read_snapshot_cache_stats();
            assert_eq!(ready, 0, "traversal loss must not publish ready state");
            assert!(!building, "failed traversal must release its build slot");
            assert_eq!(reconstructions, 1);
            assert_eq!(inflight, 0);

            drop(permission_restore);
            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();
        }

        #[test]
        fn malformed_stable_agent_profile_never_publishes_ready_snapshot() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            let project_dir = storage_root.join("projects").join("stable-project");
            let agent_dir = project_dir.join("agents").join("BrokenAgent");
            std::fs::create_dir_all(&agent_dir).expect("create malformed agent directory");
            std::fs::write(
                project_dir.join("project.json"),
                r#"{"slug":"stable-project","human_key":"/stable-project"}"#,
            )
            .expect("write stable project metadata");
            std::fs::write(agent_dir.join("profile.json"), "{not-json")
                .expect("write malformed stable profile");
            commit_archive_tree(&repo, "commit malformed stable agent profile");

            let sqlite_path = temp.path().join("missing-live.sqlite3");
            let cx = asupersync::Cx::for_testing();
            let error = match run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cx,
            )) {
                Err(error) => error,
                Ok(_) => panic!("malformed stable profile must fail the cold snapshot build"),
            };
            assert!(
                matches!(
                    error,
                    ReadSnapshotAcquireError::Failed(ref detail)
                        if detail.contains("reconstruction skipped 1 malformed or unreadable archive artifact")
                ),
                "unexpected malformed-profile snapshot error: {error:?}"
            );
            let (ready, building, reconstructions, inflight, _) = read_snapshot_cache_stats();
            assert_eq!(
                ready, 0,
                "a failed reconstruction must not publish ready state"
            );
            assert!(
                !building,
                "failed reconstruction must release its build slot"
            );
            assert_eq!(reconstructions, 1);
            assert_eq!(inflight, 0);

            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();
        }

        #[derive(Debug, Clone, Copy)]
        enum ArchivePublicationRaceMutation {
            ProjectMetadata,
            AgentProfile,
            RawAttachment,
            NotificationSignal,
            AsyncCommit,
            ColdPoolBootstrap,
        }

        impl ArchivePublicationRaceMutation {
            fn label(self) -> &'static str {
                match self {
                    Self::ProjectMetadata => "project metadata",
                    Self::AgentProfile => "agent profile",
                    Self::RawAttachment => "raw attachment",
                    Self::NotificationSignal => "notification signal",
                    Self::AsyncCommit => "async commit",
                    Self::ColdPoolBootstrap => "cold live-pool bootstrap",
                }
            }
        }

        struct PostValidationHookReleaseGuard {
            active: bool,
        }

        impl PostValidationHookReleaseGuard {
            fn new() -> Self {
                Self { active: true }
            }

            fn release(&mut self) {
                if self.active {
                    release_read_snapshot_post_validation_test_hook();
                    self.active = false;
                }
            }
        }

        impl Drop for PostValidationHookReleaseGuard {
            fn drop(&mut self) {
                self.release();
            }
        }

        fn wait_for_read_snapshot_build_completion(label: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while read_snapshot_cache_stats().1 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !read_snapshot_cache_stats().1,
                "{label} snapshot builder did not finish after the race hook was released"
            );
        }

        fn run_archive_publication_race_case(mutation: ArchivePublicationRaceMutation) {
            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();
            mcp_agent_mail_storage::flush_async_commits();

            let temp = tempfile::tempdir().expect("race tempdir");
            let storage_root = temp.path().join("archive");
            let mut config = Config {
                storage_root: storage_root.clone(),
                ..Config::default()
            };
            config.notifications_enabled = true;
            config.notifications_debounce_ms = 0;
            config.notifications_signals_dir = temp.path().join("signals");

            let archive = mcp_agent_mail_storage::ensure_archive(&config, "cache-project")
                .expect("ensure race archive");
            write_inventory_message(&storage_root, 1, 1);
            let repo = git2::Repository::open(&storage_root).expect("open race archive repo");
            commit_archive_tree(&repo, "seed publication race archive");

            let async_path = archive.root.join("async-only.txt");
            if matches!(mutation, ArchivePublicationRaceMutation::AsyncCommit) {
                std::fs::write(&async_path, b"commit me after final validation\n")
                    .expect("write async commit fixture");
            }

            let sqlite_path = temp.path().join("missing-live.sqlite3");
            let cx = asupersync::Cx::for_testing();
            let initial = run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cx,
            ))
            .expect("initial race snapshot");
            let initial_pointer = Arc::as_ptr(&initial) as usize;

            expire_read_snapshot_exact_audit();
            arm_read_snapshot_post_validation_test_hook();
            let mut release_guard = PostValidationHookReleaseGuard::new();
            let stale = run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cx,
            ))
            .expect("serve stale snapshot while exact audit runs");
            assert_eq!(
                Arc::as_ptr(&stale) as usize,
                initial_pointer,
                "{} case must initially serve the existing snapshot",
                mutation.label()
            );
            assert!(
                wait_for_read_snapshot_post_validation_test_hook(Duration::from_secs(5)),
                "{} exact audit did not pause after final validation",
                mutation.label()
            );
            let build_completion = {
                let scope =
                    read_snapshot_scope(&storage_root, &sqlite_path).expect("paused race scope");
                let slot = read_snapshot_slot(scope).expect("paused race slot");
                let state = slot
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Arc::clone(
                    &state
                        .building
                        .as_ref()
                        .expect("paused exact audit must retain its build")
                        .completion,
                )
            };

            let archive_epoch_before = mcp_agent_mail_storage::archive_application_epoch();
            let db_epoch_before =
                READ_SNAPSHOT_DB_WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire);
            let mut cold_pool = None;
            match mutation {
                ArchivePublicationRaceMutation::ProjectMetadata => {
                    mcp_agent_mail_storage::write_project_metadata_with_config(
                        &archive,
                        &config,
                        "/publication-race-metadata",
                    )
                    .expect("write project metadata during publication race");
                }
                ArchivePublicationRaceMutation::AgentProfile => {
                    mcp_agent_mail_storage::write_agent_profile_with_config(
                        &archive,
                        &config,
                        &json!({"name": "PublicationRaceAgent", "program": "test"}),
                    )
                    .expect("write agent profile during publication race");
                }
                ArchivePublicationRaceMutation::RawAttachment => {
                    let source = temp.path().join("publication-race.bin");
                    std::fs::write(&source, b"publication race attachment")
                        .expect("write attachment source fixture");
                    mcp_agent_mail_storage::store_raw_attachment(&archive, &source, 1024)
                        .expect("store raw attachment during publication race");
                }
                ArchivePublicationRaceMutation::NotificationSignal => {
                    assert_eq!(
                        mcp_agent_mail_storage::emit_notification_signal(
                            &config,
                            "cache-project",
                            "PublicationRaceAgent",
                            None,
                        ),
                        mcp_agent_mail_storage::SignalEmitOutcome::Emitted,
                        "emit signal during publication race"
                    );
                }
                ArchivePublicationRaceMutation::AsyncCommit => {
                    let head_before = repo
                        .head()
                        .expect("race HEAD before async commit")
                        .target()
                        .expect("race HEAD target before async commit");
                    mcp_agent_mail_storage::enqueue_async_commit(
                        &storage_root,
                        &config,
                        "test: async publication race",
                        &["projects/cache-project/async-only.txt".to_string()],
                    );
                    mcp_agent_mail_storage::flush_async_commits();
                    let head_after = repo
                        .head()
                        .expect("race HEAD after async commit")
                        .target()
                        .expect("race HEAD target after async commit");
                    assert_ne!(
                        head_before, head_after,
                        "async coalescer must apply a real Git mutation during the pause"
                    );
                }
                ArchivePublicationRaceMutation::ColdPoolBootstrap => {
                    let database_url =
                        mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path);
                    let storage_root_text = storage_root.display().to_string();
                    let pool = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                        &[
                            ("DATABASE_URL", database_url.as_str()),
                            ("STORAGE_ROOT", storage_root_text.as_str()),
                        ],
                        || {
                            Config::reset_cached();
                            let pool = get_db_pool().expect("enter cold-pool writer epoch");
                            let conn = match run_async(pool.acquire(&cx)) {
                                Outcome::Ok(conn) => conn,
                                Outcome::Err(err) => {
                                    panic!("cold live-pool bootstrap failed: {err}")
                                }
                                Outcome::Cancelled(_) => {
                                    panic!("cold live-pool bootstrap cancelled")
                                }
                                Outcome::Panicked(panic) => {
                                    panic!("cold live-pool bootstrap panicked: {}", panic.message())
                                }
                            };
                            drop(conn);
                            Config::reset_cached();
                            pool
                        },
                    );
                    assert!(
                        sqlite_path.is_file(),
                        "cold pool must bootstrap the live DB"
                    );
                    cold_pool = Some(pool);
                }
            }
            if matches!(mutation, ArchivePublicationRaceMutation::ColdPoolBootstrap) {
                assert_ne!(
                    db_epoch_before,
                    READ_SNAPSHOT_DB_WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire),
                    "cold bootstrap must enter the writer epoch before pool construction"
                );
            } else {
                assert_ne!(
                    archive_epoch_before,
                    mcp_agent_mail_storage::archive_application_epoch(),
                    "{} must cross the guarded archive mutation boundary",
                    mutation.label()
                );
            }

            release_guard.release();
            wait_for_read_snapshot_build_completion(mutation.label());

            let scope = read_snapshot_scope(&storage_root, &sqlite_path).expect("race scope");
            let slot = read_snapshot_slot(scope).expect("race slot");
            let state = slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ready = state
                .ready
                .as_ref()
                .and_then(|entry| entry.decision.immutable_snapshot())
                .expect("existing snapshot remains ready after rejected publication");
            assert_eq!(
                Arc::as_ptr(&ready) as usize,
                initial_pointer,
                "{} mutation must prevent the validated candidate from replacing the ready snapshot",
                mutation.label()
            );
            assert!(matches!(
                build_completion.result(),
                Some(Err(ReadSnapshotAcquireError::Busy(message)))
                    if message.contains("superseded by a durable write")
            ));
            drop(state);
            drop(ready);
            drop(slot);
            drop(cold_pool);

            mcp_agent_mail_storage::flush_async_commits();
            drop(stale);
            drop(initial);
            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();
        }

        #[test]
        fn post_validation_archive_mutations_reject_snapshot_publication() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for mutation in [
                ArchivePublicationRaceMutation::ProjectMetadata,
                ArchivePublicationRaceMutation::AgentProfile,
                ArchivePublicationRaceMutation::RawAttachment,
                ArchivePublicationRaceMutation::NotificationSignal,
                ArchivePublicationRaceMutation::AsyncCommit,
                ArchivePublicationRaceMutation::ColdPoolBootstrap,
            ] {
                run_archive_publication_race_case(mutation);
            }
        }

        #[test]
        fn read_archive_inventory_cache_never_hides_uncommitted_archive_state() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_archive_inventory_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");

            write_inventory_message(&storage_root, 1, 1);
            commit_archive_tree(&repo, "add first message");
            assert_eq!(
                read_archive_inventory(&storage_root).latest_message_id,
                Some(1)
            );
            assert_eq!(read_archive_inventory_scan_count(), 1);

            assert_eq!(
                read_archive_inventory(&storage_root).latest_message_id,
                Some(1)
            );
            assert_eq!(
                read_archive_inventory_scan_count(),
                1,
                "a clean, unchanged HEAD should reuse the cached inventory"
            );

            write_inventory_message(&storage_root, 2, 2);
            assert_eq!(read_archive_inventory(&storage_root).unique_message_ids, 2);
            assert_eq!(read_archive_inventory_scan_count(), 2);
            assert_eq!(read_archive_inventory(&storage_root).unique_message_ids, 2);
            assert_eq!(
                read_archive_inventory_scan_count(),
                3,
                "a dirty worktree must never populate or reuse the cache"
            );

            commit_archive_tree(&repo, "add second message");
            assert_eq!(read_archive_inventory(&storage_root).unique_message_ids, 2);
            assert_eq!(read_archive_inventory_scan_count(), 4);
            assert_eq!(read_archive_inventory(&storage_root).unique_message_ids, 2);
            assert_eq!(read_archive_inventory_scan_count(), 4);

            write_inventory_message(&storage_root, 1, 9);
            assert_eq!(
                read_archive_inventory(&storage_root).latest_message_id,
                Some(9)
            );
            assert_eq!(
                read_archive_inventory_scan_count(),
                5,
                "modifying a tracked archive artifact must bypass the cached HEAD"
            );

            reset_read_archive_inventory_cache();
        }

        #[test]
        fn read_archive_inventory_cache_is_bounded_and_scoped_by_canonical_root() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_archive_inventory_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let mut roots = Vec::new();
            for index in 0..=READ_ARCHIVE_INVENTORY_CACHE_CAPACITY {
                let root = temp.path().join(format!("archive-{index}"));
                let repo = git2::Repository::init(&root).expect("init archive");
                let message_id = i64::try_from(index + 1).expect("small cache test index");
                write_inventory_message(&root, message_id, message_id);
                commit_archive_tree(&repo, "seed archive");
                assert_eq!(
                    read_archive_inventory(&root).latest_message_id,
                    Some(message_id)
                );
                roots.push(root);
            }

            assert_eq!(
                read_archive_inventory_scan_count(),
                READ_ARCHIVE_INVENTORY_CACHE_CAPACITY + 1
            );
            assert_eq!(
                read_archive_inventory_cache_len(),
                READ_ARCHIVE_INVENTORY_CACHE_CAPACITY,
                "the cache must evict its least-recently-used entry"
            );

            let newest_root = roots.last().expect("newest archive root");
            assert_eq!(
                read_archive_inventory(newest_root).latest_message_id,
                Some(i64::try_from(roots.len()).expect("small cache test length"))
            );
            assert_eq!(
                read_archive_inventory_scan_count(),
                READ_ARCHIVE_INVENTORY_CACHE_CAPACITY + 1,
                "the newest independent root should remain cached"
            );

            let oldest_root = roots.first().expect("oldest archive root");
            assert_eq!(
                read_archive_inventory(oldest_root).latest_message_id,
                Some(1)
            );
            assert_eq!(
                read_archive_inventory_scan_count(),
                READ_ARCHIVE_INVENTORY_CACHE_CAPACITY + 2,
                "reading the evicted root must perform a fresh scan"
            );

            reset_read_archive_inventory_cache();
        }

        #[test]
        fn snapshot_registry_and_pending_queue_enforce_hard_admission_caps() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();

            let held = (0..READ_SNAPSHOT_SCOPE_CAPACITY)
                .map(|index| {
                    read_snapshot_slot(ReadSnapshotScope {
                        storage_root: PathBuf::from(format!("/held/archive-{index}")),
                        sqlite_path: PathBuf::from(format!("/held/mailbox-{index}.sqlite3")),
                    })
                    .expect("admit slot below hard capacity")
                })
                .collect::<Vec<_>>();
            assert!(matches!(
                read_snapshot_slot(ReadSnapshotScope {
                    storage_root: PathBuf::from("/held/archive-overflow"),
                    sqlite_path: PathBuf::from("/held/mailbox-overflow.sqlite3"),
                }),
                Err(ReadSnapshotAcquireError::Busy(_))
            ));
            assert_eq!(
                READ_SNAPSHOT_REGISTRY
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .slots
                    .len(),
                READ_SNAPSHOT_SCOPE_CAPACITY
            );

            let permits = (0..READ_SNAPSHOT_PENDING_BUILD_LIMIT)
                .map(|_| ReadSnapshotBuildPermit::try_acquire().expect("pending permit"))
                .collect::<Vec<_>>();
            assert!(ReadSnapshotBuildPermit::try_acquire().is_none());
            drop(permits);
            drop(held);
            reset_read_snapshot_cache();
        }

        #[test]
        fn archive_read_snapshot_singleflight_is_bounded_and_invalidates_dirty_generation() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_archive_inventory_cache();
            reset_read_snapshot_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            write_inventory_message(&storage_root, 1, 1);
            commit_archive_tree(&repo, "seed archive");
            let sqlite_path = temp.path().join("missing-live.sqlite3");

            let cancelled_cx = asupersync::Cx::for_testing();
            cancelled_cx.set_cancel_requested(true);
            let cancelled = match run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cancelled_cx,
            )) {
                Err(error) => error,
                Ok(_) => panic!("a cancelled request must not start reconstruction"),
            };
            assert!(matches!(cancelled, ReadSnapshotAcquireError::Cancelled));
            assert_eq!(
                read_snapshot_cache_stats(),
                (0, false, 0, 0, 0),
                "cancelled admission must leave no builder or cached generation"
            );
            assert_eq!(read_snapshot_generation_scan_stats(), (0, 0, 0));

            let worker_count = 12;
            let barrier = Arc::new(std::sync::Barrier::new(worker_count));
            let pointers = Arc::new(Mutex::new(Vec::new()));
            std::thread::scope(|scope| {
                for _ in 0..worker_count {
                    let barrier = Arc::clone(&barrier);
                    let pointers = Arc::clone(&pointers);
                    let storage_root = storage_root.clone();
                    let sqlite_path = sqlite_path.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        let cx = asupersync::Cx::for_testing();
                        let snapshot = run_async(get_or_build_archive_read_snapshot(
                            &storage_root,
                            &sqlite_path,
                            &cx,
                        ))
                        .expect("shared archive read snapshot");
                        pointers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(Arc::as_ptr(&snapshot) as usize);
                    });
                }
            });

            let pointers = pointers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(pointers.len(), worker_count);
            assert!(
                pointers.windows(2).all(|pair| pair[0] == pair[1]),
                "all concurrent readers must receive the same immutable snapshot"
            );
            let original_pointer = pointers[0];
            drop(pointers);

            let (ready, building, reconstructions, inflight, max_inflight) =
                read_snapshot_cache_stats();
            assert_eq!(ready, 1);
            assert!(!building);
            assert_eq!(reconstructions, 1);
            assert_eq!(inflight, 0);
            assert_eq!(
                max_inflight, 1,
                "single-flight must never run more than one reconstruction task"
            );
            assert_eq!(
                read_snapshot_generation_scan_stats(),
                (2, 0, 1),
                "N concurrent cold reads require only before/after generation scans"
            );

            expire_read_snapshot_validation();
            let validation_barrier = Arc::new(std::sync::Barrier::new(worker_count));
            let validation_pointers = Arc::new(Mutex::new(Vec::new()));
            std::thread::scope(|scope| {
                for _ in 0..worker_count {
                    let barrier = Arc::clone(&validation_barrier);
                    let pointers = Arc::clone(&validation_pointers);
                    let storage_root = storage_root.clone();
                    let sqlite_path = sqlite_path.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        let cx = asupersync::Cx::for_testing();
                        let snapshot = run_async(get_or_build_archive_read_snapshot(
                            &storage_root,
                            &sqlite_path,
                            &cx,
                        ))
                        .expect("coalesced snapshot validation");
                        pointers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(Arc::as_ptr(&snapshot) as usize);
                    });
                }
            });
            let validation_pointers = validation_pointers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(validation_pointers.len(), worker_count);
            assert!(
                validation_pointers
                    .iter()
                    .all(|pointer| *pointer == original_pointer),
                "a coalesced validation must reuse the exact immutable snapshot"
            );
            drop(validation_pointers);
            let validation_deadline = Instant::now() + Duration::from_secs(5);
            while read_snapshot_cache_stats().1 && Instant::now() < validation_deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !read_snapshot_cache_stats().1,
                "validation worker did not finish"
            );
            assert_eq!(
                read_snapshot_generation_scan_stats(),
                (2, 0, 1),
                "routine cache validation must use the cheap metadata and epoch signature"
            );
            assert_eq!(read_snapshot_cache_stats().2, 1);

            write_inventory_message(&storage_root, 2, 2);
            expire_read_snapshot_exact_audit();
            let replacement =
                wait_for_snapshot_pointer_change(&storage_root, &sqlite_path, original_pointer);
            let replacement_pointer = Arc::as_ptr(&replacement) as usize;
            assert_ne!(
                replacement_pointer, original_pointer,
                "a dirty archive generation must not reuse the committed snapshot"
            );
            let cx = asupersync::Cx::for_testing();
            let replacement_again = run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cx,
            ))
            .expect("reuse replacement snapshot");
            assert_eq!(
                Arc::as_ptr(&replacement_again) as usize,
                replacement_pointer,
                "the unchanged dirty generation should reuse its immutable snapshot"
            );

            write_inventory_message(&storage_root, 2, 3);
            expire_read_snapshot_exact_audit();
            let mutated =
                wait_for_snapshot_pointer_change(&storage_root, &sqlite_path, replacement_pointer);
            let mutated_pointer = Arc::as_ptr(&mutated) as usize;
            assert_ne!(
                mutated_pointer, replacement_pointer,
                "an in-place dirty artifact mutation must invalidate the snapshot"
            );
            let mutated_again = run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cx,
            ))
            .expect("reuse in-place mutation snapshot");
            assert_eq!(
                Arc::as_ptr(&mutated_again) as usize,
                mutated_pointer,
                "an unchanged in-place mutation generation should be reused"
            );
            drop(mutated_again);
            drop(mutated);
            drop(replacement_again);
            drop(replacement);

            let (ready, building, reconstructions, inflight, max_inflight) =
                read_snapshot_cache_stats();
            assert_eq!(ready, 1, "only the latest mailbox generation is retained");
            assert!(!building);
            assert_eq!(
                reconstructions, 3,
                "new and mutated dirty archive content must invalidate prior generations"
            );
            assert_eq!(inflight, 0);
            assert_eq!(max_inflight, 1);
            assert_eq!(
                read_snapshot_generation_scan_stats(),
                (6, 0, 1),
                "cheap validation adds no strong scan and both invalidations stay single-flight"
            );

            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();
        }

        #[test]
        fn real_tool_read_path_coalesces_inventory_generation_and_reconstruction() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_archive_inventory_cache();
            reset_read_snapshot_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            git2::Repository::init(&storage_root).expect("init dirty archive repo");
            write_inventory_message(&storage_root, 1, 1);
            let db_path = temp.path().join("live.sqlite3");
            let conn = mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref())
                .expect("open live db");
            conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize live schema");
            drop(conn);

            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path);
            let storage_root_text = storage_root.to_string_lossy().into_owned();
            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[
                    ("DATABASE_URL", database_url.as_str()),
                    ("STORAGE_ROOT", storage_root_text.as_str()),
                ],
                || {
                    Config::reset_cached();

                    let cancelled_cx = asupersync::Cx::for_testing();
                    cancelled_cx.set_cancel_requested(true);
                    let cancelled = run_async(open_read_db_pool_with_cx(&cancelled_cx));
                    assert!(matches!(
                        cancelled,
                        Err(ReadSnapshotAcquireError::Cancelled)
                    ));
                    assert_eq!(read_archive_inventory_scan_count(), 0);
                    assert_eq!(read_snapshot_generation_scan_stats(), (0, 0, 0));
                    assert_eq!(read_snapshot_cache_stats(), (0, false, 0, 0, 0));

                    let worker_count = 12;
                    let barrier = Arc::new(std::sync::Barrier::new(worker_count));
                    let pointers = Arc::new(Mutex::new(Vec::new()));
                    std::thread::scope(|scope| {
                        for _ in 0..worker_count {
                            let barrier = Arc::clone(&barrier);
                            let pointers = Arc::clone(&pointers);
                            scope.spawn(move || {
                                let cx = asupersync::Cx::for_testing();
                                barrier.wait();
                                let pool = run_async(open_read_db_pool_with_cx(&cx))
                                    .expect("real read path")
                                    .expect("archive snapshot pool");
                                let snapshot = pool
                                    ._snapshot
                                    .as_ref()
                                    .expect("snapshot ownership must be retained");
                                pointers
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .push(Arc::as_ptr(snapshot) as usize);
                            });
                        }
                    });

                    let pointers = pointers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    assert_eq!(pointers.len(), worker_count);
                    assert!(pointers.windows(2).all(|pair| pair[0] == pair[1]));
                    assert_eq!(
                        read_archive_inventory_scan_count(),
                        1,
                        "the real tool predicate must reuse one inventory for state and drift"
                    );
                    assert_eq!(read_snapshot_generation_scan_stats(), (2, 0, 1));
                    assert_eq!(read_snapshot_cache_stats(), (1, false, 1, 0, 1));
                },
            );

            reset_read_snapshot_cache();
            reset_read_archive_inventory_cache();
        }

        #[test]
        fn expired_live_decision_serves_stale_while_cheap_validation_runs() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();
            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            git2::Repository::init(&storage_root).expect("init empty archive");
            let sqlite_path = temp.path().join("mailbox.sqlite3");
            let conn = mcp_agent_mail_db::DbConn::open_file(sqlite_path.to_string_lossy().as_ref())
                .expect("open live mailbox");
            conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize live mailbox");
            drop(conn);
            let cx = asupersync::Cx::for_testing();
            let database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path);
            assert!(
                run_async(get_archive_read_snapshot_if(
                    &storage_root,
                    &sqlite_path,
                    &database_url,
                    &cx,
                ))
                .expect("initial live decision")
                .is_none()
            );

            expire_read_snapshot_validation();
            set_read_snapshot_test_delay(Duration::from_millis(400));
            let started = Instant::now();
            assert!(
                run_async(get_archive_read_snapshot_if(
                    &storage_root,
                    &sqlite_path,
                    &database_url,
                    &cx,
                ))
                .expect("stale live decision")
                .is_none()
            );
            assert!(
                started.elapsed() < Duration::from_millis(200),
                "expired Live decisions must not wait for background validation"
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while read_snapshot_cache_stats().1 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            set_read_snapshot_test_delay(Duration::ZERO);
            assert!(!read_snapshot_cache_stats().1);
            assert_eq!(read_snapshot_cache_stats().2, 0);
            assert_eq!(read_snapshot_generation_scan_stats(), (2, 0, 1));
            reset_read_snapshot_cache();
        }

        #[test]
        fn archive_read_snapshot_waiter_uses_leader_deadline_without_starting_second_builder() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            write_inventory_message(&storage_root, 1, 1);
            commit_archive_tree(&repo, "seed archive");
            let sqlite_path = temp.path().join("missing-live.sqlite3");
            let scope = read_snapshot_scope(&storage_root, &sqlite_path).expect("snapshot scope");

            let slot = read_snapshot_slot(scope).expect("snapshot slot");
            let simulated_build = ReadSnapshotBuild {
                token: 7,
                invalidation_epoch: 0,
                db_write_epoch: READ_SNAPSHOT_DB_WRITE_EPOCH
                    .load(std::sync::atomic::Ordering::Acquire),
                archive_application_epoch: mcp_agent_mail_storage::archive_application_epoch(),
                deadline: Instant::now(),
                completion: Arc::new(ReadSnapshotBuildCompletion::pending()),
            };
            slot.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .building = Some(simulated_build.clone());

            let cx = asupersync::Cx::for_testing();
            let error = run_async(wait_for_read_snapshot_build(
                &slot,
                simulated_build.clone(),
                &cx,
                Instant::now(),
            ))
            .expect_err("expired leader deadline must time out");
            assert!(matches!(
                error,
                ReadSnapshotAcquireError::TimedOut(ref message)
                    if message.contains("cold bootstrap")
            ));
            assert_eq!(
                read_snapshot_cache_stats(),
                (0, true, 0, 0, 0),
                "a timed-out waiter must not replace or clear the leader"
            );
            assert_eq!(
                read_snapshot_generation_scan_stats(),
                (0, 0, 0),
                "a timed-out waiter must not start generation discovery"
            );
            slot.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .building = None;
            assert_eq!(read_snapshot_cache_stats(), (0, false, 0, 0, 0));
            reset_read_snapshot_cache();
        }

        #[test]
        fn archive_read_snapshot_waiter_retains_its_generation_completion() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            std::fs::create_dir_all(&storage_root).expect("create archive root");
            let scope = read_snapshot_scope(&storage_root, &temp.path().join("live.sqlite3"))
                .expect("snapshot scope");
            let slot = read_snapshot_slot(scope).expect("snapshot slot");
            let deadline = Instant::now() + Duration::from_secs(1);

            let first = try_claim_read_snapshot_build(&slot, deadline)
                .expect("claim first build")
                .expect("first build");
            {
                let mut state = slot
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.building = None;
                first
                    .completion
                    .complete(Err(ReadSnapshotAcquireError::Failed(
                        "first generation failed".to_string(),
                    )));
            }

            let second = try_claim_read_snapshot_build(&slot, deadline)
                .expect("claim second build")
                .expect("second build");
            {
                let mut state = slot
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.building = None;
                second.completion.complete(Ok(()));
            }

            let third = try_claim_read_snapshot_build(&slot, deadline)
                .expect("claim third build")
                .expect("third build");
            let cx = asupersync::Cx::for_testing();
            let error = run_async(wait_for_read_snapshot_build(&slot, first, &cx, deadline))
                .expect_err("first waiter must observe its own failed generation");
            assert!(matches!(
                error,
                ReadSnapshotAcquireError::Failed(ref message)
                    if message == "first generation failed"
            ));
            assert!(matches!(second.completion.result(), Some(Ok(()))));
            assert_eq!(
                slot.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .building
                    .as_ref()
                    .map(|build| build.token),
                Some(third.token),
                "waiting on generation one must not follow or disturb generation three"
            );
            fail_scheduled_read_snapshot_build(&slot, &third, ReadSnapshotAcquireError::Cancelled);
            reset_read_snapshot_cache();
        }

        #[test]
        fn query_only_snapshot_pool_rejects_mutation_and_preserves_rows() {
            let temp = tempfile::tempdir().expect("tempdir");
            let sqlite_path = temp.path().join("immutable.sqlite3");
            let conn = mcp_agent_mail_db::DbConn::open_file(sqlite_path.to_string_lossy().as_ref())
                .expect("open snapshot fixture");
            conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize snapshot fixture");
            drop(conn);

            let pool =
                mcp_agent_mail_db::create_query_only_pool(&mcp_agent_mail_db::DbPoolConfig {
                    database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path),
                    storage_root: Some(temp.path().to_path_buf()),
                    ..Default::default()
                })
                .expect("query-only pool");
            let cx = asupersync::Cx::for_testing();
            let conn = match run_async(pool.acquire(&cx)) {
                Outcome::Ok(conn) => conn,
                Outcome::Err(err) => panic!("query-only pool acquire failed: {err}"),
                Outcome::Cancelled(_) => panic!("query-only pool acquire cancelled"),
                Outcome::Panicked(panic) => {
                    panic!("query-only pool acquire panicked: {}", panic.message())
                }
            };
            let mutation = conn.query_sync(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('leak', '/leak', 0)",
                &[],
            );
            assert!(
                mutation.is_err(),
                "shared snapshot connections must reject every DML statement"
            );
            let rows = conn
                .query_sync("SELECT COUNT(*) AS project_count FROM projects", &[])
                .expect("snapshot remains readable");
            assert_eq!(
                rows.first()
                    .and_then(|row| row.get_named::<i64>("project_count").ok()),
                Some(0),
                "failed mutation must not leak into later reads of the shared pool"
            );
        }

        #[test]
        fn slow_cold_snapshot_build_does_not_block_current_thread_executor() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            write_inventory_message(&storage_root, 1, 1);
            commit_archive_tree(&repo, "seed archive");
            let sqlite_path = temp.path().join("missing-live.sqlite3");
            READ_SNAPSHOT_TEST_DELAY_MILLIS.store(250, std::sync::atomic::Ordering::Release);

            let cx = asupersync::Cx::for_testing();
            let started = Instant::now();
            let _ = run_async(asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_millis(25),
                Box::pin(get_or_build_archive_read_snapshot(
                    &storage_root,
                    &sqlite_path,
                    &cx,
                )),
            ));
            assert!(
                started.elapsed() < Duration::from_millis(150),
                "a slow reconstruction must run on the blocking pool so the executor timer can fire"
            );
            assert!(
                read_snapshot_cache_stats().1,
                "the detached blocking worker should still own the cold build"
            );

            let wait_deadline = Instant::now() + Duration::from_secs(3);
            while read_snapshot_cache_stats().1 && Instant::now() < wait_deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !read_snapshot_cache_stats().1,
                "blocking worker did not finish"
            );
            reset_read_snapshot_cache();
        }

        #[test]
        fn live_generation_hashes_same_size_main_and_wal_content() {
            let temp = tempfile::tempdir().expect("tempdir");
            let sqlite_path = temp.path().join("mailbox.sqlite3");
            let wal_path = mcp_agent_mail_db::pool::sqlite_path_with_suffix(&sqlite_path, "-wal");
            std::fs::write(&sqlite_path, b"AAAA").expect("write main fixture");
            std::fs::write(&wal_path, b"1111").expect("write wal fixture");
            let first =
                read_live_db_generation(&sqlite_path, &|| false).expect("first live generation");

            std::fs::write(&sqlite_path, b"AAAA").expect("rewrite unchanged main fixture");
            std::fs::write(&wal_path, b"1111").expect("rewrite unchanged wal fixture");
            let metadata_only = read_live_db_generation(&sqlite_path, &|| false)
                .expect("metadata-only live generation");
            assert_eq!(
                first, metadata_only,
                "metadata-only rewrites must not change a content-addressed generation"
            );

            std::fs::write(&wal_path, b"2222").expect("same-size wal rewrite");
            let wal_changed =
                read_live_db_generation(&sqlite_path, &|| false).expect("wal-changed generation");
            assert_ne!(first, wal_changed, "same-size WAL writes must invalidate");

            std::fs::write(&sqlite_path, b"BBBB").expect("same-size main rewrite");
            std::fs::remove_file(&wal_path).expect("checkpoint removes wal");
            let checkpointed =
                read_live_db_generation(&sqlite_path, &|| false).expect("checkpointed generation");
            assert_ne!(
                wal_changed, checkpointed,
                "same-size main updates and WAL checkpoint transitions must invalidate"
            );
        }

        #[test]
        fn dropping_live_write_lease_immediately_invalidates_exact_scope() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();
            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            std::fs::create_dir_all(&storage_root).expect("create archive root");
            let sqlite_path = temp.path().join("mailbox.sqlite3");
            let conn = mcp_agent_mail_db::DbConn::open_file(sqlite_path.to_string_lossy().as_ref())
                .expect("open live fixture");
            conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize live fixture");
            drop(conn);
            let scope = read_snapshot_scope(&storage_root, &sqlite_path).expect("snapshot scope");
            let slot = read_snapshot_slot(scope).expect("snapshot slot");
            assert_eq!(
                slot.invalidation_epoch
                    .load(std::sync::atomic::Ordering::Acquire),
                0
            );
            let pool = mcp_agent_mail_db::create_pool_without_startup_init(
                &mcp_agent_mail_db::DbPoolConfig {
                    database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&sqlite_path),
                    storage_root: Some(storage_root.clone()),
                    ..Default::default()
                },
            )
            .expect("live write pool");
            let simulated_build =
                try_claim_read_snapshot_build(&slot, Instant::now() + Duration::from_secs(1))
                    .expect("claim build before writer")
                    .expect("new scope build");
            let write_pool = WriteDbPool {
                pool,
                _write_guard: ReadSnapshotWriteGuard::begin(
                    &storage_root,
                    Some(sqlite_path.as_path()),
                ),
            };
            assert_eq!(
                slot.invalidation_epoch
                    .load(std::sync::atomic::Ordering::Acquire),
                1,
                "write lease admission must invalidate before the first mutation"
            );
            assert_eq!(
                slot.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .active_writers,
                1
            );
            let nested_slot = begin_read_snapshot_write(&storage_root, &sqlite_path);
            assert_eq!(
                slot.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .active_writers,
                2,
                "nested write leases must keep the scope admitted until the final exit"
            );
            end_read_snapshot_write(nested_slot.as_deref());
            assert_eq!(
                slot.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .active_writers,
                1,
                "exiting one nested lease must not reopen snapshot admission"
            );
            let mut simulated_guard =
                ReadSnapshotBuildGuard::new(Arc::clone(&slot), simulated_build.token);
            simulated_guard.publish(
                &simulated_build,
                Ok(ReadSnapshotBuildOutput {
                    key: ReadSnapshotKey {
                        archive_digest: [0; 32],
                        live_db_digest: [0; 32],
                    },
                    cheap_key: read_snapshot_cheap_key(&slot).expect("cheap key"),
                    decision: ArchiveReadDecision::Live,
                    strong_validated_at: Instant::now(),
                }),
            );
            assert!(
                slot.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .ready
                    .is_none(),
                "a build claimed before writer admission must not publish during the write epoch"
            );
            assert!(matches!(
                try_claim_read_snapshot_build(&slot, Instant::now() + Duration::from_secs(1)),
                Err(ReadSnapshotAcquireError::Busy(_))
            ));
            let cx = asupersync::Cx::for_testing();
            let conn = match run_async(write_pool.acquire(&cx)) {
                Outcome::Ok(conn) => conn,
                Outcome::Err(err) => panic!("live write pool acquire failed: {err}"),
                Outcome::Cancelled(_) => panic!("live write pool acquire cancelled"),
                Outcome::Panicked(panic) => {
                    panic!("live write pool acquire panicked: {}", panic.message())
                }
            };
            conn.query_sync(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('written', '/written', 0)",
                &[],
            )
            .expect("durable live write");
            drop(conn);
            drop(write_pool);
            assert_eq!(
                slot.invalidation_epoch
                    .load(std::sync::atomic::Ordering::Acquire),
                4,
                "each nested writer entry and exit must advance the exact scope epoch"
            );
            reset_read_snapshot_cache();
        }

        #[test]
        fn expired_snapshot_detects_checkpointed_same_length_live_write() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_snapshot_cache();
            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            write_inventory_message(&storage_root, 1, 1);
            commit_archive_tree(&repo, "seed archive");
            let sqlite_path = temp.path().join("mailbox.sqlite3");
            mcp_agent_mail_db::reconstruct_from_archive(&sqlite_path, &storage_root)
                .expect("seed live mailbox");

            let cx = asupersync::Cx::for_testing();
            let first = run_async(get_or_build_archive_read_snapshot(
                &storage_root,
                &sqlite_path,
                &cx,
            ))
            .expect("initial snapshot");
            let first_pointer = Arc::as_ptr(&first) as usize;
            let live = mcp_agent_mail_db::DbConn::open_file(sqlite_path.to_string_lossy().as_ref())
                .expect("open live mailbox");
            live.query_sync("UPDATE messages SET subject = 'Mutate' WHERE id = 1", &[])
                .expect("same-length live update");
            drop(live);
            mcp_agent_mail_db::pool::wal_checkpoint_truncate_path(&sqlite_path)
                .expect("checkpoint same-length write");

            expire_read_snapshot_exact_audit();
            let refreshed =
                wait_for_snapshot_pointer_change(&storage_root, &sqlite_path, first_pointer);
            let refreshed_pool = refreshed.pool();
            let refreshed_conn = match run_async(refreshed_pool.acquire(&cx)) {
                Outcome::Ok(conn) => conn,
                Outcome::Err(err) => panic!("refreshed snapshot acquire failed: {err}"),
                Outcome::Cancelled(_) => panic!("refreshed snapshot acquire cancelled"),
                Outcome::Panicked(panic) => {
                    panic!("refreshed snapshot acquire panicked: {}", panic.message())
                }
            };
            let subject = refreshed_conn
                .query_sync("SELECT subject FROM messages WHERE id = 1", &[])
                .expect("query refreshed snapshot")[0]
                .get_named::<String>("subject")
                .expect("refreshed subject");
            assert_eq!(subject, "Mutate");
            drop(refreshed_conn);
            drop(refreshed_pool);
            drop(refreshed);
            drop(first);
            reset_read_snapshot_cache();
        }

        #[test]
        fn snapshot_acquisition_errors_have_exact_retry_envelopes() {
            let busy = read_snapshot_acquire_error_to_mcp_error(ReadSnapshotAcquireError::Busy(
                "snapshot worker unavailable".to_string(),
            ));
            let busy_data = busy.data.expect("busy envelope");
            assert_eq!(busy_data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(busy_data["error"]["recoverable"], true);

            let timeout = read_snapshot_acquire_error_to_mcp_error(
                ReadSnapshotAcquireError::TimedOut("cold bootstrap timed out".to_string()),
            );
            let timeout_data = timeout.data.expect("timeout envelope");
            assert_eq!(timeout_data["error"]["type"], "SNAPSHOT_TIMEOUT");
            assert_eq!(timeout_data["error"]["recoverable"], true);
            assert_eq!(
                timeout_data["error"]["data"]["timeout_seconds"],
                READ_SNAPSHOT_BUILD_TIMEOUT.as_secs()
            );
        }

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
            assert_eq!(
                data["error"]["data"]["db_error_classification"]["class"],
                "pool_exhaustion"
            );
        }

        #[test]
        fn open_read_db_pool_ignores_unrelated_default_archive_overlap() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
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

                    let pool = run_async(open_read_db_pool()).expect("open read db pool");
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
        fn db_error_to_mcp_error_corruption_mapping_is_pure_with_live_pool() {
            let _guard = READ_SNAPSHOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("live.sqlite3");
            let database_url = format!("sqlite:///{}", db_path.display());
            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("DATABASE_URL", database_url.as_str())],
                || {
                    Config::reset_cached();
                    let _pool = get_db_pool().expect("live pool");

                    let err = db_error_to_mcp_error(DbError::Schema(
                        "database disk image is malformed".into(),
                    ));
                    let data = err.data.expect("expected data payload");
                    assert_eq!(data["error"]["type"], "DATABASE_CORRUPTION");
                    assert_eq!(data["error"]["recoverable"], false);
                },
            );
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
        fn db_error_to_mcp_error_maps_fd_exhaustion_as_resource_busy() {
            // D3 (br-bvq1x.4.3): "Freed 0 cached repos" means eviction freed
            // nothing, so the envelope must stop advising a blind retry.
            let err = db_error_to_mcp_error(DbError::Internal(
                "send_message retry failed: Too many open files. Freed 0 cached repos".into(),
            ));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(data["error"]["recoverable"], false);
            assert_eq!(data["error"]["data"]["resource_class"], "file_descriptors");
            assert_eq!(data["error"]["data"]["eviction_freed"], 0);
            let msg = data["error"]["message"].as_str().unwrap();
            assert!(msg.contains("File descriptor limit exhausted"));
            assert!(msg.contains("Do NOT retry"), "non-retry guidance: {msg}");
            let classification = &data["error"]["data"]["db_error_classification"];
            assert_eq!(classification["class"], "fd_exhaustion");
            assert_eq!(classification["safe_to_retry"], true);
            assert_eq!(classification["blocks_edits"], true);
            let fd = &data["error"]["data"]["failure_envelope"]["fd_pressure"];
            assert_eq!(fd["eviction_freed"], 0);
            assert_eq!(fd["immediate_retry_useful"], false);
        }

        #[test]
        fn db_error_to_mcp_error_fd_exhaustion_without_freed_zero_keeps_retry_advice() {
            let err = db_error_to_mcp_error(DbError::Internal(
                "open failed: Too many open files (os error 24)".into(),
            ));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(data["error"]["recoverable"], true);
            assert_eq!(data["error"]["data"]["eviction_freed"], Value::Null);
            let msg = data["error"]["message"].as_str().unwrap();
            assert!(msg.contains("then retry"), "retry advice retained: {msg}");
        }

        #[test]
        fn db_error_to_mcp_error_does_not_map_bad_fd_as_exhaustion() {
            let err = db_error_to_mcp_error(DbError::Internal("bad file descriptor".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "UNHANDLED_EXCEPTION");
            assert_eq!(data["error"]["recoverable"], false);
        }

        #[test]
        fn db_error_to_mcp_error_maps_schema() {
            let err = db_error_to_mcp_error(DbError::Schema("no such table: messages".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_ERROR");
            assert_eq!(
                data["error"]["data"]["db_error_classification"]["class"],
                "schema_drift_or_missing_tables"
            );
            assert_eq!(
                data["error"]["data"]["db_error_classification"]["recommended_command"],
                "am doctor migrate --check"
            );
        }

        #[test]
        fn db_error_to_mcp_error_keeps_raw_schema_corruption_as_schema_drift() {
            let err = db_error_to_mcp_error(DbError::Schema(
                "malformed database schema (idx_agent_links_pair_unique) - invalid rootpage (11)"
                    .into(),
            ));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_ERROR");
            assert_eq!(data["error"]["recoverable"], true);
            assert_eq!(
                data["error"]["data"]["db_error_classification"]["class"],
                "schema_drift_or_missing_tables"
            );
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
        fn db_error_to_mcp_error_records_corruption_class_metric() {
            // A5 (br-bvq1x.1.5): the surfacing chokepoint must feed the
            // corruption-class counter. Use a delta (>= before + 1) so the
            // assertion is correct even under concurrent test execution that
            // shares the process-global metrics singleton.
            let counter = &mcp_agent_mail_core::global_metrics()
                .corruption
                .class_main_db_btree_corruption_total;
            let before = counter.load();
            let _ =
                db_error_to_mcp_error(DbError::Sqlite("database disk image is malformed".into()));
            assert!(
                counter.load() > before,
                "main_db_btree_corruption counter should increment on a classified corruption error"
            );
        }

        #[test]
        fn db_error_to_mcp_error_keeps_fts_integrity_repairable() {
            let err = db_error_to_mcp_error(DbError::IntegrityCorruption {
                message: "integrity failed".into(),
                details: vec!["fts5 search index malformed".into()],
            });
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_ERROR");
            assert_eq!(data["error"]["recoverable"], true);
            let classification = &data["error"]["data"]["db_error_classification"];
            assert_eq!(classification["class"], "fts_index_corruption");
            assert_eq!(classification["safe_to_continue_read_only"], true);
            assert_eq!(classification["blocks_edits"], false);
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
        fn db_error_to_mcp_error_includes_structured_failure_envelope() {
            let err = db_error_to_mcp_error(DbError::ResourceBusy("database is locked".into()));
            let data = err.data.expect("expected data payload");
            let envelope = &data["error"]["data"]["failure_envelope"];
            assert_eq!(
                envelope["schema_version"],
                mcp_agent_mail_db::DB_FAILURE_ENVELOPE_SCHEMA_VERSION
            );
            assert_eq!(envelope["class"], "busy_retryable");
            assert_eq!(envelope["severity"], "P2");
            assert_eq!(envelope["error_code"], "RESOURCE_BUSY");
            assert_eq!(envelope["policy"]["safe_to_retry"], true);
            assert_eq!(envelope["policy"]["blocks_edits"], true);
            assert_eq!(envelope["wal_mode"]["status"], "not_collected");
            assert_eq!(
                envelope["frankensqlite_probe"]["status"],
                "classified_from_error"
            );
            assert!(envelope["process"]["pid"].as_u64().is_some());
            assert!(envelope["sidecars"]["wal"].get("exists").is_some());
            assert_eq!(
                data["error"]["data"]["db_error_classification"]["class"],
                envelope["class"]
            );
        }

        #[test]
        fn db_error_to_mcp_error_maps_mailbox_owner_resource_busy_with_actionable_detail() {
            let detail = "mailbox activity lock is busy for storage root /tmp/mailbox \
                (exclusive lock /tmp/mailbox/.mailbox.activity.lock): another Agent Mail runtime \
                is already active; owner hint: pid=17 mode=exclusive";
            let err = db_error_to_mcp_error(DbError::ResourceBusy(detail.into()));
            let data = err.data.expect("expected data payload");
            let message = data["error"]["message"].as_str().unwrap();
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert!(message.contains("Route this operation through that server"));
            assert!(message.contains("mailbox activity lock is busy"));
            assert!(message.contains("pid=17"));
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
            assert_eq!(
                data["error"]["data"]["corruption_details"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
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
    (
        "check_file_reservation_conflicts",
        clusters::FILE_RESERVATIONS,
    ),
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
            tool_cluster("check_file_reservation_conflicts"),
            Some(clusters::FILE_RESERVATIONS)
        );
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
