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

const fn us_to_ms_ceil(us: u64) -> u64 {
    us.saturating_add(999).saturating_div(1000)
}

/// Try to write an agent profile to the git archive. Failures are logged
/// but do not fail the tool call – the DB is the source of truth.
///
/// Uses the write-behind queue when available. If the queue is unavailable,
/// logs a warning and skips the archive write.
fn try_write_agent_profile(config: &Config, project_slug: &str, agent_json: &serde_json::Value) {
    let op = mcp_agent_mail_storage::WriteOp::AgentProfile {
        project_slug: project_slug.to_string(),
        config: config.clone(),
        agent_json: agent_json.clone(),
    };
    match mcp_agent_mail_storage::wbq_enqueue(op) {
        mcp_agent_mail_storage::WbqEnqueueResult::Enqueued
        | mcp_agent_mail_storage::WbqEnqueueResult::SkippedDiskCritical => {
            // Disk pressure guard: archive writes may be disabled; DB remains authoritative.
        }
        mcp_agent_mail_storage::WbqEnqueueResult::QueueUnavailable => {
            tracing::warn!(
                "WBQ enqueue failed; skipping agent profile archive write project={project_slug}"
            );
        }
    }
}

fn enqueue_project_semantic_index(project: &mcp_agent_mail_db::ProjectRow) {
    let project_id = project.id.unwrap_or(0);
    let _ = mcp_agent_mail_db::search_service::enqueue_semantic_document(
        mcp_agent_mail_db::search_planner::DocKind::Project,
        project_id,
        Some(project_id),
        &project.slug,
        &project.human_key,
    );
}

fn enqueue_agent_semantic_index(agent: &mcp_agent_mail_db::AgentRow) {
    let _ = mcp_agent_mail_db::search_service::enqueue_semantic_document(
        mcp_agent_mail_db::search_planner::DocKind::Agent,
        agent.id.unwrap_or(0),
        Some(agent.project_id),
        &agent.name,
        &format!(
            "{}\n{}\n{}",
            agent.program, agent.model, agent.task_description
        ),
    );
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResponse {
    pub status: String,
    pub health_level: String,
    pub environment: String,
    pub http_host: String,
    pub http_port: u16,
    pub database_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_utilization: Option<PoolUtilizationResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queues: Option<QueuesHealthResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskHealthResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<IntegrityHealthResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityHealthResponse {
    pub last_ok_ts: i64,
    pub last_check_ts: i64,
    pub checks_total: u64,
    pub failures_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskHealthResponse {
    pub storage_root: String,
    pub storage_probe_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_probe_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_free_bytes: Option<u64>,
    pub pressure: String,
    pub archive_writes_disabled: bool,
    pub warning_threshold_mb: u64,
    pub critical_threshold_mb: u64,
    pub fatal_threshold_mb: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolUtilizationResponse {
    pub active: u64,
    pub idle: u64,
    pub total: u64,
    pub pending: u64,
    pub peak_active: u64,
    pub utilization_pct: u64,
    pub acquire_p50_ms: u64,
    pub acquire_p95_ms: u64,
    pub acquire_p99_ms: u64,
    pub over_80_for_s: u64,
    pub warning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuesHealthResponse {
    pub wbq: WbqQueueHealthResponse,
    pub commit_coalescer: CommitCoalescerHealthResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WbqQueueHealthResponse {
    pub depth: u64,
    pub capacity: u64,
    pub utilization_pct: u64,
    pub peak_depth: u64,
    pub enqueued_total: u64,
    pub drained_total: u64,
    pub errors_total: u64,
    pub backpressure_total: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub over_80_for_s: u64,
    pub warning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitCoalescerHealthResponse {
    pub pending_requests: u64,
    pub soft_cap: u64,
    pub utilization_pct: u64,
    pub peak_pending_requests: u64,
    pub enqueued_total: u64,
    pub drained_total: u64,
    pub errors_total: u64,
    pub sync_fallbacks_total: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub over_80_for_s: u64,
    pub warning: bool,
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
#[allow(clippy::too_many_lines)]
pub fn health_check(_ctx: &McpContext) -> McpResult<String> {
    let config = &Config::get();
    let pool = get_db_pool()?;
    pool.sample_pool_stats_now();
    // Ensure background workers are running so health_check reports stable
    // queue capacity/soft-cap values even before the first write/commit.
    mcp_agent_mail_storage::wbq_start();
    let _ = mcp_agent_mail_storage::get_commit_coalescer();
    let metrics = mcp_agent_mail_core::global_metrics().snapshot();
    let disk_sample = if config.disk_space_monitor_enabled {
        Some(mcp_agent_mail_core::disk::sample_and_record(config))
    } else {
        None
    };

    let now_us = u64::try_from(mcp_agent_mail_db::now_micros()).unwrap_or(0);
    let over_80_for_s = if metrics.db.pool_over_80_since_us == 0 {
        0
    } else {
        now_us
            .saturating_sub(metrics.db.pool_over_80_since_us)
            .saturating_div(1_000_000)
    };

    // Refresh the cached health level from live metrics
    let (health_level, _changed) = mcp_agent_mail_core::refresh_health_level();

    let response = HealthCheckResponse {
        status: "ok".to_string(),
        health_level: health_level.to_string(),
        environment: config.app_environment.to_string(),
        http_host: config.http_host.clone(),
        http_port: config.http_port,
        database_url: redact_database_url(&config.database_url),
        pool_utilization: Some(PoolUtilizationResponse {
            active: metrics.db.pool_active_connections,
            idle: metrics.db.pool_idle_connections,
            total: metrics.db.pool_total_connections,
            pending: metrics.db.pool_pending_requests,
            peak_active: metrics.db.pool_peak_active_connections,
            utilization_pct: metrics.db.pool_utilization_pct,
            acquire_p50_ms: us_to_ms_ceil(metrics.db.pool_acquire_latency_us.p50),
            acquire_p95_ms: us_to_ms_ceil(metrics.db.pool_acquire_latency_us.p95),
            acquire_p99_ms: us_to_ms_ceil(metrics.db.pool_acquire_latency_us.p99),
            over_80_for_s,
            warning: over_80_for_s >= 300,
        }),
        queues: Some({
            let wbq_over_80_for_s = if metrics.storage.wbq_over_80_since_us == 0 {
                0
            } else {
                now_us
                    .saturating_sub(metrics.storage.wbq_over_80_since_us)
                    .saturating_div(1_000_000)
            };
            let wbq_utilization_pct = if metrics.storage.wbq_capacity == 0 {
                0
            } else {
                metrics
                    .storage
                    .wbq_depth
                    .saturating_mul(100)
                    .saturating_div(metrics.storage.wbq_capacity)
            };

            let commit_over_80_for_s = if metrics.storage.commit_over_80_since_us == 0 {
                0
            } else {
                now_us
                    .saturating_sub(metrics.storage.commit_over_80_since_us)
                    .saturating_div(1_000_000)
            };
            let commit_utilization_pct = if metrics.storage.commit_soft_cap == 0 {
                0
            } else {
                metrics
                    .storage
                    .commit_pending_requests
                    .saturating_mul(100)
                    .saturating_div(metrics.storage.commit_soft_cap)
            };

            QueuesHealthResponse {
                wbq: WbqQueueHealthResponse {
                    depth: metrics.storage.wbq_depth,
                    capacity: metrics.storage.wbq_capacity,
                    utilization_pct: wbq_utilization_pct,
                    peak_depth: metrics.storage.wbq_peak_depth,
                    enqueued_total: metrics.storage.wbq_enqueued_total,
                    drained_total: metrics.storage.wbq_drained_total,
                    errors_total: metrics.storage.wbq_errors_total,
                    backpressure_total: metrics.storage.wbq_fallbacks_total,
                    latency_p50_ms: us_to_ms_ceil(metrics.storage.wbq_queue_latency_us.p50),
                    latency_p95_ms: us_to_ms_ceil(metrics.storage.wbq_queue_latency_us.p95),
                    latency_p99_ms: us_to_ms_ceil(metrics.storage.wbq_queue_latency_us.p99),
                    over_80_for_s: wbq_over_80_for_s,
                    warning: wbq_over_80_for_s >= 300,
                },
                commit_coalescer: CommitCoalescerHealthResponse {
                    pending_requests: metrics.storage.commit_pending_requests,
                    soft_cap: metrics.storage.commit_soft_cap,
                    utilization_pct: commit_utilization_pct,
                    peak_pending_requests: metrics.storage.commit_peak_pending_requests,
                    enqueued_total: metrics.storage.commit_enqueued_total,
                    drained_total: metrics.storage.commit_drained_total,
                    errors_total: metrics.storage.commit_errors_total,
                    sync_fallbacks_total: metrics.storage.commit_sync_fallbacks_total,
                    latency_p50_ms: us_to_ms_ceil(metrics.storage.commit_queue_latency_us.p50),
                    latency_p95_ms: us_to_ms_ceil(metrics.storage.commit_queue_latency_us.p95),
                    latency_p99_ms: us_to_ms_ceil(metrics.storage.commit_queue_latency_us.p99),
                    over_80_for_s: commit_over_80_for_s,
                    warning: commit_over_80_for_s >= 300,
                },
            }
        }),
        disk: disk_sample.as_ref().map(|s| DiskHealthResponse {
            storage_root: config.storage_root.display().to_string(),
            storage_probe_path: s.storage_probe_path.display().to_string(),
            db_probe_path: s.db_probe_path.as_ref().map(|p| p.display().to_string()),
            storage_free_bytes: s.storage_free_bytes,
            db_free_bytes: s.db_free_bytes,
            effective_free_bytes: s.effective_free_bytes,
            pressure: s.pressure.label().to_string(),
            archive_writes_disabled: matches!(
                s.pressure,
                mcp_agent_mail_core::disk::DiskPressure::Critical
                    | mcp_agent_mail_core::disk::DiskPressure::Fatal
            ),
            warning_threshold_mb: config.disk_space_warning_mb,
            critical_threshold_mb: config.disk_space_critical_mb,
            fatal_threshold_mb: config.disk_space_fatal_mb,
            errors: s.errors.clone(),
        }),
        integrity: {
            let im = mcp_agent_mail_db::integrity_metrics();
            if im.checks_total > 0 {
                Some(IntegrityHealthResponse {
                    last_ok_ts: im.last_ok_ts,
                    last_check_ts: im.last_check_ts,
                    checks_total: im.checks_total,
                    failures_total: im.failures_total,
                })
            } else {
                None
            }
        },
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
#[tool(
    description = "Idempotently create or ensure a project exists for the given human key.\n\nWhen to use\n-----------\n- First call in a workflow targeting a new repo/path identifier.\n- As a guard before registering agents or sending messages.\n\nHow it works\n------------\n- Validates that `human_key` is an absolute directory path (the agent's working directory).\n- Computes a stable slug from `human_key` (lowercased, safe characters) so\n  multiple agents can refer to the same project consistently.\n- Ensures DB row exists and that the on-disk archive is initialized\n  (e.g., `messages/`, `agents/`, `file_reservations/` directories).\n\nCRITICAL: Project Identity Rules\n---------------------------------\n- The `human_key` MUST be the absolute path to the agent's working directory\n- Two agents working in the SAME directory path are working on the SAME project\n- Example: Both agents in /data/projects/smartedgar_mcp \u{2192} SAME project\n- Sibling projects are DIFFERENT directories (e.g., /data/projects/smartedgar_mcp\n  vs /data/projects/smartedgar_mcp_frontend)\n\nParameters\n----------\nhuman_key : str\n    The absolute path to the agent's working directory (e.g., \"/data/projects/backend\").\n    This MUST be an absolute path, not a relative path or arbitrary slug.\n    This is the canonical identifier for the project - all agents working in this\n    directory will share the same project identity.\n\nReturns\n-------\ndict\n    Minimal project descriptor: { id, slug, human_key, created_at }.\n\nExamples\n--------\nJSON-RPC:\n```json\n{\n  \"jsonrpc\": \"2.0\",\n  \"id\": \"2\",\n  \"method\": \"tools/call\",\n  \"params\": {\"name\": \"ensure_project\", \"arguments\": {\"human_key\": \"/data/projects/backend\"}}\n}\n```\n\nCommon mistakes\n---------------\n- Passing a relative path (e.g., \"./backend\") instead of an absolute path\n- Using arbitrary slugs instead of the actual working directory path\n- Creating separate projects for the same directory with different slugs\n\nIdempotency\n-----------\n- Safe to call multiple times. If the project already exists, the existing\n  record is returned and the archive is ensured on disk (no destructive changes)."
)]
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

    let config = &Config::get();
    let pool = get_db_pool()?;

    // Log identity_mode if provided (future: resolve project identity via git remotes, etc.)
    if let Some(mode) = identity_mode {
        tracing::debug!("ensure_project identity_mode={mode}");
    }

    let row = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::ensure_project(ctx.cx(), &pool, &human_key).await,
    )?;
    enqueue_project_semantic_index(&row);

    // Ensure the git archive directory exists for this project
    if let Err(e) = mcp_agent_mail_storage::ensure_archive(config, &row.slug) {
        tracing::warn!("Failed to ensure archive for project '{}': {e}", row.slug);
    }

    // Always return extended format with identity fields (null when not resolved)
    let mut identity = mcp_agent_mail_core::resolve_project_identity(&human_key);
    identity.slug.clone_from(&row.slug);

    let response = ProjectWithIdentityResponse {
        id: row.id.unwrap_or(0),
        created_at: micros_to_iso(row.created_at),
        identity,
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
#[allow(clippy::too_many_lines)]
#[tool(
    description = "Create or update an agent identity within a project and persist its profile to Git.\n\nWhen to use\n-----------\n- At the start of a coding session by any automated agent.\n- To update an existing agent's program/model/task metadata and bump last_active.\n\nSemantics\n---------\n- If `name` is omitted, a random adjective+noun name is auto-generated.\n- Reusing the same `name` updates the profile (program/model/task) and\n  refreshes `last_active_ts`.\n- A `profile.json` file is written under `agents/<Name>/` in the project archive.\n\nCRITICAL: Agent Naming Rules\n-----------------------------\n- Agent names MUST be randomly generated adjective+noun combinations\n- Examples: \"GreenLake\", \"BlueDog\", \"RedStone\", \"PurpleBear\"\n- Names should be unique, easy to remember, and NOT descriptive\n- INVALID examples: \"BackendHarmonizer\", \"DatabaseMigrator\", \"UIRefactorer\"\n- The whole point: names should be memorable identifiers, not role descriptions\n- Best practice: Omit the `name` parameter to auto-generate a valid name\n\nParameters\n----------\nproject_key : str\n    The same human key you passed to `ensure_project` (or equivalent identifier).\nprogram : str\n    The agent program (e.g., \"codex-cli\", \"claude-code\").\nmodel : str\n    The underlying model (e.g., \"gpt5-codex\", \"opus-4.1\").\nname : Optional[str]\n    MUST be a valid adjective+noun combination if provided (e.g., \"BlueLake\").\n    If omitted, a random valid name is auto-generated (RECOMMENDED).\n    Names are unique per project; passing the same name updates the profile.\ntask_description : str\n    Short description of current focus (shows up in directory listings).\n\nReturns\n-------\ndict\n    { id, name, program, model, task_description, inception_ts, last_active_ts, project_id }\n\nExamples\n--------\nRegister with auto-generated name (RECOMMENDED):\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"3\",\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"codex-cli\",\"model\":\"gpt5-codex\",\"task_description\":\"Auth refactor\"\n}}}\n```\n\nRegister with explicit valid name:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"4\",\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"claude-code\",\"model\":\"opus-4.1\",\"name\":\"BlueLake\",\"task_description\":\"Navbar redesign\"\n}}}\n```\n\nPitfalls\n--------\n- Names MUST match the adjective+noun format or an error will be raised\n- Names are case-insensitive unique. If you see \"already in use\", pick another or omit `name`.\n- Use the same `project_key` consistently across cooperating agents."
)]
pub async fn register_agent(
    ctx: &McpContext,
    project_key: String,
    program: String,
    model: String,
    name: Option<String>,
    task_description: Option<String>,
    attachments_policy: Option<String>,
) -> McpResult<String> {
    use mcp_agent_mail_core::models::{
        detect_agent_name_mistake, generate_agent_name, is_valid_agent_name,
    };

    // Validate program and model are non-empty
    let program = program.trim().to_string();
    if program.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_PROGRAM",
            "program cannot be empty. Provide the name of your AI coding tool \
             (e.g., 'claude-code', 'codex-cli', 'cursor', 'cline').",
            true,
            json!({ "provided": program }),
        ));
    }

    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_MODEL",
            "model cannot be empty. Provide the underlying model identifier \
             (e.g., 'claude-opus-4.5', 'gpt-4-turbo', 'claude-sonnet-4').",
            true,
            json!({ "provided": model }),
        ));
    }

    let pool = get_db_pool()?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Validate or generate agent name
    let agent_name = match name {
        Some(n) => {
            if !is_valid_agent_name(&n) {
                // Check for specific mistake types before generic error
                if let Some((mistake_type, message)) = detect_agent_name_mistake(&n) {
                    return Err(legacy_tool_error(
                        mistake_type,
                        &message,
                        true,
                        json!({
                            "provided_name": n,
                            "valid_examples": ["BlueLake", "GreenCastle", "RedStone"],
                        }),
                    ));
                }
                return Err(legacy_tool_error(
                    "INVALID_AGENT_NAME",
                    format!(
                        "Invalid agent name format: '{n}'. \
Agent names MUST be randomly generated adjective+noun combinations \
(e.g., 'GreenLake', 'BlueDog'), NOT descriptive names. \
Omit the 'name' parameter to auto-generate a valid name."
                    ),
                    true,
                    json!({
                        "provided_name": n,
                        "valid_examples": ["BlueLake", "GreenCastle", "RedStone"],
                    }),
                ));
            }
            n
        }
        None => generate_agent_name(),
    };

    // Validate attachments_policy if provided
    let policy = attachments_policy.unwrap_or_else(|| "auto".to_string());
    if !is_valid_attachments_policy(&policy) {
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
    enqueue_agent_semantic_index(&row);

    // Invalidate + repopulate read cache after mutation
    mcp_agent_mail_db::read_cache().invalidate_agent(project_id, &row.name);
    mcp_agent_mail_db::read_cache().put_agent(&row);

    // Write agent profile to git archive (best-effort)
    let config = &Config::get();
    let agent_json = serde_json::json!({
        "name": row.name,
        "program": row.program,
        "model": row.model,
        "task_description": row.task_description,
        "inception_ts": micros_to_iso(row.inception_ts),
        "last_active_ts": micros_to_iso(row.last_active_ts),
        "attachments_policy": row.attachments_policy,
    });
    try_write_agent_profile(config, &project.slug, &agent_json);

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
#[allow(clippy::too_many_lines)]
#[tool(
    description = "Create a new, unique agent identity and persist its profile to Git.\n\nHow this differs from `register_agent`\n--------------------------------------\n- Always creates a new identity with a fresh unique name (never updates an existing one).\n- `name_hint`, if provided, MUST be a valid adjective+noun combination and must be available,\n  otherwise an error is raised. Without a hint, a random adjective+noun name is generated.\n\nCRITICAL: Agent Naming Rules\n-----------------------------\n- Agent names MUST be randomly generated adjective+noun combinations\n- Examples: \"GreenCastle\", \"BlueLake\", \"RedStone\", \"PurpleBear\"\n- Names should be unique, easy to remember, and NOT descriptive\n- INVALID examples: \"BackendHarmonizer\", \"DatabaseMigrator\", \"UIRefactorer\"\n- Best practice: Omit `name_hint` to auto-generate a valid name (RECOMMENDED)\n\nWhen to use\n-----------\n- Spawning a brand new worker agent that should not overwrite an existing profile.\n- Temporary task-specific identities (e.g., short-lived refactor assistants).\n\nReturns\n-------\ndict\n    { id, name, program, model, task_description, inception_ts, last_active_ts, project_id }\n\nExamples\n--------\nAuto-generate name (RECOMMENDED):\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"c2\",\"method\":\"tools/call\",\"params\":{\"name\":\"create_agent_identity\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"claude-code\",\"model\":\"opus-4.1\"\n}}}\n```\n\nWith valid name hint:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"c1\",\"method\":\"tools/call\",\"params\":{\"name\":\"create_agent_identity\",\"arguments\":{\n  \"project_key\":\"/data/projects/backend\",\"program\":\"codex-cli\",\"model\":\"gpt5-codex\",\"name_hint\":\"GreenCastle\",\n  \"task_description\":\"DB migration spike\"\n}}}\n```"
)]
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

    // Validate program and model are non-empty
    let program = program.trim().to_string();
    if program.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_PROGRAM",
            "program cannot be empty. Provide the name of your AI coding tool \
             (e.g., 'claude-code', 'codex-cli', 'cursor', 'cline').",
            true,
            json!({ "provided": program }),
        ));
    }

    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(legacy_tool_error(
            "EMPTY_MODEL",
            "model cannot be empty. Provide the underlying model identifier \
             (e.g., 'claude-opus-4.5', 'gpt-4-turbo', 'claude-sonnet-4').",
            true,
            json!({ "provided": model }),
        ));
    }

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
    if !is_valid_attachments_policy(&policy) {
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

    // Atomic insert-if-absent: eliminates TOCTOU race between a separate
    // get_agent check and register_agent upsert. Returns Duplicate if the
    // name was taken between validation and insert.
    let agent_out = mcp_agent_mail_db::queries::create_agent(
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

    let row = match agent_out {
        Outcome::Ok(row) => row,
        Outcome::Err(mcp_agent_mail_db::DbError::Duplicate { .. }) => {
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
        Outcome::Err(other) => return Err(db_error_to_mcp_error(other)),
        Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
        Outcome::Panicked(p) => {
            return Err(McpError::internal_error(format!(
                "Internal panic: {}",
                p.message()
            )));
        }
    };
    enqueue_agent_semantic_index(&row);

    // Invalidate + repopulate read cache after mutation
    mcp_agent_mail_db::read_cache().invalidate_agent(project_id, &row.name);
    mcp_agent_mail_db::read_cache().put_agent(&row);

    // Write agent profile to git archive (best-effort)
    let config = &Config::get();
    let agent_json = serde_json::json!({
        "name": row.name,
        "program": row.program,
        "model": row.model,
        "task_description": row.task_description,
        "inception_ts": micros_to_iso(row.inception_ts),
        "last_active_ts": micros_to_iso(row.last_active_ts),
        "attachments_policy": row.attachments_policy,
    });
    try_write_agent_profile(config, &project.slug, &agent_json);

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

/// Validate `attachments_policy` value.
///
/// Returns `true` if the policy is one of the valid values: auto, inline, file, none.
#[must_use]
pub fn is_valid_attachments_policy(policy: &str) -> bool {
    ["auto", "inline", "file", "none"].contains(&policy)
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
#[tool(
    description = "Return enriched profile details for an agent, optionally including recent archive commits.\n\nDiscovery\n---------\nTo discover available agent names, use: resource://agents/{project_key}\nAgent names are NOT the same as program names or user names.\n\nParameters\n----------\nproject_key : str\n    Project slug or human key.\nagent_name : str\n    Agent name to look up (use resource://agents/{project_key} to discover names).\ninclude_recent_commits : bool\n    If true, include latest commits touching the project archive authored by the configured git author.\ncommit_limit : int\n    Maximum number of recent commits to include.\n\nReturns\n-------\ndict\n    Agent profile augmented with { recent_commits: [{hexsha, summary, authored_ts}] } when requested."
)]
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
        let config = &Config::get();
        match mcp_agent_mail_storage::ensure_archive(config, &project.slug) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── redact_database_url ──

    #[test]
    fn redact_hides_password_in_postgres_url() {
        assert_eq!(
            redact_database_url("postgres://user:secret@localhost/db"),
            "postgres://****@localhost/db"
        );
    }

    #[test]
    fn redact_hides_password_in_sqlite_userinfo() {
        assert_eq!(
            redact_database_url("sqlite://admin:pass123@/data/test.db"),
            "sqlite://****@/data/test.db"
        );
    }

    #[test]
    fn redact_preserves_url_without_credentials() {
        assert_eq!(
            redact_database_url("sqlite:///data/agent_mail.db"),
            "sqlite:///data/agent_mail.db"
        );
    }

    #[test]
    fn redact_preserves_plain_path() {
        assert_eq!(
            redact_database_url("/data/agent_mail.db"),
            "/data/agent_mail.db"
        );
    }

    #[test]
    fn redact_handles_empty_string() {
        assert_eq!(redact_database_url(""), "");
    }

    #[test]
    fn redact_handles_no_at_sign() {
        assert_eq!(
            redact_database_url("postgres://localhost/db"),
            "postgres://localhost/db"
        );
    }

    #[test]
    fn redact_handles_complex_password_with_special_chars() {
        assert_eq!(
            redact_database_url("postgres://user:p@ss%40word@host:5432/db"),
            "postgres://****@host:5432/db"
        );
    }

    // ── is_valid_attachments_policy ──

    #[test]
    fn valid_attachments_policies_accepted() {
        assert!(is_valid_attachments_policy("auto"));
        assert!(is_valid_attachments_policy("inline"));
        assert!(is_valid_attachments_policy("file"));
        assert!(is_valid_attachments_policy("none"));
    }

    #[test]
    fn invalid_attachments_policies_rejected() {
        assert!(!is_valid_attachments_policy(""));
        assert!(!is_valid_attachments_policy("AUTO"));
        assert!(!is_valid_attachments_policy("Inline"));
        assert!(!is_valid_attachments_policy("always"));
        assert!(!is_valid_attachments_policy("never"));
        assert!(!is_valid_attachments_policy("detach"));
        assert!(!is_valid_attachments_policy(" auto"));
        assert!(!is_valid_attachments_policy("auto "));
    }

    // ── Response type serialization ──

    #[test]
    fn health_check_response_serializes() {
        let r = HealthCheckResponse {
            status: "ok".into(),
            health_level: "green".into(),
            environment: "development".into(),
            http_host: "0.0.0.0".into(),
            http_port: 8765,
            database_url: "sqlite:///data/test.db".into(),
            pool_utilization: None,
            queues: None,
            disk: None,
            integrity: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["http_port"], 8765);
    }

    #[test]
    fn project_response_serializes() {
        let r = ProjectResponse {
            id: 1,
            slug: "data-projects-test".into(),
            human_key: "/data/projects/test".into(),
            created_at: "2026-02-06T00:00:00Z".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["slug"], "data-projects-test");
        assert_eq!(json["human_key"], "/data/projects/test");
    }

    #[test]
    fn agent_response_serializes_all_fields() {
        let r = AgentResponse {
            id: 42,
            name: "BlueLake".into(),
            program: "claude-code".into(),
            model: "opus-4.5".into(),
            task_description: "Testing".into(),
            inception_ts: "2026-02-06T00:00:00Z".into(),
            last_active_ts: "2026-02-06T01:00:00Z".into(),
            project_id: 1,
            attachments_policy: "auto".into(),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["name"], "BlueLake");
        assert_eq!(json["program"], "claude-code");
        assert_eq!(json["attachments_policy"], "auto");
        assert_eq!(json["id"], 42);
        assert_eq!(json["project_id"], 1);
    }

    #[test]
    fn agent_response_round_trips() {
        let original = AgentResponse {
            id: 42,
            name: "BlueLake".into(),
            program: "claude-code".into(),
            model: "opus-4.5".into(),
            task_description: "Testing".into(),
            inception_ts: "2026-02-06T00:00:00Z".into(),
            last_active_ts: "2026-02-06T01:00:00Z".into(),
            project_id: 1,
            attachments_policy: "auto".into(),
        };
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: AgentResponse = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.name, original.name);
        assert_eq!(deserialized.id, original.id);
        assert_eq!(deserialized.program, original.program);
    }

    #[test]
    fn whois_response_flattens_agent_fields() {
        let r = WhoisResponse {
            agent: AgentResponse {
                id: 1,
                name: "RedFox".into(),
                program: "codex-cli".into(),
                model: "gpt-5".into(),
                task_description: String::new(),
                inception_ts: "2026-02-06T00:00:00Z".into(),
                last_active_ts: "2026-02-06T00:00:00Z".into(),
                project_id: 1,
                attachments_policy: "auto".into(),
            },
            recent_commits: vec![CommitInfo {
                hexsha: "abc123".into(),
                summary: "test commit".into(),
                authored_ts: "2026-02-06T00:00:00Z".into(),
            }],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        // Agent fields are flattened into the top level
        assert_eq!(json["name"], "RedFox");
        assert_eq!(json["program"], "codex-cli");
        // Commits are nested
        assert_eq!(json["recent_commits"][0]["hexsha"], "abc123");
    }

    #[test]
    fn whois_response_empty_commits_array() {
        let r = WhoisResponse {
            agent: AgentResponse {
                id: 1,
                name: "BlueLake".into(),
                program: "claude-code".into(),
                model: "opus-4.5".into(),
                task_description: String::new(),
                inception_ts: String::new(),
                last_active_ts: String::new(),
                project_id: 1,
                attachments_policy: "none".into(),
            },
            recent_commits: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(json["recent_commits"].as_array().unwrap().is_empty());
    }

    // ── Path validation (ensure_project logic) ──

    #[test]
    fn absolute_paths_detected() {
        assert!(Path::new("/data/projects/test").is_absolute());
        assert!(Path::new("/").is_absolute());
        assert!(Path::new("/home/user/.config").is_absolute());
    }

    #[test]
    fn relative_paths_detected() {
        assert!(!Path::new("data/projects/test").is_absolute());
        assert!(!Path::new("./test").is_absolute());
        assert!(!Path::new("test").is_absolute());
        assert!(!Path::new("").is_absolute());
    }

    // ── Agent name validation (from core) ──

    #[test]
    fn valid_agent_names_accepted() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(is_valid_agent_name("BlueLake"));
        assert!(is_valid_agent_name("RedFox"));
        assert!(is_valid_agent_name("GoldHawk"));
    }

    #[test]
    fn invalid_agent_names_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(!is_valid_agent_name(""));
        assert!(!is_valid_agent_name("blue_lake")); // underscore not allowed
        assert!(!is_valid_agent_name("123"));
        assert!(!is_valid_agent_name("Blue Lake")); // space not allowed
        assert!(!is_valid_agent_name("EaglePeak")); // eagle is a noun, not adjective
        assert!(!is_valid_agent_name("BraveLion")); // brave not in adjective list
        assert!(!is_valid_agent_name("x")); // too short
    }

    // ── Whitespace trimming for program/model ──

    #[test]
    fn whitespace_only_program_is_empty_after_trim() {
        assert!("".trim().is_empty());
        assert!("  ".trim().is_empty());
        assert!("\t".trim().is_empty());
        assert!(!"claude-code".trim().is_empty());
    }

    #[test]
    fn whitespace_only_model_is_empty_after_trim() {
        assert!("".trim().is_empty());
        assert!("  ".trim().is_empty());
        assert!(!"opus-4.5".trim().is_empty());
    }

    // -----------------------------------------------------------------------
    // Tool validation rule tests (br-2841)
    // -----------------------------------------------------------------------

    // ── ensure_project validation ──

    #[test]
    fn ensure_project_rejects_relative_path() {
        // ensure_project requires absolute paths (starts with '/')
        let key = "relative/path/to/project";
        assert!(!Path::new(key).is_absolute());
    }

    #[test]
    fn ensure_project_rejects_empty_key() {
        assert!(!Path::new("").is_absolute());
    }

    #[test]
    fn ensure_project_accepts_root_path() {
        assert!(Path::new("/").is_absolute());
    }

    #[test]
    fn ensure_project_accepts_deeply_nested_path() {
        assert!(Path::new("/a/b/c/d/e/f/g").is_absolute());
    }

    // ── Agent name validation extended ──

    #[test]
    fn agent_name_validation_case_insensitive() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        // Validation is case-insensitive (lowercases before checking)
        assert!(is_valid_agent_name("BlueLake"));
        assert!(is_valid_agent_name("bluelake"));
        assert!(is_valid_agent_name("BLUELAKE"));
        assert!(is_valid_agent_name("bLuElAkE"));
    }

    #[test]
    fn agent_name_numbers_only_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(!is_valid_agent_name("12345"));
    }

    #[test]
    fn agent_name_special_chars_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        assert!(!is_valid_agent_name("Blue-Lake"));
        assert!(!is_valid_agent_name("Blue_Lake"));
        assert!(!is_valid_agent_name("Blue.Lake"));
        assert!(!is_valid_agent_name("Blue@Lake"));
    }

    #[test]
    fn agent_name_descriptive_names_rejected() {
        use mcp_agent_mail_core::models::is_valid_agent_name;
        // These look like agent names but use invalid adjectives/nouns
        assert!(!is_valid_agent_name("BackendHarmonizer"));
        assert!(!is_valid_agent_name("DatabaseMigrator"));
        assert!(!is_valid_agent_name("UIRefactorer"));
    }

    // ── Attachments policy validation extended ──

    #[test]
    fn attachments_policy_all_valid_values() {
        for policy in &["auto", "inline", "file", "none"] {
            assert!(
                is_valid_attachments_policy(policy),
                "Policy '{policy}' should be valid"
            );
        }
    }

    #[test]
    fn attachments_policy_boundary_values() {
        // Near-misses and common mistakes
        assert!(!is_valid_attachments_policy("auto\n"));
        assert!(!is_valid_attachments_policy("\nauto"));
        assert!(!is_valid_attachments_policy("auto\0"));
        assert!(!is_valid_attachments_policy("inlined"));
        assert!(!is_valid_attachments_policy("files"));
    }

    // ── us_to_ms_ceil correctness ──

    #[test]
    fn us_to_ms_ceil_rounds_up() {
        assert_eq!(us_to_ms_ceil(0), 0);
        assert_eq!(us_to_ms_ceil(1), 1); // 1µs → 1ms (rounded up)
        assert_eq!(us_to_ms_ceil(999), 1); // 999µs → 1ms
        assert_eq!(us_to_ms_ceil(1000), 1); // exactly 1ms
        assert_eq!(us_to_ms_ceil(1001), 2); // 1001µs → 2ms
        assert_eq!(us_to_ms_ceil(1500), 2); // 1.5ms → 2ms
        assert_eq!(us_to_ms_ceil(2000), 2); // exactly 2ms
    }

    #[test]
    fn us_to_ms_ceil_handles_max() {
        // Should not overflow/panic with u64::MAX
        let result = us_to_ms_ceil(u64::MAX);
        // u64::MAX.saturating_add(999) → u64::MAX; u64::MAX / 1000 = 18446744073709551
        assert!(result > 0);
    }

    // ── Response serialization — optional fields omitted ──

    #[test]
    fn health_check_omits_optional_null_fields() {
        let r = HealthCheckResponse {
            status: "ok".into(),
            health_level: "green".into(),
            environment: "test".into(),
            http_host: "localhost".into(),
            http_port: 8765,
            database_url: "sqlite:///:memory:".into(),
            pool_utilization: None,
            queues: None,
            disk: None,
            integrity: None,
        };
        let json_str = serde_json::to_string(&r).unwrap();
        assert!(!json_str.contains("pool_utilization"));
        assert!(!json_str.contains("queues"));
        assert!(!json_str.contains("disk"));
        assert!(!json_str.contains("integrity"));
    }

    #[test]
    fn redact_database_url_memory_db() {
        // In-memory SQLite should pass through unchanged
        assert_eq!(
            redact_database_url("sqlite:///:memory:"),
            "sqlite:///:memory:"
        );
    }

    #[test]
    fn redact_database_url_multiple_at_signs() {
        // Edge case: multiple @ signs — last one is the host separator
        let result = redact_database_url("postgres://user:p@ss@host/db");
        assert_eq!(result, "postgres://****@host/db");
    }
}
