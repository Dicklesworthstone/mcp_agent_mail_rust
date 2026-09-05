//! MCP tools and resources implementation for MCP Agent Mail
//!
//! This crate provides implementations for all 39 MCP tools:
//! - Infrastructure cluster (4 tools)
//! - Identity cluster (6 tools)
//! - Messaging cluster (6 tools)
//! - Contact cluster (4 tools)
//! - File reservation cluster (4 tools)
//! - Search cluster (2 tools)
//! - Workflow macro cluster (4 tools)
//! - Product bus cluster (5 tools)
//! - Build slot cluster (3 tools)
//!
//! And 25 MCP resources for read-only data access.

#![forbid(unsafe_code)]
#![recursion_limit = "256"]
#![allow(
    clippy::needless_pass_by_value,
    clippy::needless_borrows_for_generic_args,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::needless_borrow,
    clippy::manual_ignore_case_cmp
)]

mod archive_read;

pub mod build_slots;
pub mod contacts;
pub mod degraded_intents;
pub mod idempotency;
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
    LatencySnapshot, MetricsSnapshotEntry, RECENT_TOOL_CALL_CAPACITY, RecentToolCall,
    RecentToolCallOutcome, recent_call_count, recent_calls_since, record_call, record_call_idx,
    record_error, record_error_idx, record_latency, record_latency_idx, record_recent_call,
    record_rejection, record_rejection_idx, reset_tool_latencies, reset_tool_metrics, slow_tools,
    tool_index, tool_meta, tool_metrics_snapshot, tool_metrics_snapshot_full,
};
pub use products::*;
pub use reservation_parity::*;
pub use reservations::*;
pub use resources::*;
pub use search::*;
pub use tool_util::{mcp_error_is_client_refusal, tool_error_code, tool_error_is_client_refusal};

pub mod tool_util {
    use fastmcp::McpErrorCode;
    use fastmcp::prelude::*;
    use mcp_agent_mail_core::Config;
    use mcp_agent_mail_db::{DbError, DbPool, DbPoolConfig, get_or_create_pool};
    use serde_json::{Map, Value, json};
    use std::collections::{BTreeSet, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};

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

    /// Extract the legacy error code from a tool [`McpError`].
    ///
    /// Reads `data.error.type` (e.g. `"INVALID_ARGUMENT"`) as built via
    /// [`legacy_tool_error`] / [`legacy_mcp_error`]. Returns `None` for errors
    /// without a legacy payload (framework internals, panics).
    #[must_use]
    pub fn tool_error_code(err: &McpError) -> Option<&str> {
        err.data.as_ref()?.get("error")?.get("type")?.as_str()
    }

    /// Classify a legacy tool error CODE as a pure client-side refusal.
    ///
    /// Refusals are invalid input, a not-found lookup, a policy/contact
    /// refusal, a feature-disabled refusal, an idempotency conflict, a
    /// cursor-window miss, or an auth (token/proof) validation failure — as
    /// opposed to a server fault (DB errors, `RESOURCE_BUSY`, timeouts,
    /// `ARCHIVE_ERROR`, `DISK_FULL`, `DURABILITY_DEGRADED`, internal panics).
    ///
    /// Unknown or unparsable codes classify as server faults, failing toward
    /// visibility (br-315wc).
    #[must_use]
    pub fn tool_error_is_client_refusal(code: &str) -> bool {
        // Availability failures that merely wear an auth-family prefix are
        // server faults, so exclude them before the prefix rules.
        if code == "PROOF_NONCE_STORE_UNAVAILABLE" || code == "SENDER_TOKEN_UNAVAILABLE" {
            return false;
        }
        // Families: input validation and auth/proof validation refusals.
        if code.starts_with("INVALID_") || code.starts_with("EMPTY_") || code.starts_with("PROOF_")
        {
            return true;
        }
        // Not-found lookups (NOT_FOUND, RECIPIENT_NOT_FOUND, AGENT_NOT_FOUND,
        // IDENTITY_NOT_FOUND, ...).
        if code == "NOT_FOUND" || code.ends_with("_NOT_FOUND") {
            return true;
        }
        matches!(
            code,
            // Input validation refusals outside the prefix families.
            "MISSING_FIELD"
                | "MISSING_PANE_ID"
                | "TYPE_ERROR"
                | "TOO_MANY_PATHS"
                | "PATH_TOO_LONG"
                | "PAYLOAD_TOO_LARGE"
                // Policy / contact refusals.
                | "CONTACT_REQUIRED"
                | "CONTACT_BLOCKED"
                | "CONTACTS_ONLY"
                // Feature-disabled refusals.
                | "BROADCAST_DISABLED"
                | "FEATURE_DISABLED"
                | "WORKTREES_DISABLED"
                // Advisory conflicts the client is expected to resolve.
                | "FILE_RESERVATION_CONFLICT"
                | "IDEMPOTENCY_KEY_CONFLICT"
                // Cursor-window misses.
                | "CURSOR_EXPIRED"
                | "CURSOR_AHEAD"
                // Auth/token validation failures.
                | "SENDER_TOKEN_REQUIRED"
                | "SENDER_TOKEN_MISMATCH"
        )
    }

    /// Whole-error variant of [`tool_error_is_client_refusal`]: `true` only
    /// when the error carries a legacy code that classifies as a client
    /// refusal. Errors without an extractable code classify as server faults.
    #[must_use]
    pub fn mcp_error_is_client_refusal(err: &McpError) -> bool {
        tool_error_code(err).is_some_and(tool_error_is_client_refusal)
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
            DbError::InvalidArgument {
                field: "agent_retired",
                message,
            } => legacy_tool_error(
                "AGENT_RETIRED",
                message.clone(),
                true,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "state": "retired",
                        "error_detail": message,
                    }),
                ),
            ),
            DbError::InvalidArgument {
                field: "agent_deregistered",
                message,
            } => legacy_tool_error(
                "AGENT_DEREGISTERED",
                message.clone(),
                false,
                db_error_data(
                    classification,
                    &failure_envelope,
                    json!({
                        "state": "deregistered",
                        "error_detail": message,
                    }),
                ),
            ),
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

    /// Live database lease for mutation paths. The guard brackets pool
    /// bootstrap and the caller's entire operation, invalidating archive-read
    /// decisions on both entry and exit.
    pub struct WriteDbPool {
        pool: DbPool,
        _guard: crate::archive_read::WriteGuard,
    }

    impl std::ops::Deref for WriteDbPool {
        type Target = DbPool;

        fn deref(&self) -> &Self::Target {
            &self.pool
        }
    }

    fn bootstrap_db_pool_with_barrier_handoff(
        cfg: &DbPoolConfig,
        storage_root: &Path,
        sqlite_path: Option<&Path>,
    ) -> McpResult<(DbPool, crate::archive_read::WriteGuard)> {
        let (barrier, outcome) =
            mcp_agent_mail_db::write_barrier::acquire_promotion_barrier_draining(
                mcp_agent_mail_db::write_barrier::writer_drain_timeout(),
            );
        if let mcp_agent_mail_db::write_barrier::DrainOutcome::TimedOut { remaining_writers } =
            outcome
        {
            drop(barrier);
            return Err(McpError::internal_error(format!(
                "database pool bootstrap deferred because {remaining_writers} writer(s) did not drain before the recovery barrier timeout"
            )));
        }
        let pool =
            get_or_create_pool(cfg).map_err(|error| McpError::internal_error(error.to_string()))?;
        // Atomic handoff: register the caller as a writer while the promotion
        // barrier is still closed, then release the barrier. No promotion can
        // enter between cold bootstrap/recovery and the operation using this
        // pool.
        let guard = crate::archive_read::WriteGuard::begin(storage_root, sqlite_path);
        drop(barrier);
        Ok((pool, guard))
    }

    pub fn get_db_pool() -> McpResult<WriteDbPool> {
        let cfg = DbPoolConfig::from_env();
        let sqlite_path =
            if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&cfg.database_url) {
                None
            } else {
                Some(
                    mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(&cfg.database_url)
                        .map_err(|error| McpError::internal_error(error.to_string()))?
                        .canonical_path,
                )
            };
        let storage_root = cfg
            .storage_root
            .clone()
            .unwrap_or_else(|| Config::from_env().storage_root);
        let guard = crate::archive_read::WriteGuard::begin(
            &storage_root,
            sqlite_path.as_deref().map(Path::new),
        );
        if let Some(pool) = mcp_agent_mail_db::get_cached_pool(&cfg) {
            return Ok(WriteDbPool {
                pool,
                _guard: guard,
            });
        }
        // A cold pool may migrate or recover, which requires the promotion
        // barrier. Relinquish this provisional writer first, then perform an
        // explicit barrier-to-writer handoff in the helper.
        drop(guard);
        let (pool, guard) = bootstrap_db_pool_with_barrier_handoff(
            &cfg,
            &storage_root,
            sqlite_path.as_deref().map(Path::new),
        )?;
        Ok(WriteDbPool {
            pool,
            _guard: guard,
        })
    }

    /// Open the live mailbox for a read surface without migrations, recovery,
    /// reconciliation, directory creation, or writer-generation changes.
    pub(crate) fn get_live_read_db_pool() -> McpResult<DbPool> {
        let mut cfg = DbPoolConfig::from_env();
        if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&cfg.database_url) {
            return get_or_create_pool(&cfg)
                .map_err(|error| McpError::internal_error(error.to_string()));
        }
        cfg.run_migrations = false;
        cfg.warmup_connections = 0;
        mcp_agent_mail_db::create_query_only_pool(&cfg)
            .map_err(|error| McpError::internal_error(error.to_string()))
    }

    /// Open the live mailbox through the query-only lane used by request-path
    /// reads that must not wait for archive reconciliation or the write-behind
    /// coalescer.  The caller receives only an existing SQLite connection:
    /// this path never starts migrations, recovery, reconciliation, or a
    /// writer-generation transition.
    pub(crate) fn get_coalescer_bypass_read_db_pool() -> McpResult<DbPool> {
        get_live_read_db_pool()
    }

    /// Reuse a hot authoritative live pool for health reads without advancing
    /// the writer generation. An initialized mailbox whose pool has merely
    /// been dropped (the pool cache holds weak references) opens through the
    /// zero-footprint query-only path: no migrations, recovery, warmup,
    /// sqlite-family writes, or writer-generation changes (br-lg5dd). Only a
    /// genuinely cold bootstrap — missing/empty sqlite file, or a memory
    /// database with no live pool — remains bracketed because it may
    /// initialize, migrate, or recover the mailbox.
    pub(crate) fn get_authoritative_live_db_pool() -> McpResult<DbPool> {
        let cfg = DbPoolConfig::from_env();
        if let Some(pool) = mcp_agent_mail_db::get_cached_pool(&cfg) {
            return Ok(pool);
        }
        let sqlite_path =
            if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&cfg.database_url) {
                None
            } else {
                Some(
                    mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(&cfg.database_url)
                        .map_err(|error| McpError::internal_error(error.to_string()))?
                        .canonical_path,
                )
            };
        let mailbox_initialized = sqlite_path
            .as_deref()
            .is_some_and(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0));
        if mailbox_initialized {
            return get_live_read_db_pool();
        }
        let storage_root = cfg
            .storage_root
            .clone()
            .unwrap_or_else(|| Config::from_env().storage_root);
        let (pool, guard) = bootstrap_db_pool_with_barrier_handoff(
            &cfg,
            &storage_root,
            sqlite_path.as_deref().map(Path::new),
        )?;
        drop(guard);
        Ok(pool)
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
    enum ReadArchiveSignatureState {
        /// Persisted cross-process epoch protocol (GH#235): O(1) in the
        /// archive tree size. Any mutation window — in this process or any
        /// other — rewrites the on-disk token, changing the stamp.
        Epoch(mcp_agent_mail_storage::ArchiveEpochReadStamp),
        /// Legacy gate for archives without an epoch token (mixed versions):
        /// `HEAD` with a fully clean `projects/` tree, established by a full
        /// `statuses()` walk.
        CleanCommit { head: git2::Oid },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadArchiveSignature {
        storage_root: PathBuf,
        state: ReadArchiveSignatureState,
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

    /// Return a cacheable archive signature.
    ///
    /// When the storage crate's persisted per-repo epoch token (GH#235) is
    /// present, the signature is the O(1) epoch stamp — `HEAD`, Git index
    /// stamp, and the on-disk token every mutation window (in any process)
    /// rewrites on both edges — and NO working-tree walk happens. When the
    /// token file is missing or unparsable (an archive last written by an
    /// older version), fall back to the legacy gate: cacheable only when
    /// `projects/` is fully represented by one stable commit, established by a
    /// full `statuses()` walk, because archive writes are durable in the
    /// worktree before the asynchronous Git coalescer advances `HEAD`.
    fn clean_read_archive_signature(storage_root: &Path) -> Option<ReadArchiveSignature> {
        let canonical_root = storage_root.canonicalize().ok()?;
        let repo = git2::Repository::open(&canonical_root).ok()?;
        let canonical_workdir = repo.workdir()?.canonicalize().ok()?;
        if canonical_workdir != canonical_root {
            return None;
        }

        match mcp_agent_mail_storage::archive_epoch_read_stamp(&canonical_root) {
            Ok(Some(stamp)) => {
                return Some(ReadArchiveSignature {
                    storage_root: canonical_root,
                    state: ReadArchiveSignatureState::Epoch(stamp),
                });
            }
            // No epoch token on disk: mixed-version archive — use the legacy
            // full-scan gate below.
            Ok(None) => {}
            // Contention (active mutation window, index.lock, token rewritten
            // mid-sample): scan without caching.
            Err(_) => return None,
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
            state: ReadArchiveSignatureState::CleanCommit { head: head_after },
        })
    }

    fn scan_read_archive_inventory(
        storage_root: &Path,
    ) -> mcp_agent_mail_db::ArchiveMessageInventory {
        #[cfg(test)]
        READ_ARCHIVE_INVENTORY_SCAN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        mcp_agent_mail_db::scan_archive_message_inventory(storage_root)
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

    #[cfg(test)]
    fn reset_read_archive_inventory_cache() {
        READ_ARCHIVE_INVENTORY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        READ_ARCHIVE_INVENTORY_SCAN_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    fn read_archive_inventory_scan_count() -> usize {
        READ_ARCHIVE_INVENTORY_SCAN_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    fn read_archive_inventory_cache_len() -> usize {
        READ_ARCHIVE_INVENTORY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn query_read_db_inventory(
        conn: &impl mcp_agent_mail_db::pool::SyncQuery,
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

    pub(crate) const fn read_archive_inventory_has_state(
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

    pub(crate) fn read_archive_is_ahead(
        storage_root: &Path,
        sqlite_path: &Path,
        conn: &impl mcp_agent_mail_db::pool::SyncQuery,
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
        snapshot: Option<Arc<crate::archive_read::SharedSnapshot>>,
    }

    impl ToolReadPool {
        pub(crate) const fn live(pool: mcp_agent_mail_db::DbPool) -> Self {
            Self {
                pool,
                snapshot: None,
            }
        }

        fn snapshot(snapshot: Arc<crate::archive_read::SharedSnapshot>) -> Self {
            Self {
                pool: snapshot.pool(),
                snapshot: Some(snapshot),
            }
        }

        /// Snapshot-local IDs cannot authorize writes to either database.
        pub(crate) fn live_sqlite_path(&self) -> Option<&str> {
            self.snapshot.is_none().then(|| self.pool.sqlite_path())
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
    pub(crate) fn live_db_is_suspect(
        database_url: &str,
        storage_root: &Path,
        sqlite_path: &Path,
    ) -> bool {
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

    fn open_read_db_pool(
        cx: &asupersync::Cx,
    ) -> Result<Option<ToolReadPool>, crate::archive_read::AcquireError> {
        if cx.is_cancel_requested() {
            return Err(crate::archive_read::AcquireError::Cancelled);
        }
        let config = Config::from_env();
        if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url) {
            return Ok(None);
        }

        let sqlite_path =
            mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(&config.database_url)
                .map_err(|error| crate::archive_read::AcquireError::Failed(error.to_string()))?
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
        crate::archive_read::acquire_if_needed(
            &config.storage_root,
            &resolved_path,
            &config.database_url,
            cx,
        )
        .map(|snapshot| snapshot.map(ToolReadPool::snapshot))
    }

    pub async fn get_read_db_pool(cx: &asupersync::Cx) -> McpResult<ToolReadPool> {
        match open_read_db_pool(cx) {
            Ok(Some(pool)) => Ok(pool),
            Ok(None) => get_live_read_db_pool().map(ToolReadPool::live),
            Err(crate::archive_read::AcquireError::Cancelled) => Err(McpError::request_cancelled()),
            Err(crate::archive_read::AcquireError::Busy(message)) => {
                Err(db_error_to_mcp_error(DbError::ResourceBusy(message)))
            }
            Err(crate::archive_read::AcquireError::TimedOut(message)) => Err(legacy_tool_error(
                "SNAPSHOT_TIMEOUT",
                message,
                true,
                json!({"timeout_seconds": 120}),
            )),
            Err(crate::archive_read::AcquireError::Failed(message)) => {
                Err(read_pool_setup_error_to_mcp_error(message))
            }
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
        // Real rows only: this runs on the NOT_FOUND path, where the orphan
        // placeholder augmentation's full anti-join scans would dominate the
        // error's latency and `[unknown-project-N]` names make no sense as
        // "did you mean" suggestions.
        let out = mcp_agent_mail_db::queries::list_project_rows(ctx.cx(), pool).await;
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
        resolve_project_with_mode(ctx, pool, project_key, true).await
    }

    /// Resolve a project that must already exist without allowing a read
    /// request to create it.
    ///
    /// This is the companion to the query-only read lane: absolute project keys
    /// are looked up by `human_key` rather than flowing through `ensure_project`.
    pub async fn resolve_existing_project(
        ctx: &McpContext,
        pool: &DbPool,
        project_key: &str,
    ) -> McpResult<mcp_agent_mail_db::ProjectRow> {
        resolve_project_with_mode(ctx, pool, project_key, false).await
    }

    #[allow(clippy::too_many_lines)]
    async fn resolve_project_with_mode(
        ctx: &McpContext,
        pool: &DbPool,
        project_key: &str,
        allow_create: bool,
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

        // Delegate to the queries layer, which is cache-first against the
        // *pool-scoped* cache. The previous code path consulted the unscoped
        // (`scope = ""`) cache here as a pre-check and wrote every resolved
        // project back to it. Unscoped entries are invisible to
        // `invalidate_scope` (which only ever runs with
        // "{identity}@{generation}" scopes), so a numeric `project.id` cached
        // here survived every pool retirement and recovery promotion for the
        // full cache TTL — and could even originate from an archive-snapshot
        // read pool whose reconstructed rowids never matched the live file.
        // Write tools then minted placeholder agents and agent_links against
        // the stale id (mcp_agent_mail_rust#219). Same split-brain class as
        // the agent-side fix for mcp_agent_mail_rust#106: always go through
        // the scoped path so the cache key matches the pool the SQL will
        // actually run against.
        let is_absolute = std::path::Path::new(raw_identifier).is_absolute();
        let out = if is_absolute && allow_create {
            mcp_agent_mail_db::queries::ensure_project(ctx.cx(), pool, raw_identifier).await
        } else if is_absolute {
            mcp_agent_mail_db::queries::get_project_by_human_key(ctx.cx(), pool, raw_identifier)
                .await
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
        use std::sync::{Mutex, OnceLock};

        static READ_POOL_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

        fn run_async<F, Fut, T>(f: F) -> T
        where
            F: FnOnce(asupersync::Cx) -> Fut,
            Fut: std::future::Future<Output = T>,
        {
            let cx = asupersync::Cx::for_testing();
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("build test runtime");
            runtime.block_on(f(cx))
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
        fn read_archive_inventory_cache_never_hides_uncommitted_archive_state() {
            let _guard = READ_POOL_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
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

        /// Seed the persisted per-repo mutation epoch token (GH#235) exactly
        /// as a storage-crate mutation window would leave it on disk.
        fn seed_epoch_token(storage_root: &Path, token_hex: &str) {
            std::fs::write(
                storage_root
                    .join(".git")
                    .join(mcp_agent_mail_storage::ARCHIVE_EPOCH_FILE_NAME),
                format!("1{token_hex}"),
            )
            .expect("write epoch token");
        }

        #[test]
        fn read_archive_inventory_epoch_token_rewrite_invalidates_cache() {
            let _guard = READ_POOL_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_archive_inventory_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            write_inventory_message(&storage_root, 1, 1);
            commit_archive_tree(&repo, "add first message");
            seed_epoch_token(&storage_root, &"aa".repeat(16));

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
                "a stable epoch token must reuse the cached inventory"
            );

            // Simulate a writer in ANOTHER process: only the token file
            // changes (no in-process mutation epoch bump). Two signature
            // samples straddling the rewrite must disagree, so the inventory
            // is never served from cache.
            let signature_before = clean_read_archive_signature(&storage_root)
                .expect("epoch-token signature must be available");
            seed_epoch_token(&storage_root, &"bb".repeat(16));
            let signature_after = clean_read_archive_signature(&storage_root)
                .expect("epoch-token signature must be available");
            assert_ne!(
                signature_before, signature_after,
                "an externally rewritten token must change the read signature"
            );

            assert_eq!(
                read_archive_inventory(&storage_root).latest_message_id,
                Some(1)
            );
            assert_eq!(
                read_archive_inventory_scan_count(),
                2,
                "a cross-process token rewrite must invalidate the cached inventory"
            );

            reset_read_archive_inventory_cache();
        }

        #[test]
        fn read_archive_inventory_epoch_token_replaces_statuses_walk_for_uncommitted_writes() {
            let _guard = READ_POOL_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_read_archive_inventory_cache();

            let temp = tempfile::tempdir().expect("tempdir");
            let storage_root = temp.path().join("archive");
            let repo = git2::Repository::init(&storage_root).expect("init archive repo");
            write_inventory_message(&storage_root, 1, 1);
            commit_archive_tree(&repo, "add first message");
            seed_epoch_token(&storage_root, &"cc".repeat(16));

            assert_eq!(
                read_archive_inventory(&storage_root).latest_message_id,
                Some(1)
            );
            assert_eq!(read_archive_inventory_scan_count(), 1);

            // A cross-process uncommitted archive write: durable in the
            // worktree, invisible to HEAD/index, signaled ONLY through the
            // token file — exactly what the statuses() walk used to catch.
            write_inventory_message(&storage_root, 2, 2);
            seed_epoch_token(&storage_root, &"dd".repeat(16));
            assert_eq!(read_archive_inventory(&storage_root).unique_message_ids, 2);
            assert_eq!(
                read_archive_inventory_scan_count(),
                2,
                "the token rewrite alone must surface the uncommitted write"
            );

            // With the token stable, even a dirty worktree is cacheable: the
            // token — not a working-tree walk — is the mutation authority.
            assert_eq!(read_archive_inventory(&storage_root).unique_message_ids, 2);
            assert_eq!(
                read_archive_inventory_scan_count(),
                2,
                "a dirty worktree with a stable token must reuse the cache"
            );

            reset_read_archive_inventory_cache();
        }

        #[test]
        fn read_archive_inventory_cache_is_bounded_and_scoped_by_canonical_root() {
            let _guard = READ_POOL_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
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
            assert_eq!(
                parse_attachment_metadata_json(""),
                [] as [serde_json::Value; 0]
            );
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
            let _guard = READ_POOL_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("custom.sqlite3");
            let database_url = format!("sqlite:///{}", db_path.display());

            // br-99aih: redirect the *default* storage root into a private
            // tempdir (HOME + XDG_DATA_HOME); an XDG-only override still
            // resolved to the operator's live archive on any host that had run
            // the daemon.
            mcp_agent_mail_core::config::with_isolated_default_storage_root_and_env_overrides_for_test(
                &[("DATABASE_URL", database_url.as_str())],
                |_isolated_default_root| {
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

                    run_async(|cx| async move {
                        let pool = open_read_db_pool(&cx).expect("open read db pool");
                        assert!(
                            pool.is_none(),
                            "default global archive should not force shared tool read snapshots for an external custom DB"
                        );
                    });
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
            let _guard = READ_POOL_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
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
    ("retire_agent", clusters::IDENTITY),
    ("unretire_agent", clusters::IDENTITY),
    ("deregister_agent", clusters::IDENTITY),
    ("whois", clusters::IDENTITY),
    ("resolve_pane_identity", clusters::IDENTITY),
    ("cleanup_pane_identities", clusters::IDENTITY),
    ("list_agents", clusters::IDENTITY),
    // Messaging
    ("send_message", clusters::MESSAGING),
    ("reply_message", clusters::MESSAGING),
    ("fetch_inbox", clusters::MESSAGING),
    ("fetch_topic", clusters::MESSAGING),
    ("fetch_inbox_events", clusters::MESSAGING),
    ("mark_message_read", clusters::MESSAGING),
    ("mark_all_read", clusters::MESSAGING),
    ("acknowledge_message", clusters::MESSAGING),
    ("get_message_delivery_receipt", clusters::MESSAGING),
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

    // -- error refusal classifier tests (br-315wc) --

    #[test]
    fn client_refusal_codes_classify_as_rejections() {
        for code in [
            "INVALID_ARGUMENT",
            "INVALID_LIMIT",
            "NOT_FOUND",
            "RECIPIENT_NOT_FOUND",
            "AGENT_NOT_FOUND",
            "IDENTITY_NOT_FOUND",
            "MISSING_FIELD",
            "EMPTY_PATHS",
            "CONTACT_REQUIRED",
            "CONTACT_BLOCKED",
            "BROADCAST_DISABLED",
            "FEATURE_DISABLED",
            "WORKTREES_DISABLED",
            "FILE_RESERVATION_CONFLICT",
            "IDEMPOTENCY_KEY_CONFLICT",
            "CURSOR_EXPIRED",
            "CURSOR_AHEAD",
            "SENDER_TOKEN_REQUIRED",
            "SENDER_TOKEN_MISMATCH",
            "PROOF_REQUIRED",
            "PROOF_EXPIRED",
            "PROOF_BAD_SIGNATURE",
        ] {
            assert!(
                tool_error_is_client_refusal(code),
                "{code} must classify as a client refusal"
            );
        }
    }

    #[test]
    fn server_fault_codes_stay_errors() {
        for code in [
            "DATABASE_ERROR",
            "DATABASE_CORRUPTION",
            "DATABASE_POOL_EXHAUSTED",
            "RESOURCE_BUSY",
            "ARCHIVE_ERROR",
            "DISK_FULL",
            "DURABILITY_DEGRADED",
            "UNHANDLED_EXCEPTION",
            "SNAPSHOT_TIMEOUT",
            "PROOF_NONCE_STORE_UNAVAILABLE",
            "SENDER_TOKEN_UNAVAILABLE",
            // Unknown/unparsable codes fail toward visibility.
            "SOME_FUTURE_CODE",
            "",
        ] {
            assert!(
                !tool_error_is_client_refusal(code),
                "{code} must stay classified as a server error"
            );
        }
    }

    #[test]
    fn tool_error_code_extracts_legacy_payload_type() {
        let err = tool_util::legacy_tool_error(
            "CONTACT_REQUIRED",
            "Contact required",
            true,
            serde_json::json!({}),
        );
        assert_eq!(tool_error_code(&err), Some("CONTACT_REQUIRED"));
        assert!(mcp_error_is_client_refusal(&err));

        let db_err = tool_util::legacy_tool_error(
            "DATABASE_ERROR",
            "db exploded",
            false,
            serde_json::json!({}),
        );
        assert_eq!(tool_error_code(&db_err), Some("DATABASE_ERROR"));
        assert!(!mcp_error_is_client_refusal(&db_err));

        // Errors without a legacy payload have no extractable code and are
        // never classified as client refusals.
        let bare =
            fastmcp::prelude::McpError::new(fastmcp::McpErrorCode::InternalError, "internal panic");
        assert_eq!(tool_error_code(&bare), None);
        assert!(!mcp_error_is_client_refusal(&bare));
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
