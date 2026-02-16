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

pub mod tool_util {
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
        let a = a.to_ascii_lowercase();
        let b = b.to_ascii_lowercase();
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
                curr[j] = if a_bytes[i - 1] == b_bytes[j - 1] {
                    prev[j - 1] + 1
                } else {
                    prev[j].max(curr[j - 1])
                };
            }
            std::mem::swap(&mut prev, &mut curr);
            curr.fill(0);
        }
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
        suggestions.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
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
        if !raw_identifier.starts_with('/') {
            if let Some(cached) = mcp_agent_mail_db::read_cache().get_project(raw_identifier) {
                return Ok(cached);
            }
        }
        let out = if raw_identifier.starts_with('/') {
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
        suggestions.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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

        let name = agent_name.trim();

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
    let left = mcp_agent_mail_core::pattern_overlap::CompiledPattern::new(left);
    let right = mcp_agent_mail_core::pattern_overlap::CompiledPattern::new(right);
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
