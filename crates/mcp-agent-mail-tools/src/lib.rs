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
#![allow(clippy::needless_pass_by_value)]

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

pub(crate) mod tool_util {
    use fastmcp::McpErrorCode;
    use fastmcp::prelude::*;
    use mcp_agent_mail_db::{DbError, DbPool, DbPoolConfig, get_or_create_pool};
    use serde_json::json;

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

    #[allow(clippy::too_many_lines)]
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
            DbError::Pool(message) => legacy_tool_error(
                "DATABASE_POOL_EXHAUSTED",
                "Database connection pool exhausted. Reduce concurrency or increase pool settings.",
                true,
                json!({ "error_detail": message }),
            ),
            DbError::Sqlite(message) | DbError::Schema(message) => legacy_tool_error(
                "DATABASE_ERROR",
                format!("Database error: {message}"),
                true,
                json!({ "error_detail": message }),
            ),
            DbError::Serialization(message) => legacy_tool_error(
                "TYPE_ERROR",
                format!("Argument type mismatch: {message}."),
                true,
                json!({ "error_detail": message }),
            ),
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

    pub async fn resolve_project(
        ctx: &McpContext,
        pool: &DbPool,
        project_key: &str,
    ) -> McpResult<mcp_agent_mail_db::ProjectRow> {
        // Check read cache first (slug lookups only; ensure_project always hits DB)
        if !project_key.starts_with('/') {
            if let Some(cached) = mcp_agent_mail_db::read_cache().get_project(project_key) {
                return Ok(cached);
            }
        }
        let out = if project_key.starts_with('/') {
            mcp_agent_mail_db::queries::ensure_project(ctx.cx(), pool, project_key).await
        } else {
            mcp_agent_mail_db::queries::get_project_by_slug(ctx.cx(), pool, project_key).await
        };
        let project = db_outcome_to_mcp_result(out)?;
        // Populate cache on miss
        mcp_agent_mail_db::read_cache().put_project(&project);
        Ok(project)
    }

    pub async fn resolve_agent(
        ctx: &McpContext,
        pool: &DbPool,
        project_id: i64,
        agent_name: &str,
    ) -> McpResult<mcp_agent_mail_db::AgentRow> {
        // Check read cache first
        if let Some(cached) = mcp_agent_mail_db::read_cache().get_agent(project_id, agent_name) {
            return Ok(cached);
        }
        let out =
            mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, agent_name).await;
        let agent = db_outcome_to_mcp_result(out)?;
        // Populate cache on miss
        mcp_agent_mail_db::read_cache().put_agent(&agent);
        Ok(agent)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

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
        fn db_error_to_mcp_error_maps_schema() {
            let err = db_error_to_mcp_error(DbError::Schema("migration v4 failed".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_ERROR");
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
    }
}

/// Fast glob overlap checks used by messaging + file reservations.
///
/// This preserves the legacy heuristic:
/// - Normalize both patterns
/// - If BOTH compile as globs, check `A matches B` OR `B matches A`
/// - Otherwise, only exact normalized equality is considered overlapping
pub(crate) mod pattern_overlap {
    use globset::{Glob, GlobMatcher};

    fn normalize_pattern(pattern: &str) -> String {
        let mut normalized = pattern.trim().replace('\\', "/");
        while normalized.starts_with("./") {
            normalized = normalized[2..].to_string();
        }
        normalized.trim_start_matches('/').to_string()
    }

    #[derive(Debug, Clone)]
    pub struct CompiledPattern {
        norm: String,
        matcher: Option<GlobMatcher>,
    }

    /// Returns `true` if the string contains glob metacharacters (`*`, `?`, `[`, `{`).
    pub fn has_glob_meta(s: &str) -> bool {
        s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
    }

    impl CompiledPattern {
        pub fn new(raw: &str) -> Self {
            let norm = normalize_pattern(raw);
            let matcher = Glob::new(&norm).ok().map(|g| g.compile_matcher());
            Self { norm, matcher }
        }

        /// Returns the normalized pattern string.
        pub fn normalized(&self) -> &str {
            &self.norm
        }

        /// Returns `true` if the normalized pattern contains glob metacharacters.
        pub fn is_glob(&self) -> bool {
            has_glob_meta(&self.norm)
        }

        /// Returns the first path segment if it doesn't contain glob chars.
        ///
        /// E.g. `"src/api/*.rs"` → `Some("src")`, `"*.rs"` → `None`.
        pub fn first_literal_segment(&self) -> Option<&str> {
            let seg = self.norm.split('/').next().unwrap_or("");
            if seg.is_empty() || has_glob_meta(seg) {
                None
            } else {
                Some(seg)
            }
        }

        /// Returns `true` if the glob matcher matches the given path string.
        ///
        /// Returns `false` if the pattern couldn't be compiled.
        pub fn matches(&self, path: &str) -> bool {
            self.matcher.as_ref().is_some_and(|m| m.is_match(path))
        }

        pub fn overlaps(&self, other: &Self) -> bool {
            if self.norm == other.norm {
                return true;
            }

            match (&self.matcher, &other.matcher) {
                (Some(a), Some(b)) => a.is_match(&other.norm) || b.is_match(&self.norm),
                _ => false,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::CompiledPattern;

        #[test]
        fn overlaps_is_symmetric_for_equal_norms() {
            let a = CompiledPattern::new("./src/**");
            let b = CompiledPattern::new("src/**");
            assert!(a.overlaps(&b));
            assert!(b.overlaps(&a));
        }

        #[test]
        fn overlaps_falls_back_to_equality_if_any_glob_invalid() {
            // Glob with an unclosed character class should fail to compile.
            // In that case we must not attempt matching: only equality counts.
            let invalid = CompiledPattern::new("[abc");
            let other = CompiledPattern::new("abc");
            assert!(!invalid.overlaps(&other));
            assert!(!other.overlaps(&invalid));

            let invalid_same = CompiledPattern::new(" [abc ");
            assert!(invalid.overlaps(&invalid_same));
        }
    }
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
