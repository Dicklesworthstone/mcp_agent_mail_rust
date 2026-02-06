//! Identity cluster tools
//!
//! Tools for project and agent identity management:
//! - `health_check`: Infrastructure status
//! - `ensure_project`: Create/ensure project exists
//! - `register_agent`: Register or update agent
//! - `create_agent_identity`: Create new agent identity
//! - whois: Agent profile lookup

use fastmcp::McpErrorCode;
use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::micros_to_iso;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

use crate::tool_util::{
    db_error_to_mcp_error, db_outcome_to_mcp_result, get_db_pool, legacy_tool_error,
    resolve_project,
};

fn redact_database_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some((_creds, host)) = rest.rsplit_once('@') {
            return format!("{scheme}://****@{host}");
        }
    }
    url.to_string()
}

/// Try to write an agent profile to the git archive. Failures are logged
/// but do not fail the tool call – the DB is the source of truth.
///
/// Uses the write-behind queue when available; falls back to synchronous
/// write if the queue is full.
fn try_write_agent_profile(config: &Config, project_slug: &str, agent_json: &serde_json::Value) {
    let op = mcp_agent_mail_storage::WriteOp::AgentProfile {
        project_slug: project_slug.to_string(),
        config: config.clone(),
        agent_json: agent_json.clone(),
    };
    if !mcp_agent_mail_storage::wbq_enqueue(op) {
        match mcp_agent_mail_storage::ensure_archive(config, project_slug) {
            Ok(archive) => {
                if let Err(e) = mcp_agent_mail_storage::write_agent_profile_with_config(
                    &archive, config, agent_json,
                ) {
                    tracing::warn!("Failed to write agent profile to archive: {e}");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to ensure archive for profile write: {e}");
            }
        }
    }
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResponse {
    pub status: String,
    pub environment: String,
    pub http_host: String,
    pub http_port: u16,
    pub database_url: String,
}

/// Project response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectResponse {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: String,
}

/// Project response with worktree identity metadata (when `WORKTREES_ENABLED=1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectWithIdentityResponse {
    pub id: i64,
    pub created_at: String,
    #[serde(flatten)]
    pub identity: mcp_agent_mail_core::ProjectIdentity,
}

/// Agent response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    pub inception_ts: String,
    pub last_active_ts: String,
    pub project_id: i64,
    pub attachments_policy: String,
}

/// Whois response with optional recent commits
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoisResponse {
    #[serde(flatten)]
    pub agent: AgentResponse,
    pub recent_commits: Vec<CommitInfo>,
}

/// Git commit information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub hexsha: String,
    pub summary: String,
    pub authored_ts: String,
}

/// Check infrastructure health and return configuration status.
///
/// Returns basic server configuration and status information.
#[tool(description = "Return basic readiness information for the Agent Mail server.")]
pub fn health_check(_ctx: &McpContext) -> McpResult<String> {
    let config = Config::from_env();

    let response = HealthCheckResponse {
        status: "ok".to_string(),
        environment: config.app_environment.to_string(),
        http_host: config.http_host,
        http_port: config.http_port,
        database_url: redact_database_url(&config.database_url),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Idempotently create or ensure a project exists.
///
/// # Parameters
/// - `human_key`: Absolute path to the project directory (REQUIRED)
/// - `identity_mode`: Optional override for project identity resolution
///
/// # Returns
/// Project descriptor with id, slug, `human_key`, `created_at`
#[tool(description = "Idempotently create or ensure a project exists for the given human key.")]
pub async fn ensure_project(
    ctx: &McpContext,
    human_key: String,
    identity_mode: Option<String>,
) -> McpResult<String> {
    if !Path::new(&human_key).is_absolute() {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid argument value: human_key must be an absolute directory path, got: '{human_key}'. \
Use the agent's working directory path (e.g., '/data/projects/backend' on Unix or 'C:\\\\projects\\\\backend' on Windows). \
Check that all parameters have valid values."
            ),
            true,
            json!({
                "field": "human_key",
                "error_detail": human_key,
            }),
        ));
    }

    let config = Config::from_env();
    let pool = get_db_pool()?;

    // Log identity_mode if provided (future: resolve project identity via git remotes, etc.)
    if let Some(mode) = identity_mode {
        tracing::debug!("ensure_project identity_mode={mode}");
    }

    let row = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::ensure_project(ctx.cx(), &pool, &human_key).await,
    )?;

    // Ensure the git archive directory exists for this project
    if let Err(e) = mcp_agent_mail_storage::ensure_archive(&config, &row.slug) {
        tracing::warn!("Failed to ensure archive for project '{}': {e}", row.slug);
    }

    if config.worktrees_enabled {
        let mut identity = mcp_agent_mail_core::resolve_project_identity(&human_key);
        identity.slug.clone_from(&row.slug); // Ensure follow-up calls using slug resolve correctly.

        let response = ProjectWithIdentityResponse {
            id: row.id.unwrap_or(0),
            created_at: micros_to_iso(row.created_at),
            identity,
        };

        return serde_json::to_string(&response)
            .map_err(|e| McpError::internal_error(format!("JSON error: {e}")));
    }

    let response = ProjectResponse {
        id: row.id.unwrap_or(0),
        slug: row.slug,
        human_key: row.human_key,
        created_at: micros_to_iso(row.created_at),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Register or update an agent identity within a project.
///
/// # Parameters
/// - `project_key`: Project human key or slug
/// - `program`: Agent program (e.g., "claude-code", "codex-cli")
/// - `model`: Model identifier (e.g., "opus-4.5", "gpt5-codex")
/// - `name`: Optional agent name (auto-generated if omitted)
/// - `task_description`: Optional current task description
/// - `attachments_policy`: Optional attachment handling policy
///
/// # Returns
/// Agent profile with all fields
#[tool(description = "Create or update an agent identity within a project.")]
pub async fn register_agent(
    ctx: &McpContext,
    project_key: String,
    program: String,
    model: String,
    name: Option<String>,
    task_description: Option<String>,
    attachments_policy: Option<String>,
) -> McpResult<String> {
    use mcp_agent_mail_core::models::{generate_agent_name, is_valid_agent_name};

    let pool = get_db_pool()?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Validate or generate agent name
    let agent_name = match name {
        Some(n) => {
            if !is_valid_agent_name(&n) {
                return Err(legacy_tool_error(
                    "INVALID_ARGUMENT",
                    format!(
                        "Invalid argument value: Invalid agent name '{n}'. \
Names must be adjective+noun format (e.g., BlueLake). \
Check that all parameters have valid values."
                    ),
                    true,
                    json!({
                        "field": "name",
                        "error_detail": n,
                    }),
                ));
            }
            n
        }
        None => generate_agent_name(),
    };

    // Validate attachments_policy if provided
    let policy = attachments_policy.unwrap_or_else(|| "auto".to_string());
    if !["auto", "inline", "file", "none"].contains(&policy.as_str()) {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid argument value: Invalid attachments_policy '{policy}'. \
Must be: auto, inline, file, or none. \
Check that all parameters have valid values."
            ),
            true,
            json!({
                "field": "attachments_policy",
                "error_detail": policy,
            }),
        ));
    }

    let agent_out = mcp_agent_mail_db::queries::register_agent(
        ctx.cx(),
        &pool,
        project_id,
        &agent_name,
        &program,
        &model,
        task_description.as_deref(),
        Some(&policy),
    )
    .await;

    let row = db_outcome_to_mcp_result(agent_out)?;

    // Write agent profile to git archive (best-effort)
    let config = Config::from_env();
    let agent_json = serde_json::json!({
        "name": row.name,
        "program": row.program,
        "model": row.model,
        "task_description": row.task_description,
        "inception_ts": micros_to_iso(row.inception_ts),
        "last_active_ts": micros_to_iso(row.last_active_ts),
        "attachments_policy": row.attachments_policy,
    });
    try_write_agent_profile(&config, &project.slug, &agent_json);

    let response = AgentResponse {
        id: row.id.unwrap_or(0),
        name: row.name,
        program: row.program,
        model: row.model,
        task_description: row.task_description,
        inception_ts: micros_to_iso(row.inception_ts),
        last_active_ts: micros_to_iso(row.last_active_ts),
        project_id: row.project_id,
        attachments_policy: row.attachments_policy,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Create a new, unique agent identity.
///
/// Always creates a new identity with a fresh unique name (never updates existing).
///
/// # Parameters
/// - `project_key`: Project human key or slug
/// - `program`: Agent program
/// - `model`: Model identifier
/// - `name_hint`: Optional name hint (must be valid adjective+noun if provided)
/// - `task_description`: Optional current task description
/// - `attachments_policy`: Optional attachment handling policy
///
/// # Returns
/// New agent profile
#[tool(description = "Create a new, unique agent identity and persist its profile to Git.")]
pub async fn create_agent_identity(
    ctx: &McpContext,
    project_key: String,
    program: String,
    model: String,
    name_hint: Option<String>,
    task_description: Option<String>,
    attachments_policy: Option<String>,
) -> McpResult<String> {
    use mcp_agent_mail_core::models::{generate_agent_name, is_valid_agent_name};

    let pool = get_db_pool()?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Generate or validate agent name
    let agent_name = match name_hint {
        Some(hint) => {
            if !is_valid_agent_name(&hint) {
                return Err(legacy_tool_error(
                    "INVALID_ARGUMENT",
                    format!(
                        "Invalid argument value: Invalid name_hint '{hint}'. \
Names must be adjective+noun format (e.g., BlueLake). \
Check that all parameters have valid values."
                    ),
                    true,
                    json!({
                        "field": "name_hint",
                        "error_detail": hint,
                    }),
                ));
            }
            hint
        }
        None => generate_agent_name(),
    };

    // Validate attachments_policy if provided
    let policy = attachments_policy.unwrap_or_else(|| "auto".to_string());
    if !["auto", "inline", "file", "none"].contains(&policy.as_str()) {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid argument value: Invalid attachments_policy '{policy}'. \
Must be: auto, inline, file, or none. \
Check that all parameters have valid values."
            ),
            true,
            json!({
                "field": "attachments_policy",
                "error_detail": policy,
            }),
        ));
    }

    // Enforce uniqueness: this tool must never update an existing identity.
    match mcp_agent_mail_db::queries::get_agent(ctx.cx(), &pool, project_id, &agent_name).await {
        Outcome::Ok(_) => {
            return Err(legacy_tool_error(
                "INVALID_ARGUMENT",
                format!(
                    "Invalid argument value: Agent name '{agent_name}' already exists in this project. \
Choose a different name (or omit the name to auto-generate one)."
                ),
                true,
                json!({
                    "field": "name_hint",
                    "error_detail": agent_name,
                }),
            ));
        }
        Outcome::Err(e) => match e {
            mcp_agent_mail_db::DbError::NotFound { .. } => {}
            other => return Err(db_error_to_mcp_error(other)),
        },
        Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
        Outcome::Panicked(p) => {
            return Err(McpError::internal_error(format!(
                "Internal panic: {}",
                p.message()
            )));
        }
    }

    let agent_out = mcp_agent_mail_db::queries::register_agent(
        ctx.cx(),
        &pool,
        project_id,
        &agent_name,
        &program,
        &model,
        task_description.as_deref(),
        Some(&policy),
    )
    .await;

    let row = db_outcome_to_mcp_result(agent_out)?;

    // Write agent profile to git archive (best-effort)
    let config = Config::from_env();
    let agent_json = serde_json::json!({
        "name": row.name,
        "program": row.program,
        "model": row.model,
        "task_description": row.task_description,
        "inception_ts": micros_to_iso(row.inception_ts),
        "last_active_ts": micros_to_iso(row.last_active_ts),
        "attachments_policy": row.attachments_policy,
    });
    try_write_agent_profile(&config, &project.slug, &agent_json);

    let response = AgentResponse {
        id: row.id.unwrap_or(0),
        name: row.name,
        program: row.program,
        model: row.model,
        task_description: row.task_description,
        inception_ts: micros_to_iso(row.inception_ts),
        last_active_ts: micros_to_iso(row.last_active_ts),
        project_id: row.project_id,
        attachments_policy: row.attachments_policy,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Look up agent profile with optional recent commits.
///
/// # Parameters
/// - `project_key`: Project human key or slug
/// - `agent_name`: Agent name to look up
/// - `include_recent_commits`: Include recent Git commits (default: true)
/// - `commit_limit`: Max commits to include (default: 5)
///
/// # Returns
/// Agent profile with optional commit history
#[tool(description = "Return enriched profile details for an agent.")]
pub async fn whois(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    include_recent_commits: Option<bool>,
    commit_limit: Option<i32>,
) -> McpResult<String> {
    let pool = get_db_pool()?;

    let include_commits = include_recent_commits.unwrap_or(true);
    let limit_raw = commit_limit.unwrap_or(5).max(0);
    let limit = usize::try_from(limit_raw).unwrap_or(0);

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent_out =
        mcp_agent_mail_db::queries::get_agent(ctx.cx(), &pool, project_id, &agent_name).await;
    let agent_row = db_outcome_to_mcp_result(agent_out)?;

    // Fetch recent commits from the git archive if requested
    let recent_commits = if include_commits && limit > 0 {
        let config = Config::from_env();
        match mcp_agent_mail_storage::ensure_archive(&config, &project.slug) {
            Ok(archive) => {
                let path_filter = format!("projects/{}/agents/{}", project.slug, agent_row.name);
                match mcp_agent_mail_storage::get_recent_commits(
                    &archive,
                    limit,
                    Some(&path_filter),
                ) {
                    Ok(commits) => commits
                        .into_iter()
                        .map(|c| CommitInfo {
                            hexsha: c.sha,
                            summary: c.summary,
                            authored_ts: c.date,
                        })
                        .collect(),
                    Err(e) => {
                        tracing::warn!("Failed to get recent commits: {e}");
                        Vec::new()
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to ensure archive for commits: {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let response = WhoisResponse {
        agent: AgentResponse {
            id: agent_row.id.unwrap_or(0),
            name: agent_row.name,
            program: agent_row.program,
            model: agent_row.model,
            task_description: agent_row.task_description,
            inception_ts: micros_to_iso(agent_row.inception_ts),
            last_active_ts: micros_to_iso(agent_row.last_active_ts),
            project_id: agent_row.project_id,
            attachments_policy: agent_row.attachments_policy,
        },
        recent_commits,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}
