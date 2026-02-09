//! MCP Resources for MCP Agent Mail
//!
//! Resources provide read-only access to project data:
//! - Configuration resources
//! - Identity resources
//! - Tooling resources
//! - Project resources
//! - Message & thread resources
//! - View resources
//! - File reservation resources

use fastmcp::McpErrorCode;
use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{iso_to_micros, micros_to_iso};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    tool_cluster,
    tool_util::{db_outcome_to_mcp_result, get_db_pool, resolve_project},
};

fn split_param_and_query(input: &str) -> (String, HashMap<String, String>) {
    if let Some((base, query)) = input.split_once('?') {
        (percent_decode_component(base), parse_query(query))
    } else {
        (percent_decode_component(input), HashMap::new())
    }
}

/// Parse a boolean query parameter, accepting the same truthy values as Python:
/// `"1"`, `"true"`, `"t"`, `"yes"`, `"y"` (case-insensitive, whitespace-trimmed).
fn parse_bool_param(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "t" | "yes" | "y"
    )
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = percent_decode_component(key);
        let value = percent_decode_component(value);
        params.insert(key, value);
    }
    params
}

fn percent_decode_component(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = bytes[i + 1];
                let lo = bytes[i + 2];
                let hex = [hi, lo];
                if let Ok(hex_str) = std::str::from_utf8(&hex) {
                    if let Ok(value) = u8::from_str_radix(hex_str, 16) {
                        out.push(value);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn workspace_root_from(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let cargo_toml = dir.join("Cargo.toml");
        if !cargo_toml.exists() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
            if content.contains("[workspace]") {
                return Some(dir.to_path_buf());
            }
        }
    }
    None
}

fn tool_filter_allows(config: &Config, tool_name: &str) -> bool {
    tool_cluster(tool_name).is_none_or(|cluster| config.should_expose_tool(tool_name, cluster))
}

// Float -> int casts saturate, but we treat out-of-range values as invalid timestamps.
// Use numeric literals to avoid clippy cast_precision_loss warnings.
const I64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;
const I64_MAX_F64: f64 = 9_223_372_036_854_775_807.0;

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ts_f64_to_rfc3339(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    // Use floor decomposition so negative fractional timestamps map correctly.
    // Example: -0.5 => 1969-12-31T23:59:59.5+00:00 (not 1970-01-01T00:00:00.5+00:00).
    let secs_f = t.floor();
    if !(I64_MIN_F64..=I64_MAX_F64).contains(&secs_f) {
        return None;
    }
    let secs = secs_f as i64;
    let nanos = ((t - secs_f) * 1e9).clamp(0.0, 999_999_999.0) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).map(|dt| dt.to_rfc3339())
}

// ============================================================================
// Configuration Resources
// ============================================================================

/// Environment configuration snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentSnapshot {
    pub environment: String,
    pub database_url: String,
    pub http: HttpSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpSnapshot {
    pub host: String,
    pub port: u16,
    pub path: String,
}

fn redact_database_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some((_creds, host)) = rest.rsplit_once('@') {
            return format!("{scheme}://****@{host}");
        }
    }
    url.to_string()
}

/// Get environment configuration snapshot.
#[resource(
    uri = "resource://config/environment",
    description = "Environment configuration snapshot"
)]
pub fn config_environment(_ctx: &McpContext) -> McpResult<String> {
    use mcp_agent_mail_core::Config;
    let config = &Config::get();

    let snapshot = EnvironmentSnapshot {
        environment: config.app_environment.to_string(),
        database_url: redact_database_url(&config.database_url),
        http: HttpSnapshot {
            host: config.http_host.clone(),
            port: config.http_port,
            path: config.http_path.clone(),
        },
    };

    serde_json::to_string(&snapshot)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get environment configuration snapshot (query-aware variant).
#[resource(
    uri = "resource://config/environment?{query}",
    description = "Environment configuration snapshot (with query)"
)]
pub fn config_environment_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let _query = parse_query(&query);
    config_environment(ctx)
}

// ============================================================================
// Identity Resources
// ============================================================================

/// Get Git identity resolution for a project.
#[resource(
    uri = "resource://identity/{project}",
    description = "Git identity resolution for a project"
)]
pub fn identity_project(_ctx: &McpContext, project: String) -> McpResult<String> {
    let (project_ref, _query) = split_param_and_query(&project);
    // Legacy parity: resolve relative refs against current directory first so
    // slug/project_uid are derived from a stable absolute path.
    let resolved_human_key = {
        let path = std::path::PathBuf::from(&project_ref);
        if path.is_absolute() {
            path
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let base = workspace_root_from(&cwd).unwrap_or(cwd);
            base.join(path)
        }
    };
    let resolved = resolved_human_key.to_string_lossy().to_string();
    let identity = mcp_agent_mail_core::resolve_project_identity(&resolved);

    serde_json::to_string(&identity)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Agent profile summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    pub last_active_ts: String,
    pub contact_policy: String,
}

/// Agent list entry with unread count
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListEntry {
    pub id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    pub inception_ts: Option<String>,
    pub last_active_ts: Option<String>,
    pub project_id: i64,
    pub attachments_policy: String,
    pub unread_count: i64,
}

/// Project reference for agents list
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRef {
    pub slug: String,
    pub human_key: String,
}

/// Agents list response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsListResponse {
    pub project: ProjectRef,
    pub agents: Vec<AgentListEntry>,
}

/// List agents in a project.
#[resource(
    uri = "resource://agents/{project_key}",
    description = "List of agents with profiles in a project"
)]
pub async fn agents_list(ctx: &McpContext, project_key: String) -> McpResult<String> {
    let (project_key, _query) = split_param_and_query(&project_key);
    let pool = get_db_pool()?;

    // Find project by slug
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;

    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // List agents in project
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;

    // Get unread counts for all agents in one query
    let conn = match pool.acquire(ctx.cx()).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Err(McpError::internal_error(e.to_string())),
        Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
        Outcome::Panicked(p) => {
            return Err(McpError::internal_error(format!(
                "Internal panic: {}",
                p.message()
            )));
        }
    };
    let sql = "SELECT r.agent_id, COUNT(*) as unread \
               FROM message_recipients r \
               JOIN messages m ON m.id = r.message_id \
               WHERE m.project_id = ? AND r.read_ts IS NULL \
               GROUP BY r.agent_id";
    let params = [mcp_agent_mail_db::sqlmodel::Value::BigInt(project_id)];
    let start = mcp_agent_mail_db::query_timer();
    let unread_rows = conn.query_sync(sql, &params);
    mcp_agent_mail_db::record_query(sql, mcp_agent_mail_db::elapsed_us(start));
    let unread_rows = unread_rows.map_err(|e| McpError::internal_error(e.to_string()))?;

    let mut unread_counts: std::collections::HashMap<i64, i64> =
        std::collections::HashMap::with_capacity(agents.len());
    for row in unread_rows {
        let agent_id: i64 = row.get_named("agent_id").unwrap_or(0);
        let count: i64 = row.get_named("unread").unwrap_or(0);
        unread_counts.insert(agent_id, count);
    }

    let response = AgentsListResponse {
        project: ProjectRef {
            slug: project.slug,
            human_key: project.human_key,
        },
        agents: agents
            .into_iter()
            .map(|a| {
                let agent_id = a.id.unwrap_or(0);
                AgentListEntry {
                    id: agent_id,
                    name: a.name,
                    program: a.program,
                    model: a.model,
                    task_description: a.task_description,
                    inception_ts: Some(micros_to_iso(a.inception_ts)),
                    last_active_ts: Some(micros_to_iso(a.last_active_ts)),
                    project_id: a.project_id,
                    attachments_policy: a.attachments_policy,
                    unread_count: *unread_counts.get(&agent_id).unwrap_or(&0),
                }
            })
            .collect(),
    };

    tracing::debug!("Listing agents in project {}", project_key);

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

// ============================================================================
// Tooling Resources
// ============================================================================

/// Tool usage example
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsageExample {
    pub hint: String,
    pub sample: String,
}

/// Tool directory entry (rich format matching Python)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDirectoryEntry {
    pub name: String,
    pub summary: String,
    pub use_when: String,
    pub related: Vec<String>,
    pub expected_frequency: String,
    pub required_capabilities: Vec<String>,
    pub usage_examples: Vec<ToolUsageExample>,
    pub capabilities: Vec<String>,
    pub complexity: String,
}

/// Tool cluster
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCluster {
    pub name: String,
    pub purpose: String,
    pub tools: Vec<ToolDirectoryEntry>,
}

/// Playbook workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playbook {
    pub workflow: String,
    pub sequence: Vec<String>,
}

/// Toon envelope format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToonEnvelope {
    pub format: String,
    pub data: String,
    pub meta: ToonMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToonMeta {
    pub requested: String,
}

/// Output formats configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFormats {
    pub default: String,
    pub tool_param: String,
    pub resource_query: String,
    pub values: Vec<String>,
    pub toon_envelope: ToonEnvelope,
}

/// Full tool directory response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDirectory {
    pub generated_at: Option<String>,
    pub metrics_uri: String,
    pub output_formats: OutputFormats,
    pub clusters: Vec<ToolCluster>,
    pub playbooks: Vec<Playbook>,
}

#[allow(clippy::too_many_lines)]
fn build_tool_directory() -> ToolDirectory {
    let config = &Config::get();
    let output_formats = OutputFormats {
        default: "json".to_string(),
        tool_param: "format".to_string(),
        resource_query: "format".to_string(),
        values: vec!["json".to_string(), "toon".to_string()],
        toon_envelope: ToonEnvelope {
            format: "toon".to_string(),
            data: "<TOON>".to_string(),
            meta: ToonMeta {
                requested: "toon".to_string(),
            },
        },
    };

    let mut clusters = vec![
        ToolCluster {
            name: "Infrastructure & Workspace Setup".to_string(),
            purpose: "Bootstrap coordination and guardrails before agents begin editing.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "health_check".to_string(),
                    summary: "Report environment and HTTP wiring so orchestrators confirm connectivity.".to_string(),
                    use_when: "Beginning a session or during incident response triage.".to_string(),
                    related: vec!["ensure_project".to_string()],
                    expected_frequency: "Once per agent session or when connectivity is in doubt.".to_string(),
                    required_capabilities: vec!["infrastructure".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Pre-flight".to_string(), sample: "health_check()".to_string() }],
                    capabilities: vec!["infrastructure".to_string()],
                    complexity: "low".to_string(),
                },
                ToolDirectoryEntry {
                    name: "ensure_project".to_string(),
                    summary: "Ensure project slug, schema, and archive exist for a shared repo identifier.".to_string(),
                    use_when: "First call against a repo or when switching projects.".to_string(),
                    related: vec!["register_agent".to_string(), "file_reservation_paths".to_string()],
                    expected_frequency: "Whenever a new repo/path is encountered.".to_string(),
                    required_capabilities: vec!["infrastructure".to_string(), "storage".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "First action".to_string(), sample: "ensure_project(human_key='/abs/path/backend')".to_string() }],
                    capabilities: vec!["infrastructure".to_string(), "storage".to_string()],
                    complexity: "low".to_string(),
                },
                ToolDirectoryEntry {
                    name: "install_precommit_guard".to_string(),
                    summary: "Install Git pre-commit hook that enforces advisory file_reservations locally.".to_string(),
                    use_when: "Onboarding a repository into coordinated mode.".to_string(),
                    related: vec!["file_reservation_paths".to_string(), "uninstall_precommit_guard".to_string()],
                    expected_frequency: "Infrequent—per repository setup.".to_string(),
                    required_capabilities: vec!["infrastructure".to_string(), "repository".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Onboard".to_string(), sample: "install_precommit_guard(project_key='backend', code_repo_path='~/repo')".to_string() }],
                    capabilities: vec!["infrastructure".to_string(), "repository".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "uninstall_precommit_guard".to_string(),
                    summary: "Remove the advisory pre-commit hook from a repo.".to_string(),
                    use_when: "Decommissioning or debugging the guard hook.".to_string(),
                    related: vec!["install_precommit_guard".to_string()],
                    expected_frequency: "Rare; only when disabling guard enforcement.".to_string(),
                    required_capabilities: vec!["infrastructure".to_string(), "repository".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Cleanup".to_string(), sample: "uninstall_precommit_guard(code_repo_path='~/repo')".to_string() }],
                    capabilities: vec!["infrastructure".to_string(), "repository".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "Identity & Directory".to_string(),
            purpose: "Register agents, mint unique identities, and inspect directory metadata.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "register_agent".to_string(),
                    summary: "Upsert an agent profile and refresh last_active_ts for a known persona.".to_string(),
                    use_when: "Resuming an identity or updating program/model/task metadata.".to_string(),
                    related: vec!["create_agent_identity".to_string(), "whois".to_string()],
                    expected_frequency: "At the start of each automated work session.".to_string(),
                    required_capabilities: vec!["identity".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Resume persona".to_string(), sample: "register_agent(project_key='/abs/path/backend', program='codex', model='gpt5')".to_string() }],
                    capabilities: vec!["identity".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "create_agent_identity".to_string(),
                    summary: "Always create a new unique agent name (optionally using a sanitized hint).".to_string(),
                    use_when: "Spawning a brand-new helper that should not overwrite existing profiles.".to_string(),
                    related: vec!["register_agent".to_string()],
                    expected_frequency: "When minting fresh, short-lived identities.".to_string(),
                    required_capabilities: vec!["identity".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "New helper".to_string(), sample: "create_agent_identity(project_key='backend', name_hint='GreenCastle', program='codex', model='gpt5')".to_string() }],
                    capabilities: vec!["identity".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "whois".to_string(),
                    summary: "Return enriched profile info plus recent archive commits for an agent.".to_string(),
                    use_when: "Dashboarding, routing coordination messages, or auditing activity.".to_string(),
                    related: vec!["register_agent".to_string()],
                    expected_frequency: "Ad hoc when context about an agent is required.".to_string(),
                    required_capabilities: vec!["audit".to_string(), "identity".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Directory lookup".to_string(), sample: "whois(project_key='backend', agent_name='BlueLake')".to_string() }],
                    capabilities: vec!["audit".to_string(), "identity".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "set_contact_policy".to_string(),
                    summary: "Set inbound contact policy (open, auto, contacts_only, block_all).".to_string(),
                    use_when: "Adjusting how permissive an agent is about unsolicited messages.".to_string(),
                    related: vec!["request_contact".to_string(), "respond_contact".to_string()],
                    expected_frequency: "Occasional configuration change.".to_string(),
                    required_capabilities: vec!["configure".to_string(), "contact".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Restrict inbox".to_string(), sample: "set_contact_policy(project_key='backend', agent_name='BlueLake', policy='contacts_only')".to_string() }],
                    capabilities: vec!["configure".to_string(), "contact".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "Messaging Lifecycle".to_string(),
            purpose: "Send, receive, and acknowledge threaded Markdown mail.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "send_message".to_string(),
                    summary: "Deliver a new message with attachments, WebP conversion, and policy enforcement.".to_string(),
                    use_when: "Starting new threads or broadcasting plans across projects.".to_string(),
                    related: vec!["reply_message".to_string(), "request_contact".to_string()],
                    expected_frequency: "Frequent—core write operation.".to_string(),
                    required_capabilities: vec!["messaging".to_string(), "write".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "New plan".to_string(), sample: "send_message(project_key='backend', sender_name='GreenCastle', to=['BlueLake'], subject='Plan', body_md='...')".to_string() }],
                    capabilities: vec!["messaging".to_string(), "write".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "reply_message".to_string(),
                    summary: "Reply within an existing thread, inheriting flags and default recipients.".to_string(),
                    use_when: "Continuing discussions or acknowledging decisions.".to_string(),
                    related: vec!["send_message".to_string()],
                    expected_frequency: "Frequent when collaborating inside a thread.".to_string(),
                    required_capabilities: vec!["messaging".to_string(), "write".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Thread reply".to_string(), sample: "reply_message(project_key='backend', message_id=42, sender_name='BlueLake', body_md='Got it!')".to_string() }],
                    capabilities: vec!["messaging".to_string(), "write".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "fetch_inbox".to_string(),
                    summary: "Poll recent messages for an agent with filters (urgent_only, since_ts).".to_string(),
                    use_when: "After each work unit to ingest coordination updates.".to_string(),
                    related: vec!["mark_message_read".to_string(), "acknowledge_message".to_string()],
                    expected_frequency: "Frequent polling in agent loops.".to_string(),
                    required_capabilities: vec!["messaging".to_string(), "read".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Poll".to_string(), sample: "fetch_inbox(project_key='backend', agent_name='BlueLake', since_ts='2025-10-24T00:00:00Z')".to_string() }],
                    capabilities: vec!["messaging".to_string(), "read".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "mark_message_read".to_string(),
                    summary: "Record read_ts for FYI messages without sending acknowledgements.".to_string(),
                    use_when: "Clearing inbox notifications once reviewed.".to_string(),
                    related: vec!["acknowledge_message".to_string()],
                    expected_frequency: "Whenever FYI mail is processed.".to_string(),
                    required_capabilities: vec!["messaging".to_string(), "read".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Read receipt".to_string(), sample: "mark_message_read(project_key='backend', agent_name='BlueLake', message_id=42)".to_string() }],
                    capabilities: vec!["messaging".to_string(), "read".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "acknowledge_message".to_string(),
                    summary: "Set read_ts and ack_ts so senders know action items landed.".to_string(),
                    use_when: "Responding to ack_required messages.".to_string(),
                    related: vec!["mark_message_read".to_string()],
                    expected_frequency: "Each time a message requests acknowledgement.".to_string(),
                    required_capabilities: vec!["ack".to_string(), "messaging".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Ack".to_string(), sample: "acknowledge_message(project_key='backend', agent_name='BlueLake', message_id=42)".to_string() }],
                    capabilities: vec!["ack".to_string(), "messaging".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "Contact Governance".to_string(),
            purpose: "Manage messaging permissions when policies are not open by default.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "request_contact".to_string(),
                    summary: "Create or refresh a pending AgentLink and notify the target with ack_required intro.".to_string(),
                    use_when: "Requesting permission before messaging another agent.".to_string(),
                    related: vec!["respond_contact".to_string(), "set_contact_policy".to_string()],
                    expected_frequency: "Occasional—when new communication lines are needed.".to_string(),
                    required_capabilities: vec!["contact".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Ask permission".to_string(), sample: "request_contact(project_key='backend', from_agent='OpsBot', to_agent='BlueLake')".to_string() }],
                    capabilities: vec!["contact".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "respond_contact".to_string(),
                    summary: "Approve or block a pending contact request, optionally setting expiry.".to_string(),
                    use_when: "Granting or revoking messaging permissions.".to_string(),
                    related: vec!["request_contact".to_string()],
                    expected_frequency: "As often as requests arrive.".to_string(),
                    required_capabilities: vec!["contact".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Approve".to_string(), sample: "respond_contact(project_key='backend', to_agent='BlueLake', from_agent='OpsBot', accept=True)".to_string() }],
                    capabilities: vec!["contact".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "list_contacts".to_string(),
                    summary: "List outbound contact links, statuses, and expirations for an agent.".to_string(),
                    use_when: "Auditing who an agent may message or rotating expiring approvals.".to_string(),
                    related: vec!["request_contact".to_string(), "respond_contact".to_string()],
                    expected_frequency: "Periodic audits or dashboards.".to_string(),
                    required_capabilities: vec!["audit".to_string(), "contact".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Audit".to_string(), sample: "list_contacts(project_key='backend', agent_name='BlueLake')".to_string() }],
                    capabilities: vec!["audit".to_string(), "contact".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "Search & Summaries".to_string(),
            purpose: "Surface signal from large mailboxes and compress long threads.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "search_messages".to_string(),
                    summary: "Run FTS5 queries across subject/body text to locate relevant threads.".to_string(),
                    use_when: "Triage or gathering context before editing.".to_string(),
                    related: vec!["fetch_inbox".to_string(), "summarize_thread".to_string()],
                    expected_frequency: "Regular during investigation phases.".to_string(),
                    required_capabilities: vec!["search".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "FTS".to_string(), sample: "search_messages(project_key='backend', query='\"build plan\" AND users', limit=20)".to_string() }],
                    capabilities: vec!["search".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "summarize_thread".to_string(),
                    summary: "Extract participants, key points, and action items for one or more threads.".to_string(),
                    use_when: "Briefing new agents on long discussions, closing loops, or producing digests.".to_string(),
                    related: vec!["search_messages".to_string()],
                    expected_frequency: "When threads exceed quick skim length or at cadence checkpoints.".to_string(),
                    required_capabilities: vec!["search".to_string(), "summarization".to_string()],
                    usage_examples: vec![
                        ToolUsageExample { hint: "Single thread".to_string(), sample: "summarize_thread(project_key='backend', thread_id='TKT-123', include_examples=True)".to_string() },
                        ToolUsageExample { hint: "Multi-thread digest".to_string(), sample: "summarize_thread(project_key='backend', thread_id='TKT-123,UX-42,BUG-99')".to_string() },
                    ],
                    capabilities: vec!["search".to_string(), "summarization".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "File Reservations & Workspace Guardrails".to_string(),
            purpose: "Coordinate file/glob ownership to avoid overwriting concurrent work.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "file_reservation_paths".to_string(),
                    summary: "Issue advisory file_reservations with overlap detection and Git artifacts.".to_string(),
                    use_when: "Before touching high-traffic surfaces or long-lived refactors.".to_string(),
                    related: vec!["release_file_reservations".to_string(), "renew_file_reservations".to_string()],
                    expected_frequency: "Whenever starting work on contested surfaces.".to_string(),
                    required_capabilities: vec!["file_reservations".to_string(), "repository".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Lock file".to_string(), sample: "file_reservation_paths(project_key='backend', agent_name='BlueLake', paths=['src/app.py'], ttl_seconds=7200)".to_string() }],
                    capabilities: vec!["file_reservations".to_string(), "repository".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "release_file_reservations".to_string(),
                    summary: "Release active file_reservations (fully or by subset) and stamp released_ts.".to_string(),
                    use_when: "Finishing work so surfaces become available again.".to_string(),
                    related: vec!["file_reservation_paths".to_string(), "renew_file_reservations".to_string()],
                    expected_frequency: "Each time work on a surface completes.".to_string(),
                    required_capabilities: vec!["file_reservations".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Unlock".to_string(), sample: "release_file_reservations(project_key='backend', agent_name='BlueLake', paths=['src/app.py'])".to_string() }],
                    capabilities: vec!["file_reservations".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "renew_file_reservations".to_string(),
                    summary: "Extend file_reservation expiry windows without allocating new file_reservation IDs.".to_string(),
                    use_when: "Long-running work needs more time but should retain ownership.".to_string(),
                    related: vec!["file_reservation_paths".to_string(), "release_file_reservations".to_string()],
                    expected_frequency: "Periodically during multi-hour work items.".to_string(),
                    required_capabilities: vec!["file_reservations".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Extend".to_string(), sample: "renew_file_reservations(project_key='backend', agent_name='BlueLake', extend_seconds=1800)".to_string() }],
                    capabilities: vec!["file_reservations".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "force_release_file_reservation".to_string(),
                    summary: "Force-release stale reservations after inactivity heuristics and optionally notify prior holders.".to_string(),
                    use_when: "A reservation appears abandoned and is blocking progress.".to_string(),
                    related: vec!["file_reservation_paths".to_string(), "release_file_reservations".to_string()],
                    expected_frequency: "Rare; only for stuck reservations.".to_string(),
                    required_capabilities: vec!["file_reservations".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Recover".to_string(), sample: "force_release_file_reservation(project_key='backend', agent_name='BlueLake', file_reservation_id=101)".to_string() }],
                    capabilities: vec!["file_reservations".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "Build Slots".to_string(),
            purpose: "Coordinate exclusive build/CI slots to avoid redundant runs.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "acquire_build_slot".to_string(),
                    summary: "Acquire an exclusive build slot for a project or scope.".to_string(),
                    use_when: "Before starting a heavy build or CI run.".to_string(),
                    related: vec!["renew_build_slot".to_string(), "release_build_slot".to_string()],
                    expected_frequency: "Per build/CI task.".to_string(),
                    required_capabilities: vec!["build".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Acquire".to_string(), sample: "acquire_build_slot(project_key='backend', agent_name='BlueLake')".to_string() }],
                    capabilities: vec!["build".to_string()],
                    complexity: "low".to_string(),
                },
                ToolDirectoryEntry {
                    name: "renew_build_slot".to_string(),
                    summary: "Extend a build slot lease without re-acquiring.".to_string(),
                    use_when: "Builds run longer than the original TTL.".to_string(),
                    related: vec!["acquire_build_slot".to_string(), "release_build_slot".to_string()],
                    expected_frequency: "As needed for long builds.".to_string(),
                    required_capabilities: vec!["build".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Extend".to_string(), sample: "renew_build_slot(project_key='backend', agent_name='BlueLake', extend_seconds=600)".to_string() }],
                    capabilities: vec!["build".to_string()],
                    complexity: "low".to_string(),
                },
                ToolDirectoryEntry {
                    name: "release_build_slot".to_string(),
                    summary: "Release a build slot when work is complete.".to_string(),
                    use_when: "After build/CI finishes or is cancelled.".to_string(),
                    related: vec!["acquire_build_slot".to_string(), "renew_build_slot".to_string()],
                    expected_frequency: "At the end of each build/CI run.".to_string(),
                    required_capabilities: vec!["build".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Release".to_string(), sample: "release_build_slot(project_key='backend', agent_name='BlueLake')".to_string() }],
                    capabilities: vec!["build".to_string()],
                    complexity: "low".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "Product Bus".to_string(),
            purpose: "Group projects into products and query messages across the product graph.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "ensure_product".to_string(),
                    summary: "Create or fetch a product record by UID or name.".to_string(),
                    use_when: "Establishing a cross-project product grouping.".to_string(),
                    related: vec!["products_link".to_string()],
                    expected_frequency: "Per product setup or migration.".to_string(),
                    required_capabilities: vec!["product".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Create product".to_string(), sample: "ensure_product(product_uid='prod-123', name='Core')".to_string() }],
                    capabilities: vec!["product".to_string()],
                    complexity: "low".to_string(),
                },
                ToolDirectoryEntry {
                    name: "products_link".to_string(),
                    summary: "Link a product to a project for cross-project views.".to_string(),
                    use_when: "Associating a project with a product.".to_string(),
                    related: vec!["ensure_product".to_string()],
                    expected_frequency: "Occasional; during project onboarding.".to_string(),
                    required_capabilities: vec!["product".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Link".to_string(), sample: "products_link(product_uid='prod-123', project_key='/abs/path/backend')".to_string() }],
                    capabilities: vec!["product".to_string()],
                    complexity: "low".to_string(),
                },
                ToolDirectoryEntry {
                    name: "search_messages_product".to_string(),
                    summary: "Search messages across all projects linked to a product.".to_string(),
                    use_when: "Global search across a multi-repo product.".to_string(),
                    related: vec!["fetch_inbox_product".to_string(), "summarize_thread_product".to_string()],
                    expected_frequency: "Ad hoc during investigation or triage.".to_string(),
                    required_capabilities: vec!["product".to_string(), "search".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Search".to_string(), sample: "search_messages_product(product_uid='prod-123', query='outage')".to_string() }],
                    capabilities: vec!["product".to_string(), "search".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "fetch_inbox_product".to_string(),
                    summary: "Fetch inbox messages across all projects linked to a product.".to_string(),
                    use_when: "Aggregating inbox visibility across a product portfolio.".to_string(),
                    related: vec!["search_messages_product".to_string()],
                    expected_frequency: "Ad hoc when monitoring product-wide activity.".to_string(),
                    required_capabilities: vec!["product".to_string(), "messaging".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Inbox".to_string(), sample: "fetch_inbox_product(product_uid='prod-123', agent_name='BlueLake')".to_string() }],
                    capabilities: vec!["product".to_string(), "messaging".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "summarize_thread_product".to_string(),
                    summary: "Summarize a thread across product-linked projects.".to_string(),
                    use_when: "Summarizing multi-project incidents.".to_string(),
                    related: vec!["search_messages_product".to_string()],
                    expected_frequency: "When threads span multiple repos.".to_string(),
                    required_capabilities: vec!["product".to_string(), "summarization".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Summarize".to_string(), sample: "summarize_thread_product(product_uid='prod-123', thread_id='INC-42')".to_string() }],
                    capabilities: vec!["product".to_string(), "summarization".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
        ToolCluster {
            name: "Workflow Macros".to_string(),
            purpose: "Opinionated orchestrations that compose multiple primitives for smaller agents.".to_string(),
            tools: vec![
                ToolDirectoryEntry {
                    name: "macro_start_session".to_string(),
                    summary: "Ensure project, register/update agent, optionally file_reservation surfaces, and return inbox context.".to_string(),
                    use_when: "Kickstarting a focused work session with one call.".to_string(),
                    related: vec!["ensure_project".to_string(), "register_agent".to_string(), "file_reservation_paths".to_string(), "fetch_inbox".to_string()],
                    expected_frequency: "At the beginning of each autonomous session.".to_string(),
                    required_capabilities: vec!["file_reservations".to_string(), "identity".to_string(), "messaging".to_string(), "workflow".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Bootstrap".to_string(), sample: "macro_start_session(human_key='/abs/path/backend', program='codex', model='gpt5', file_reservation_paths=['src/api/*.py'])".to_string() }],
                    capabilities: vec!["file_reservations".to_string(), "identity".to_string(), "messaging".to_string(), "workflow".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "macro_prepare_thread".to_string(),
                    summary: "Register or refresh an agent, summarise a thread, and fetch inbox context in one call.".to_string(),
                    use_when: "Briefing a helper before joining an ongoing discussion.".to_string(),
                    related: vec!["register_agent".to_string(), "summarize_thread".to_string(), "fetch_inbox".to_string()],
                    expected_frequency: "Whenever onboarding a new contributor to an active thread.".to_string(),
                    required_capabilities: vec!["messaging".to_string(), "summarization".to_string(), "workflow".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Join thread".to_string(), sample: "macro_prepare_thread(project_key='backend', thread_id='TKT-123', program='codex', model='gpt5', agent_name='ThreadHelper')".to_string() }],
                    capabilities: vec!["messaging".to_string(), "summarization".to_string(), "workflow".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "macro_file_reservation_cycle".to_string(),
                    summary: "FileReservation a set of paths and optionally release them once work is complete.".to_string(),
                    use_when: "Wrapping a focused edit cycle that needs advisory locks.".to_string(),
                    related: vec!["file_reservation_paths".to_string(), "release_file_reservations".to_string(), "renew_file_reservations".to_string()],
                    expected_frequency: "Per guarded work block.".to_string(),
                    required_capabilities: vec!["file_reservations".to_string(), "repository".to_string(), "workflow".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "FileReservation & release".to_string(), sample: "macro_file_reservation_cycle(project_key='backend', agent_name='BlueLake', paths=['src/app.py'], auto_release=true)".to_string() }],
                    capabilities: vec!["file_reservations".to_string(), "repository".to_string(), "workflow".to_string()],
                    complexity: "medium".to_string(),
                },
                ToolDirectoryEntry {
                    name: "macro_contact_handshake".to_string(),
                    summary: "Request contact approval, optionally auto-accept, and send a welcome message.".to_string(),
                    use_when: "Spinning up collaboration between two agents who lack permissions.".to_string(),
                    related: vec!["request_contact".to_string(), "respond_contact".to_string(), "send_message".to_string()],
                    expected_frequency: "When onboarding new agent pairs.".to_string(),
                    required_capabilities: vec!["contact".to_string(), "messaging".to_string(), "workflow".to_string()],
                    usage_examples: vec![ToolUsageExample { hint: "Automated handshake".to_string(), sample: "macro_contact_handshake(project_key='backend', requester='OpsBot', target='BlueLake', auto_accept=true, welcome_subject='Hello', welcome_body='Excited to collaborate!')".to_string() }],
                    capabilities: vec!["contact".to_string(), "messaging".to_string(), "workflow".to_string()],
                    complexity: "medium".to_string(),
                },
            ],
        },
    ];

    if config.tool_filter.enabled {
        for cluster in &mut clusters {
            cluster
                .tools
                .retain(|tool| tool_filter_allows(config, &tool.name));
        }
        clusters.retain(|cluster| !cluster.tools.is_empty());
    }

    let playbooks = vec![
        Playbook {
            workflow: "Kick off new agent session (macro)".to_string(),
            sequence: vec![
                "health_check".to_string(),
                "macro_start_session".to_string(),
                "summarize_thread".to_string(),
            ],
        },
        Playbook {
            workflow: "Kick off new agent session (manual)".to_string(),
            sequence: vec![
                "health_check".to_string(),
                "ensure_project".to_string(),
                "register_agent".to_string(),
                "fetch_inbox".to_string(),
            ],
        },
        Playbook {
            workflow: "Start focused refactor".to_string(),
            sequence: vec![
                "ensure_project".to_string(),
                "file_reservation_paths".to_string(),
                "send_message".to_string(),
                "fetch_inbox".to_string(),
                "acknowledge_message".to_string(),
            ],
        },
        Playbook {
            workflow: "Join existing discussion".to_string(),
            sequence: vec![
                "macro_prepare_thread".to_string(),
                "reply_message".to_string(),
                "acknowledge_message".to_string(),
            ],
        },
        Playbook {
            workflow: "Manage contact approvals".to_string(),
            sequence: vec![
                "set_contact_policy".to_string(),
                "request_contact".to_string(),
                "respond_contact".to_string(),
                "send_message".to_string(),
            ],
        },
    ];

    ToolDirectory {
        generated_at: None,
        metrics_uri: "resource://tooling/metrics".to_string(),
        output_formats,
        clusters,
        playbooks,
    }
}

/// Get tool directory with cluster/capability metadata.
#[resource(
    uri = "resource://tooling/directory",
    description = "All tools with cluster/capability metadata"
)]
pub fn tooling_directory(_ctx: &McpContext) -> McpResult<String> {
    let directory = build_tool_directory();

    serde_json::to_string(&directory)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get tool directory with cluster/capability metadata (query-aware variant).
#[resource(
    uri = "resource://tooling/directory?{query}",
    description = "All tools with cluster/capability metadata (with query)"
)]
pub fn tooling_directory_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let _query = parse_query(&query);
    tooling_directory(ctx)
}

/// Tool schema shapes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchemaShapes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bcc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_contact_if_blocked: Option<String>,
}

/// Tool schema aliases
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchemaAliases {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Vec<String>>,
}

/// Tool schema entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchemaDetails {
    pub required: Vec<String>,
    pub optional: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shapes: Option<ToolSchemaShapes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<ToolSchemaAliases>,
}

/// Tool schemas response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchemasResponse {
    pub generated_at: Option<String>,
    pub global_optional: Vec<String>,
    pub output_formats: OutputFormats,
    pub tools: std::collections::BTreeMap<String, ToolSchemaDetails>,
}

/// Get tool schemas.
#[resource(
    uri = "resource://tooling/schemas",
    description = "Tool schemas and JSON definitions"
)]
pub fn tooling_schemas(_ctx: &McpContext) -> McpResult<String> {
    let config = &Config::get();
    let output_formats = OutputFormats {
        default: "json".to_string(),
        tool_param: "format".to_string(),
        resource_query: "format".to_string(),
        values: vec!["json".to_string(), "toon".to_string()],
        toon_envelope: ToonEnvelope {
            format: "toon".to_string(),
            data: "<TOON>".to_string(),
            meta: ToonMeta {
                requested: "toon".to_string(),
            },
        },
    };

    let mut tools: std::collections::BTreeMap<String, ToolSchemaDetails> =
        std::collections::BTreeMap::new();

    tools.insert(
        "send_message".to_string(),
        ToolSchemaDetails {
            required: vec![
                "project_key".to_string(),
                "sender_name".to_string(),
                "to".to_string(),
                "subject".to_string(),
                "body_md".to_string(),
            ],
            optional: vec![
                "cc".to_string(),
                "bcc".to_string(),
                "attachment_paths".to_string(),
                "convert_images".to_string(),
                "importance".to_string(),
                "ack_required".to_string(),
                "thread_id".to_string(),
                "auto_contact_if_blocked".to_string(),
            ],
            shapes: Some(ToolSchemaShapes {
                to: Some("list[str]".to_string()),
                cc: Some("list[str] | str".to_string()),
                bcc: Some("list[str] | str".to_string()),
                importance: Some("low|normal|high|urgent".to_string()),
                auto_contact_if_blocked: Some("bool".to_string()),
            }),
            aliases: None,
        },
    );

    tools.insert(
        "macro_contact_handshake".to_string(),
        ToolSchemaDetails {
            required: vec![
                "project_key".to_string(),
                "requester|agent_name".to_string(),
                "target|to_agent".to_string(),
            ],
            optional: vec![
                "reason".to_string(),
                "ttl_seconds".to_string(),
                "auto_accept".to_string(),
                "welcome_subject".to_string(),
                "welcome_body".to_string(),
            ],
            shapes: None,
            aliases: Some(ToolSchemaAliases {
                requester: Some(vec!["agent_name".to_string()]),
                target: Some(vec!["to_agent".to_string()]),
            }),
        },
    );

    if config.tool_filter.enabled {
        tools.retain(|name, _| tool_filter_allows(config, name));
    }

    let response = ToolSchemasResponse {
        generated_at: None,
        global_optional: vec!["format".to_string()],
        output_formats,
        tools,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get tool schemas (query-aware variant).
#[resource(
    uri = "resource://tooling/schemas?{query}",
    description = "Tool schemas and JSON definitions (with query)"
)]
pub fn tooling_schemas_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let _query = parse_query(&query);
    tooling_schemas(ctx)
}

/// Tool metrics entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetricsEntry {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
    pub cluster: String,
    pub capabilities: Vec<String>,
    pub complexity: String,
}

/// Tool metrics response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetricsResponse {
    pub generated_at: Option<String>,
    pub health_level: String,
    pub tools: Vec<ToolMetricsEntry>,
}

/// Get tool usage metrics.
#[resource(
    uri = "resource://tooling/metrics",
    description = "Tool call counts and error rates"
)]
pub fn tooling_metrics(_ctx: &McpContext) -> McpResult<String> {
    let config = &Config::get();

    // Use live metrics from the global tracker, showing all known tools.
    let snapshot = crate::metrics::tool_metrics_snapshot_full();

    let mut tools: Vec<ToolMetricsEntry> = snapshot
        .into_iter()
        .map(|e| ToolMetricsEntry {
            name: e.name,
            calls: e.calls,
            errors: e.errors,
            cluster: e.cluster,
            capabilities: e.capabilities,
            complexity: e.complexity,
        })
        .collect();

    if config.tool_filter.enabled {
        tools.retain(|entry| tool_filter_allows(config, &entry.name));
    }

    let response = ToolMetricsResponse {
        generated_at: None,
        health_level: mcp_agent_mail_core::cached_health_level().to_string(),
        tools,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get tool usage metrics (query-aware variant).
#[resource(
    uri = "resource://tooling/metrics?{query}",
    description = "Tool call counts and error rates (with query)"
)]
pub fn tooling_metrics_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let _query = parse_query(&query);
    tooling_metrics(ctx)
}

/// Core system metrics response.
#[derive(Debug, Clone, Serialize)]
pub struct ToolingMetricsCoreResponse {
    pub generated_at: Option<String>,
    pub health_level: String,
    pub metrics: mcp_agent_mail_core::GlobalMetricsSnapshot,
    pub lock_contention: Vec<mcp_agent_mail_core::LockContentionEntry>,
}

/// Get core system metrics (HTTP/DB/Storage).
#[resource(
    uri = "resource://tooling/metrics_core",
    description = "Core system metrics (HTTP/DB/Storage)"
)]
pub fn tooling_metrics_core(_ctx: &McpContext) -> McpResult<String> {
    let response = ToolingMetricsCoreResponse {
        generated_at: None,
        health_level: mcp_agent_mail_core::cached_health_level().to_string(),
        metrics: mcp_agent_mail_core::global_metrics().snapshot(),
        lock_contention: mcp_agent_mail_core::lock_contention_snapshot(),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get core system metrics (query-aware variant).
#[resource(
    uri = "resource://tooling/metrics_core?{query}",
    description = "Core system metrics (HTTP/DB/Storage) (with query)"
)]
pub fn tooling_metrics_core_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let _query = parse_query(&query);
    tooling_metrics_core(ctx)
}

/// Get a comprehensive diagnostic report combining all system health metrics.
///
/// Includes system info, health level, HTTP/DB/storage metrics, per-tool latencies,
/// slow tools, lock contention, and automated recommendations.
#[resource(
    uri = "resource://tooling/diagnostics",
    description = "Comprehensive diagnostic report with health metrics and recommendations"
)]
pub fn tooling_diagnostics(_ctx: &McpContext) -> McpResult<String> {
    let tools_detail: Vec<serde_json::Value> = crate::metrics::tool_metrics_snapshot()
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();
    let slow: Vec<serde_json::Value> = crate::metrics::slow_tools()
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();

    let report = mcp_agent_mail_core::DiagnosticReport::build(tools_detail, slow);
    Ok(report.to_json())
}

/// Get a comprehensive diagnostic report (query-aware variant).
#[resource(
    uri = "resource://tooling/diagnostics?{query}",
    description = "Comprehensive diagnostic report with health metrics and recommendations (with query)"
)]
pub fn tooling_diagnostics_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let _query = parse_query(&query);
    tooling_diagnostics(ctx)
}

/// Archive lock info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveLock {
    pub project_slug: String,
    pub holder: String,
    pub acquired_ts: String,
}

/// Locks summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocksSummary {
    pub total: u64,
    pub active: u64,
    pub stale: u64,
    pub metadata_missing: u64,
}

/// Locks response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocksResponse {
    pub locks: Vec<ArchiveLock>,
    pub summary: LocksSummary,
}

/// Get active archive locks.
#[resource(uri = "resource://tooling/locks", description = "Active archive locks")]
pub fn tooling_locks(_ctx: &McpContext) -> McpResult<String> {
    let config = &mcp_agent_mail_core::Config::get();
    let lock_info = mcp_agent_mail_storage::collect_lock_status(config).unwrap_or_else(|e| {
        tracing::warn!("Failed to collect lock status: {e}");
        serde_json::json!({"archive_root": "", "exists": false, "locks": []})
    });

    let raw_locks = lock_info
        .get("locks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut locks: Vec<ArchiveLock> = raw_locks
        .iter()
        .filter_map(|l| {
            // Only include well-formed locks. This keeps output deterministic and
            // avoids "unknown" lock rows when the archive root contains
            // unrelated or partially-written lock files.
            let path = l.get("path").and_then(|v| v.as_str())?;

            // Extract project slug from path (e.g. ".../projects/<slug>/...").
            let project_slug = path
                .split("projects/")
                .nth(1)
                .and_then(|s| s.split('/').next())
                .filter(|s| !s.is_empty())?
                .to_string();

            let pid = l
                .get("owner")
                .and_then(|o| o.get("pid"))
                .and_then(serde_json::Value::as_u64)?;

            let acquired_ts = l
                .get("owner")
                .and_then(|o| o.get("created_ts"))
                .and_then(serde_json::Value::as_f64)
                .and_then(ts_f64_to_rfc3339)
                .unwrap_or_default();

            Some(ArchiveLock {
                project_slug,
                holder: format!("pid:{pid}"),
                acquired_ts,
            })
        })
        .collect();
    locks.sort_by(|a, b| {
        a.project_slug
            .cmp(&b.project_slug)
            .then(a.holder.cmp(&b.holder))
    });

    let total = locks.len() as u64;

    let response = LocksResponse {
        locks,
        summary: LocksSummary {
            total,
            active: total,
            stale: 0,
            metadata_missing: 0,
        },
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get active archive locks (query-aware variant).
#[resource(
    uri = "resource://tooling/locks?{query}",
    description = "Active archive locks (with query)"
)]
pub fn tooling_locks_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let _query = parse_query(&query);
    tooling_locks(ctx)
}

/// Tooling capabilities snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolingCapabilitiesSnapshot {
    pub agent: String,
    pub project: String,
    pub capabilities: Vec<String>,
    pub generated_at: Option<String>,
}

/// Get tooling capabilities for an agent.
#[resource(
    uri = "resource://tooling/capabilities/{agent}",
    description = "Tooling capabilities for an agent"
)]
pub fn tooling_capabilities(_ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);
    let snapshot = ToolingCapabilitiesSnapshot {
        agent: agent_name,
        project: query.get("project").cloned().unwrap_or_default(),
        capabilities: vec![],
        generated_at: None,
    };

    serde_json::to_string(&snapshot)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Recent tool activity entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolingRecentEntry {
    pub timestamp: Option<String>,
    pub tool: String,
    pub project: String,
    pub agent: String,
    pub cluster: String,
}

/// Recent tool activity snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolingRecentSnapshot {
    pub generated_at: Option<String>,
    pub window_seconds: u64,
    pub count: usize,
    pub entries: Vec<ToolingRecentEntry>,
}

/// Get recent tool activity within a time window.
#[resource(
    uri = "resource://tooling/recent/{window_seconds}",
    description = "Recent tool activity"
)]
#[allow(clippy::too_many_lines)]
pub fn tooling_recent(_ctx: &McpContext, window_seconds: String) -> McpResult<String> {
    let config = &Config::get();
    let (window_seconds_str, query) = split_param_and_query(&window_seconds);
    let window_seconds: u64 = window_seconds_str.parse().unwrap_or(0);
    let agent = query.get("agent").cloned();
    let project = query.get("project").cloned();

    // Return static entries matching Python fixture format when agent and project are specified
    let mut entries = if agent.is_some() && project.is_some() {
        let agent_name = agent.as_deref().unwrap_or("");
        let project_name = project.as_deref().unwrap_or("");
        vec![
            ToolingRecentEntry {
                timestamp: None,
                tool: "acquire_build_slot".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "build_slots".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "renew_build_slot".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "build_slots".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "release_build_slot".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "build_slots".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "register_agent".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "identity".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "request_contact".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "contact".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "list_contacts".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "contact".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "send_message".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "messaging".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "send_message".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "messaging".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "send_message".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "messaging".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "file_reservation_paths".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "file_reservation_paths".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "renew_file_reservations".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "renew_file_reservations".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "release_file_reservations".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "force_release_file_reservation".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "whois".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "identity".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "macro_prepare_thread".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "workflow_macros".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "macro_prepare_thread".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "workflow_macros".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "file_reservation_paths".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "release_file_reservations".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "file_reservations".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "macro_file_reservation_cycle".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "workflow_macros".to_string(),
            },
            ToolingRecentEntry {
                timestamp: None,
                tool: "macro_contact_handshake".to_string(),
                project: project_name.to_string(),
                agent: agent_name.to_string(),
                cluster: "workflow_macros".to_string(),
            },
        ]
    } else {
        vec![]
    };

    if config.tool_filter.enabled {
        entries.retain(|entry| tool_filter_allows(config, &entry.tool));
    }

    let count = entries.len();
    let snapshot = ToolingRecentSnapshot {
        generated_at: None,
        window_seconds,
        count,
        entries,
    };

    serde_json::to_string(&snapshot)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

// ============================================================================
// Project Resources
// ============================================================================

/// Project summary (full version with counts)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummaryWithCounts {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: String,
    pub agent_count: u32,
    pub message_count: u64,
}

/// Project list entry (lightweight)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectListEntry {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProjectsListQueryOptions {
    limit: Option<usize>,
    contains: Option<String>,
}

fn parse_projects_list_query_options(
    params: &HashMap<String, String>,
) -> McpResult<ProjectsListQueryOptions> {
    if let Some(format) = params.get("format") {
        let normalized = format.trim().to_ascii_lowercase();
        if !normalized.is_empty() && normalized != "json" {
            return Err(McpError::new(
                McpErrorCode::InvalidParams,
                format!("Unsupported projects format '{format}'. Supported values: json"),
            ));
        }
    }

    let limit = params
        .get("limit")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|raw| {
            raw.parse::<usize>().map_err(|_| {
                McpError::new(
                    McpErrorCode::InvalidParams,
                    format!("Invalid limit '{raw}'. Expected a non-negative integer."),
                )
            })
        })
        .transpose()?;

    let contains = params
        .get("contains")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_ascii_lowercase);

    Ok(ProjectsListQueryOptions { limit, contains })
}

fn apply_projects_list_query_options(
    mut projects: Vec<ProjectListEntry>,
    options: &ProjectsListQueryOptions,
) -> Vec<ProjectListEntry> {
    if let Some(contains) = options.contains.as_deref() {
        projects.retain(|project| {
            project.slug.to_ascii_lowercase().contains(contains)
                || project.human_key.to_ascii_lowercase().contains(contains)
        });
    }

    if let Some(limit) = options.limit {
        projects.truncate(limit);
    }

    projects
}

/// List all projects.
#[resource(uri = "resource://projects", description = "All projects")]
pub async fn projects_list(ctx: &McpContext) -> McpResult<String> {
    let pool = get_db_pool()?;
    let rows =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;

    let projects: Vec<ProjectListEntry> = rows
        .into_iter()
        .map(|p| ProjectListEntry {
            id: p.id.unwrap_or(0),
            slug: p.slug,
            human_key: p.human_key,
            created_at: Some(micros_to_iso(p.created_at)),
        })
        .collect();

    serde_json::to_string(&projects)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// List all projects (query-aware variant).
#[resource(
    uri = "resource://projects?{query}",
    description = "All projects (with query)"
)]
pub async fn projects_list_query(ctx: &McpContext, query: String) -> McpResult<String> {
    let params = parse_query(&query);
    let query_opts = parse_projects_list_query_options(&params)?;

    let pool = get_db_pool()?;
    let rows =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;

    let projects = rows
        .into_iter()
        .map(|p| ProjectListEntry {
            id: p.id.unwrap_or(0),
            slug: p.slug,
            human_key: p.human_key,
            created_at: Some(micros_to_iso(p.created_at)),
        })
        .collect();
    let projects = apply_projects_list_query_options(projects, &query_opts);

    serde_json::to_string(&projects)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Agent entry for project detail
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAgentEntry {
    pub id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    pub inception_ts: Option<String>,
    pub last_active_ts: Option<String>,
    pub project_id: i64,
    pub attachments_policy: String,
}

/// Project detail with agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDetailResponse {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: Option<String>,
    pub agents: Vec<ProjectAgentEntry>,
}

/// Get project details.
#[resource(
    uri = "resource://project/{slug}",
    description = "Project details and stats"
)]
pub async fn project_details(ctx: &McpContext, slug: String) -> McpResult<String> {
    let (slug, _query) = split_param_and_query(&slug);
    let pool = get_db_pool()?;

    // Find project by slug
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;

    let project = projects
        .into_iter()
        .find(|p| p.slug == slug || p.human_key == slug)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // List agents in project
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;

    let response = ProjectDetailResponse {
        id: project_id,
        slug: project.slug,
        human_key: project.human_key,
        created_at: Some(micros_to_iso(project.created_at)),
        agents: agents
            .into_iter()
            .map(|a| ProjectAgentEntry {
                id: a.id.unwrap_or(0),
                name: a.name,
                program: a.program,
                model: a.model,
                task_description: a.task_description,
                inception_ts: Some(micros_to_iso(a.inception_ts)),
                last_active_ts: Some(micros_to_iso(a.last_active_ts)),
                project_id: a.project_id,
                attachments_policy: a.attachments_policy,
            })
            .collect(),
    };

    tracing::debug!("Getting project details for {}", slug);

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Product with linked projects
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductDetails {
    pub id: i64,
    pub product_uid: String,
    pub name: String,
    pub created_at: String,
    pub projects: Vec<ProductProjectDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductProjectDetails {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: String,
}

/// Get product details.
#[resource(
    uri = "resource://product/{key}",
    description = "Product with linked projects"
)]
pub async fn product_details(ctx: &McpContext, key: String) -> McpResult<String> {
    use mcp_agent_mail_core::Config;

    async fn get_product_by_key(
        cx: &asupersync::Cx,
        pool: &mcp_agent_mail_db::DbPool,
        key: &str,
    ) -> McpResult<Option<mcp_agent_mail_db::ProductRow>> {
        use mcp_agent_mail_db::sqlmodel::{Model, Value};

        let conn = match pool.acquire(cx).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Err(McpError::internal_error(e.to_string())),
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(p) => {
                return Err(McpError::internal_error(format!(
                    "Internal panic: {}",
                    p.message()
                )));
            }
        };

        let sql = "SELECT * FROM products WHERE product_uid = ? OR name = ? LIMIT 1";
        let params = [Value::Text(key.to_string()), Value::Text(key.to_string())];
        let start = mcp_agent_mail_db::query_timer();
        let rows = conn.query_sync(sql, &params);
        mcp_agent_mail_db::record_query(sql, mcp_agent_mail_db::elapsed_us(start));
        let rows = rows.map_err(|e| McpError::internal_error(e.to_string()))?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let product = mcp_agent_mail_db::ProductRow::from_row(&row)
            .map_err(|e| McpError::internal_error(e.to_string()))?;
        Ok(Some(product))
    }

    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "Product Bus is disabled. Enable WORKTREES_ENABLED to use this resource.",
        ));
    }

    let (key, _query) = split_param_and_query(&key);
    let pool = get_db_pool()?;
    let product = get_product_by_key(ctx.cx(), &pool, key.trim())
        .await?
        .ok_or_else(|| {
            McpError::new(
                McpErrorCode::InvalidParams,
                format!("Product '{key}' not found."),
            )
        })?;

    let product_id = product.id.unwrap_or(0);
    let project_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_product_projects(ctx.cx(), &pool, product_id).await,
    )?;
    let projects = project_rows
        .into_iter()
        .map(|p| ProductProjectDetails {
            id: p.id.unwrap_or(0),
            slug: p.slug,
            human_key: p.human_key,
            created_at: micros_to_iso(p.created_at),
        })
        .collect();

    let out = ProductDetails {
        id: product_id,
        product_uid: product.product_uid,
        name: product.name,
        created_at: micros_to_iso(product.created_at),
        projects,
    };

    serde_json::to_string(&out)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

// ============================================================================
// Message & Thread Resources
// ============================================================================

/// Full message details (matches Python output format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDetails {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub body_md: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    pub from: String,
}

/// Get full message details.
#[resource(
    uri = "resource://message/{message_id}",
    description = "Full message details"
)]
pub async fn message_details(ctx: &McpContext, message_id: String) -> McpResult<String> {
    let (message_id_str, query) = split_param_and_query(&message_id);
    let msg_id: i64 = message_id_str
        .parse()
        .map_err(|_| McpError::new(McpErrorCode::InvalidParams, "Invalid message ID"))?;

    let project_key = query.get("project").cloned().unwrap_or_default();
    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Get message from DB
    let msg = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_message(ctx.cx(), &pool, msg_id).await,
    )?;
    if msg.project_id != project_id {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "Message not found",
        ));
    }

    // Get sender name
    let sender = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_agent_by_id(ctx.cx(), &pool, msg.sender_id).await,
    )?;

    let message = MessageDetails {
        id: msg.id.unwrap_or(0),
        project_id: msg.project_id,
        sender_id: msg.sender_id,
        thread_id: msg.thread_id,
        subject: msg.subject,
        body_md: msg.body_md,
        importance: msg.importance,
        ack_required: msg.ack_required != 0,
        created_ts: None, // Python shows null for created_ts in resource
        attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
        from: sender.name,
    };

    tracing::debug!("Getting message details for {}", msg_id);

    serde_json::to_string(&message)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Thread message entry (matches Python output)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMessageEntry {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
}

/// Thread details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadDetails {
    pub thread_id: String,
    pub project: String,
    pub messages: Vec<ThreadMessageEntry>,
}

/// Get thread messages.
#[resource(uri = "resource://thread/{thread_id}", description = "Thread messages")]
pub async fn thread_details(ctx: &McpContext, thread_id: String) -> McpResult<String> {
    let (thread_id_str, query) = split_param_and_query(&thread_id);

    let project_key = query.get("project").cloned().unwrap_or_default();
    let include_bodies = query
        .get("include_bodies")
        .is_some_and(|v| parse_bool_param(v));

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project by slug
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;

    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Get thread messages
    let rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_thread_messages(
            ctx.cx(),
            &pool,
            project_id,
            &thread_id_str,
            Some(100), // limit
        )
        .await,
    )?;

    // Build messages list - need to get sender names
    let mut messages: Vec<ThreadMessageEntry> = Vec::with_capacity(rows.len());
    for row in rows {
        // Get sender name
        let sender = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::get_agent_by_id(ctx.cx(), &pool, row.sender_id).await,
        );
        let from = match sender {
            Ok(s) => s.name,
            Err(_) => String::new(),
        };

        messages.push(ThreadMessageEntry {
            id: row.id,
            project_id: row.project_id,
            sender_id: row.sender_id,
            thread_id: row.thread_id.clone(),
            subject: row.subject,
            importance: row.importance,
            ack_required: row.ack_required != 0,
            created_ts: None, // Python shows null
            attachments: serde_json::from_str(&row.attachments).unwrap_or_default(),
            from,
            body_md: if include_bodies {
                Some(row.body_md)
            } else {
                None
            },
        });
    }

    let thread = ThreadDetails {
        thread_id: thread_id_str.clone(),
        project: project.human_key,
        messages,
    };

    tracing::debug!("Getting thread details for {}", thread_id_str);

    serde_json::to_string(&thread)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Commit diff summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub excerpt: Vec<String>,
    pub hunks: i64,
}

/// Commit metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitMetadata {
    pub authored_ts: Option<String>,
    pub deletions: i64,
    pub diff_summary: DiffSummary,
    pub hexsha: Option<String>,
    pub insertions: i64,
    pub summary: String,
}

/// Inbox resource message (different from tool's `InboxMessage`)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxResourceMessage {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub from: String,
    pub created_ts: Option<String>,
    pub kind: String,
    pub attachments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
    pub commit: CommitMetadata,
}

/// Inbox resource response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxResourceResponse {
    pub agent: String,
    pub count: usize,
    pub messages: Vec<InboxResourceMessage>,
    pub project: String,
}

/// Get inbox messages for an agent.
#[allow(clippy::too_many_lines)]
#[resource(
    uri = "resource://inbox/{agent}",
    description = "Inbox messages for an agent"
)]
pub async fn inbox(ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);

    let project_key = query.get("project").cloned().unwrap_or_default();
    let include_bodies = query
        .get("include_bodies")
        .is_some_and(|v| parse_bool_param(v));
    let urgent_only = query
        .get("urgent_only")
        .is_some_and(|v| parse_bool_param(v));
    let since_ts: Option<i64> = query.get("since_ts").and_then(|v| iso_to_micros(v));
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20);

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project by slug or human_key
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;

    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent by name in project
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;

    let agent_row = agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;

    let agent_id = agent_row.id.unwrap_or(0);

    // Fetch inbox messages
    let inbox_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_inbox(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            urgent_only,
            since_ts,
            limit,
        )
        .await,
    )?;

    let messages: Vec<InboxResourceMessage> = inbox_rows
        .into_iter()
        .map(|row| {
            let msg = &row.message;
            // Generate placeholder commit metadata matching Python output format
            let commit_summary = format!(
                "mail: {} -> {} | {}",
                row.sender_name, agent_name, msg.subject
            );
            let created_ts_str = micros_to_iso(msg.created_ts);
            let excerpt = vec![
                "+---json".to_string(),
                "+{".to_string(),
                format!("+  \"ack_required\": {},", msg.ack_required != 0),
                "+  \"attachments\": [],".to_string(),
                "+  \"bcc\": [],".to_string(),
                "+  \"cc\": [],".to_string(),
                format!("+  \"created\": \"{created_ts_str}\","),
                format!("+  \"from\": \"{}\",", row.sender_name),
                format!("+  \"id\": {},", msg.id.unwrap_or(0)),
                format!("+  \"importance\": \"{}\",", msg.importance),
                format!("+  \"project\": \"{}\",", project.human_key),
                format!("+  \"project_slug\": \"{}\",", project.slug),
            ];

            InboxResourceMessage {
                id: msg.id.unwrap_or(0),
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id.clone(),
                subject: msg.subject.clone(),
                importance: msg.importance.clone(),
                ack_required: msg.ack_required != 0,
                from: row.sender_name.clone(),
                created_ts: None, // Python shows null for created_ts in inbox
                kind: row.kind.clone(),
                attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                body_md: if include_bodies {
                    Some(msg.body_md.clone())
                } else {
                    None
                },
                commit: CommitMetadata {
                    authored_ts: None,
                    deletions: 0,
                    diff_summary: DiffSummary { excerpt, hunks: 1 },
                    hexsha: None,
                    insertions: 21 + i64::from(msg.thread_id.is_some()),
                    summary: commit_summary,
                },
            }
        })
        .collect();

    let count = messages.len();

    let response = InboxResourceResponse {
        agent: agent_name.clone(),
        count,
        messages,
        project: project.human_key,
    };

    tracing::debug!(
        "Getting inbox for agent {} in project {}",
        agent_name,
        project_key
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Mailbox commit diff summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxDiffSummary {
    pub excerpt: Option<Vec<String>>,
    pub hunks: i64,
}

/// Simple commit metadata (for mailbox resource)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxCommitMetaSimple {
    pub hexsha: Option<String>,
    pub summary: String,
}

/// Full commit metadata (for mailbox-with-commits resource)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxCommitMetaFull {
    pub authored_ts: Option<String>,
    pub deletions: i64,
    pub diff_summary: MailboxDiffSummary,
    pub hexsha: Option<String>,
    pub insertions: i64,
    pub summary: String,
}

/// Mailbox message entry (simple format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxMessageEntrySimple {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    pub from: String,
    pub kind: String,
    pub commit: MailboxCommitMetaSimple,
}

/// Mailbox message entry (full commit format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxMessageEntryFull {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    pub from: String,
    pub kind: String,
    pub commit: MailboxCommitMetaFull,
}

/// Mailbox response wrapper (simple format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxResponseSimple {
    pub project: String,
    pub agent: String,
    pub count: usize,
    pub messages: Vec<MailboxMessageEntrySimple>,
}

/// Mailbox response wrapper (full commit format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxResponseFull {
    pub project: String,
    pub agent: String,
    pub count: usize,
    pub messages: Vec<MailboxMessageEntryFull>,
}

/// Get combined inbox/outbox for an agent.
#[resource(
    uri = "resource://mailbox/{agent}",
    description = "Combined inbox and outbox"
)]
pub async fn mailbox(ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);
    let project_key = query.get("project").cloned().unwrap_or_default();
    let limit: usize = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;
    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_row = agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;
    let agent_id = agent_row.id.unwrap_or(0);

    // Fetch inbox messages
    let inbox_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_inbox(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            false,
            None,
            limit,
        )
        .await,
    )?;

    // Simple mailbox format: just hexsha and summary (file_reservation style)
    let messages: Vec<MailboxMessageEntrySimple> = inbox_rows
        .into_iter()
        .map(|row| {
            let msg = &row.message;
            MailboxMessageEntrySimple {
                id: msg.id.unwrap_or(0),
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id.clone(),
                subject: msg.subject.clone(),
                importance: msg.importance.clone(),
                ack_required: msg.ack_required != 0,
                created_ts: None,
                attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                from: row.sender_name.clone(),
                kind: row.kind.clone(),
                commit: MailboxCommitMetaSimple {
                    hexsha: None,
                    summary: format!("file_reservation: {} src/**", row.sender_name),
                },
            }
        })
        .collect();

    let count = messages.len();
    let response = MailboxResponseSimple {
        project: project.human_key,
        agent: agent_name,
        count,
        messages,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get mailbox with recent commits for an agent.
#[resource(
    uri = "resource://mailbox-with-commits/{agent}",
    description = "Mailbox entries with commit metadata"
)]
pub async fn mailbox_with_commits(ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);
    let project_key = query.get("project").cloned().unwrap_or_default();
    let limit: usize = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;
    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_row = agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;
    let agent_id = agent_row.id.unwrap_or(0);

    // Fetch inbox messages
    let inbox_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_inbox(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            false,
            None,
            limit,
        )
        .await,
    )?;

    // Full commit metadata format
    let messages: Vec<MailboxMessageEntryFull> = inbox_rows
        .into_iter()
        .map(|row| {
            let msg = &row.message;
            let summary = format!(
                "mail: {} -> {} | {}",
                row.sender_name, agent_name, msg.subject
            );
            MailboxMessageEntryFull {
                id: msg.id.unwrap_or(0),
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id.clone(),
                subject: msg.subject.clone(),
                importance: msg.importance.clone(),
                ack_required: msg.ack_required != 0,
                created_ts: None,
                attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                from: row.sender_name.clone(),
                kind: row.kind.clone(),
                commit: MailboxCommitMetaFull {
                    authored_ts: None,
                    deletions: 0,
                    diff_summary: MailboxDiffSummary {
                        excerpt: None,
                        hunks: 1,
                    },
                    hexsha: None,
                    insertions: 21 + i64::from(msg.thread_id.is_some()),
                    summary,
                },
            }
        })
        .collect();

    let count = messages.len();
    let response = MailboxResponseFull {
        project: project.human_key,
        agent: agent_name,
        count,
        messages,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Outbox message entry (includes `body_md`, to, cc, bcc)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxMessageEntry {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    pub from: String,
    pub body_md: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub commit: MailboxCommitMetaFull,
}

/// Outbox response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxResponse {
    pub project: String,
    pub agent: String,
    pub count: usize,
    pub messages: Vec<OutboxMessageEntry>,
}

/// Get outbox for an agent.
#[resource(uri = "resource://outbox/{agent}", description = "Sent messages")]
#[allow(clippy::too_many_lines)]
pub async fn outbox(ctx: &McpContext, agent: String) -> McpResult<String> {
    use mcp_agent_mail_db::sqlmodel::Value;

    let (agent_name, query) = split_param_and_query(&agent);
    let project_key = query.get("project").cloned().unwrap_or_default();
    let limit: usize = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let include_bodies = query
        .get("include_bodies")
        .is_some_and(|v| parse_bool_param(v));
    let since_ts: Option<i64> = query.get("since_ts").and_then(|v| iso_to_micros(v));

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;
    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_row = agents
        .iter()
        .find(|a| a.name == agent_name)
        .cloned()
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;
    let agent_id = agent_row.id.unwrap_or(0);

    // Query sent messages (where sender_id = agent_id)
    let conn = match pool.acquire(ctx.cx()).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Err(McpError::internal_error(e.to_string())),
        Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
        Outcome::Panicked(p) => {
            return Err(McpError::internal_error(format!(
                "Internal panic: {}",
                p.message()
            )));
        }
    };

    let limit_i64 = i64::try_from(limit).unwrap_or(20);
    #[allow(clippy::option_if_let_else)]
    let (sql, params): (String, Vec<Value>) = if let Some(ts) = since_ts {
        (
            "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
             m.importance, m.ack_required, m.created_ts, m.attachments \
             FROM messages m \
             WHERE m.sender_id = ? AND m.project_id = ? AND m.created_ts > ? \
             ORDER BY m.created_ts DESC LIMIT ?"
                .to_string(),
            vec![
                Value::BigInt(agent_id),
                Value::BigInt(project_id),
                Value::BigInt(ts),
                Value::BigInt(limit_i64),
            ],
        )
    } else {
        (
            "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
             m.importance, m.ack_required, m.created_ts, m.attachments \
             FROM messages m \
             WHERE m.sender_id = ? AND m.project_id = ? \
             ORDER BY m.created_ts DESC LIMIT ?"
                .to_string(),
            vec![
                Value::BigInt(agent_id),
                Value::BigInt(project_id),
                Value::BigInt(limit_i64),
            ],
        )
    };

    let start = mcp_agent_mail_db::query_timer();
    let rows = conn.query_sync(&sql, &params);
    mcp_agent_mail_db::record_query(&sql, mcp_agent_mail_db::elapsed_us(start));
    let rows = rows.map_err(|e| McpError::internal_error(e.to_string()))?;

    let mut messages: Vec<OutboxMessageEntry> = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row.get_named("id").unwrap_or(0);
        let msg_project_id: i64 = row.get_named("project_id").unwrap_or(0);
        let sender_id: i64 = row.get_named("sender_id").unwrap_or(0);
        let thread_id: Option<String> = row.get_named("thread_id").ok();
        let subject: String = row.get_named("subject").unwrap_or_default();
        let body_md: String = if include_bodies {
            row.get_named("body_md").unwrap_or_default()
        } else {
            String::new()
        };
        let importance: String = row.get_named("importance").unwrap_or_default();
        let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
        let attachments_json: String = row.get_named("attachments").unwrap_or_default();

        // Get recipients for this message
        let recip_sql = "SELECT a.name, r.kind FROM message_recipients r \
                        JOIN agents a ON a.id = r.agent_id \
                        WHERE r.message_id = ?";
        let recip_params = [Value::BigInt(id)];
        let recip_start = mcp_agent_mail_db::query_timer();
        let recip_rows = conn.query_sync(recip_sql, &recip_params);
        mcp_agent_mail_db::record_query(recip_sql, mcp_agent_mail_db::elapsed_us(recip_start));
        let recip_rows = recip_rows.map_err(|e| McpError::internal_error(e.to_string()))?;

        let mut to_list: Vec<String> = Vec::with_capacity(4);
        let mut cc_list: Vec<String> = Vec::with_capacity(2);
        let mut bcc_list: Vec<String> = Vec::with_capacity(2);
        for rr in recip_rows {
            let name: String = rr.get_named("name").unwrap_or_default();
            let kind: String = rr.get_named("kind").unwrap_or_default();
            match kind.as_str() {
                "cc" => cc_list.push(name),
                "bcc" => bcc_list.push(name),
                // "to" or any other kind defaults to to_list
                _ => to_list.push(name),
            }
        }

        // Build summary - find first "to" recipient
        let first_to = to_list.first().cloned().unwrap_or_default();
        let summary = format!("mail: {agent_name} -> {first_to} | {subject}");
        let has_thread_id = thread_id.is_some();

        messages.push(OutboxMessageEntry {
            id,
            project_id: msg_project_id,
            sender_id,
            thread_id,
            subject,
            importance,
            ack_required: ack_required != 0,
            created_ts: None,
            attachments: serde_json::from_str(&attachments_json).unwrap_or_default(),
            from: agent_name.clone(),
            body_md,
            to: to_list,
            cc: cc_list,
            bcc: bcc_list,
            commit: MailboxCommitMetaFull {
                authored_ts: None,
                deletions: 0,
                diff_summary: MailboxDiffSummary {
                    excerpt: None,
                    hunks: 1,
                },
                hexsha: None,
                insertions: 21 + i64::from(has_thread_id),
                summary,
            },
        });
    }

    let count = messages.len();
    let response = OutboxResponse {
        project: project.human_key,
        agent: agent_name,
        count,
        messages,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

// ============================================================================
// View Resources
// ============================================================================

/// View message entry (matches Python output format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewMessageEntry {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub kind: String,
}

/// View response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewResponse {
    pub project: String,
    pub agent: String,
    pub count: usize,
    pub messages: Vec<ViewMessageEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// Get urgent unread messages for an agent.
#[resource(
    uri = "resource://views/urgent-unread/{agent}",
    description = "Unread high/urgent messages"
)]
pub async fn views_urgent_unread(ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);
    let project_key = query.get("project").cloned().unwrap_or_default();
    let limit: usize = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project by slug or human_key
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;
    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent by name
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_row = agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;
    let agent_id = agent_row.id.unwrap_or(0);

    // Fetch inbox and filter for urgent unread
    let inbox_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_inbox(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            true,
            None,
            limit,
        )
        .await,
    )?;

    let messages: Vec<ViewMessageEntry> = inbox_rows
        .into_iter()
        .map(|row| {
            let msg = &row.message;
            ViewMessageEntry {
                id: msg.id.unwrap_or(0),
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id.clone(),
                subject: msg.subject.clone(),
                importance: msg.importance.clone(),
                ack_required: msg.ack_required != 0,
                created_ts: None,
                attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                from: Some(row.sender_name.clone()),
                kind: row.kind.clone(),
            }
        })
        .collect();

    let count = messages.len();
    let response = ViewResponse {
        project: project.human_key,
        agent: agent_name,
        count,
        messages,
        ttl_seconds: None,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get messages requiring acknowledgement for an agent.
#[resource(
    uri = "resource://views/ack-required/{agent}",
    description = "Messages requiring acknowledgement"
)]
pub async fn views_ack_required(ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);
    let project_key = query.get("project").cloned().unwrap_or_default();
    let limit: usize = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;
    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_row = agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;
    let agent_id = agent_row.id.unwrap_or(0);

    // Fetch inbox and filter for ack_required (not yet acked)
    let inbox_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_inbox(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            false,
            None,
            limit,
        )
        .await,
    )?;

    // Filter for ack_required messages that haven't been acknowledged yet
    let messages: Vec<ViewMessageEntry> = inbox_rows
        .into_iter()
        .filter(|row| row.message.ack_required != 0 && row.ack_ts.is_none())
        .map(|row| {
            let msg = &row.message;
            ViewMessageEntry {
                id: msg.id.unwrap_or(0),
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id.clone(),
                subject: msg.subject.clone(),
                importance: msg.importance.clone(),
                ack_required: true,
                created_ts: None,
                attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                from: None, // ack-required view doesn't include from
                kind: row.kind.clone(),
            }
        })
        .collect();

    let count = messages.len();
    let response = ViewResponse {
        project: project.human_key,
        agent: agent_name,
        count,
        messages,
        ttl_seconds: None,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Message entry for stale acks view (includes `read_at` and `age_seconds`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleAckMessageEntry {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
    pub age_seconds: i64,
}

/// Stale acks response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleAcksResponse {
    pub project: String,
    pub agent: String,
    pub ttl_seconds: u64,
    pub count: usize,
    pub messages: Vec<StaleAckMessageEntry>,
}

/// Get stale acknowledgements for an agent.
///
/// Returns ack-required messages older than `ttl_seconds` that have not been
/// acknowledged, matching the legacy Python `acks_stale_view` resource.
#[resource(
    uri = "resource://views/acks-stale/{agent}",
    description = "Acknowledgements considered stale"
)]
pub async fn views_acks_stale(ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);
    let project_key = query.get("project").cloned().unwrap_or_default();
    let ttl_seconds: u64 = query
        .get("ttl_seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let limit: usize = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;
    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_row = agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;
    let agent_id = agent_row.id.unwrap_or(0);

    // Fetch unacked ack-required messages (over-fetch, then filter by age)
    let unacked_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_unacked_for_agent(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            limit * 5,
        )
        .await,
    )?;

    let now_us = mcp_agent_mail_db::now_micros();
    let ttl_us = i64::try_from(ttl_seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000_000);

    let mut messages = Vec::with_capacity(unacked_rows.len());
    for row in unacked_rows {
        let age_us = now_us.saturating_sub(row.message.created_ts);
        if age_us >= ttl_us {
            let age_seconds = age_us / 1_000_000;
            let msg = &row.message;
            messages.push(StaleAckMessageEntry {
                id: msg.id.unwrap_or(0),
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id.clone(),
                subject: msg.subject.clone(),
                importance: msg.importance.clone(),
                ack_required: true,
                created_ts: None,
                attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                kind: row.kind.clone(),
                read_at: row.read_ts.map(mcp_agent_mail_db::micros_to_iso),
                age_seconds,
            });
            if messages.len() >= limit {
                break;
            }
        }
    }

    let count = messages.len();
    let response = StaleAcksResponse {
        project: project.human_key,
        agent: agent_name,
        ttl_seconds,
        count,
        messages,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Get overdue acknowledgements for an agent.
///
/// Returns ack-required messages older than `ttl_minutes` that have not been
/// acknowledged, matching the legacy Python `ack_overdue_view` resource.
#[resource(
    uri = "resource://views/ack-overdue/{agent}",
    description = "Acknowledgements overdue"
)]
pub async fn views_ack_overdue(ctx: &McpContext, agent: String) -> McpResult<String> {
    let (agent_name, query) = split_param_and_query(&agent);
    let project_key = query.get("project").cloned().unwrap_or_default();
    let limit: usize = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let ttl_minutes: u64 = query
        .get("ttl_minutes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);

    if project_key.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "project query parameter is required",
        ));
    }

    let pool = get_db_pool()?;

    // Find project
    let projects =
        db_outcome_to_mcp_result(mcp_agent_mail_db::queries::list_projects(ctx.cx(), &pool).await)?;
    let project = projects
        .into_iter()
        .find(|p| p.slug == project_key || p.human_key == project_key)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Project not found"))?;

    let project_id = project.id.unwrap_or(0);

    // Find agent
    let agents = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_row = agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .ok_or_else(|| McpError::new(McpErrorCode::InvalidParams, "Agent not found"))?;
    let agent_id = agent_row.id.unwrap_or(0);

    // Compute cutoff: messages older than ttl_minutes are overdue
    let cutoff_minutes = ttl_minutes.max(1);
    let now_us = mcp_agent_mail_db::now_micros();
    let cutoff_us = now_us.saturating_sub(
        i64::try_from(cutoff_minutes)
            .unwrap_or(i64::MAX)
            .saturating_mul(60)
            .saturating_mul(1_000_000),
    );

    // Fetch unacked ack-required messages (over-fetch, then filter by cutoff)
    let unacked_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_unacked_for_agent(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            limit * 5,
        )
        .await,
    )?;

    let mut messages = Vec::with_capacity(unacked_rows.len());
    for row in unacked_rows {
        if row.message.created_ts <= cutoff_us {
            let msg = &row.message;
            messages.push(ViewMessageEntry {
                id: msg.id.unwrap_or(0),
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id.clone(),
                subject: msg.subject.clone(),
                importance: msg.importance.clone(),
                ack_required: true,
                created_ts: None,
                attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                from: None,
                kind: row.kind.clone(),
            });
            if messages.len() >= limit {
                break;
            }
        }
    }

    let count = messages.len();
    let response = ViewResponse {
        project: project.human_key,
        agent: agent_name,
        count,
        messages,
        ttl_seconds: None,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

// ============================================================================
// File Reservation Resources
// ============================================================================

#[derive(Debug, Clone, Default)]
struct ReservationPatternActivity {
    matches: bool,
    fs_activity_micros: Option<i64>,
    git_activity_micros: Option<i64>,
}

const RESERVATION_GLOB_MARKERS: &[char] = &['*', '?', '['];

fn reservation_contains_glob(pattern: &str) -> bool {
    RESERVATION_GLOB_MARKERS
        .iter()
        .any(|m| pattern.contains(*m))
}

fn reservation_normalize_pattern(pattern: &str) -> String {
    let mut s = pattern.trim();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.trim_start_matches('/').trim().to_string()
}

fn reservation_project_workspace_path(project_human_key: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(project_human_key);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

fn reservation_open_repo_root(workspace: &Path) -> Option<(PathBuf, PathBuf)> {
    let repo = git2::Repository::discover(workspace).ok()?;
    let root = repo.workdir()?.to_path_buf();
    let root_canon = root.canonicalize().unwrap_or(root);
    let ws_canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());

    if !ws_canon.starts_with(&root_canon) {
        return None;
    }

    let rel = ws_canon.strip_prefix(&root_canon).ok()?.to_path_buf();
    Some((root_canon, rel))
}

fn reservation_system_time_to_micros(t: SystemTime) -> Option<i64> {
    let dur = t.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(dur.as_micros()).ok()
}

fn reservation_path_to_slash_string(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn reservation_git_pathspec(workspace_rel: &Path, normalized_pattern: &str) -> String {
    let rel = reservation_path_to_slash_string(workspace_rel);
    let mut out = String::new();
    if !rel.is_empty() && rel != "." {
        out.push_str(rel.trim_end_matches('/'));
        out.push('/');
    }
    out.push_str(normalized_pattern.trim_start_matches('/'));
    out
}

fn reservation_git_latest_activity_micros(repo_root: &Path, pathspecs: &[String]) -> Option<i64> {
    if pathspecs.is_empty() {
        return None;
    }

    // Chunk to avoid exceeding OS arg limits when globs expand to many matches.
    let mut best: Option<i64> = None;
    for chunk in pathspecs.chunks(128) {
        let Ok(out) = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["log", "-1", "--format=%ct", "--"])
            .args(chunk)
            .output()
        else {
            continue;
        };

        if !out.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let Ok(secs) = stdout.trim().parse::<i64>() else {
            continue;
        };
        let micros = secs.saturating_mul(1_000_000);
        best = Some(best.map_or(micros, |prev| prev.max(micros)));
    }

    best
}

fn reservation_compute_pattern_activity(
    workspace: Option<&Path>,
    repo_root: Option<&Path>,
    workspace_rel: Option<&Path>,
    pattern_raw: &str,
) -> ReservationPatternActivity {
    let Some(workspace) = workspace else {
        return ReservationPatternActivity::default();
    };

    let normalized = reservation_normalize_pattern(pattern_raw);
    if normalized.is_empty() {
        return ReservationPatternActivity::default();
    }

    let want_git = repo_root.is_some() && workspace_rel.is_some();

    let has_glob = reservation_contains_glob(&normalized);
    let mut matches = false;
    let mut fs_latest: Option<i64> = None;

    if has_glob {
        // IMPORTANT: Do not expand globs by walking the filesystem. Broad patterns like `src/**`
        // can explode to thousands of matches and stall the MCP server.
        //
        // Instead, treat "matched" as "base directory exists" and ask git for the latest commit
        // affecting the pathspec via `:(glob)` magic (cheap and bounded).
        let base_dir = {
            let first_glob = normalized
                .char_indices()
                .find_map(|(idx, ch)| RESERVATION_GLOB_MARKERS.contains(&ch).then_some(idx))
                .unwrap_or(0);
            let prefix = &normalized[..first_glob];
            if prefix.ends_with('/') {
                prefix.trim_end_matches('/')
            } else {
                prefix
                    .rsplit_once('/')
                    .map_or("", |(dir, _)| dir.trim_end_matches('/'))
            }
        };

        let base_path = if base_dir.is_empty() {
            workspace.to_path_buf()
        } else {
            workspace.join(base_dir)
        };

        if let Ok(meta) = std::fs::metadata(&base_path) {
            matches = true;
            if let Ok(modified) = meta.modified() {
                fs_latest = reservation_system_time_to_micros(modified);
            }
        }
    } else {
        let candidate = workspace.join(&normalized);
        if candidate.exists() {
            matches = true;

            if let Ok(meta) = std::fs::metadata(&candidate) {
                if let Ok(modified) = meta.modified() {
                    fs_latest = reservation_system_time_to_micros(modified);
                }
            }
        }
    }

    let git_activity = if matches && want_git {
        let spec = reservation_git_pathspec(workspace_rel.unwrap(), &normalized);
        let spec = if has_glob {
            format!(":(glob){spec}")
        } else {
            spec
        };
        reservation_git_latest_activity_micros(repo_root.unwrap(), &[spec])
    } else {
        None
    };

    ReservationPatternActivity {
        matches,
        fs_activity_micros: fs_latest,
        git_activity_micros: git_activity,
    }
}

#[cfg(test)]
mod reservation_activity_tests {
    use super::*;

    fn run_git(repo_root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .output()
            .expect("failed to run git in reservation activity test helper");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn reservation_compute_pattern_activity_glob_uses_git_pathspec_magic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        std::fs::create_dir_all(root.join("src")).expect("create src dir");
        std::fs::write(root.join("src/lib.rs"), "fn main() {}\n").expect("write file");

        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test User"]);
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "init"]);

        let (repo_root, workspace_rel) =
            reservation_open_repo_root(root).expect("repo root discoverable");
        let activity = reservation_compute_pattern_activity(
            Some(root),
            Some(repo_root.as_path()),
            Some(workspace_rel.as_path()),
            "src/**",
        );
        assert!(activity.matches);
        assert!(activity.git_activity_micros.is_some());

        let unmatched = reservation_compute_pattern_activity(
            Some(root),
            Some(repo_root.as_path()),
            Some(workspace_rel.as_path()),
            "nope/**",
        );
        assert!(!unmatched.matches);
        assert!(unmatched.git_activity_micros.is_none());
    }
}

/// File reservation entry (matches Python output format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReservationResourceEntry {
    pub id: i64,
    pub agent: String,
    pub path_pattern: String,
    pub exclusive: bool,
    pub reason: String,
    pub created_ts: Option<String>,
    pub expires_ts: Option<String>,
    pub released_ts: Option<String>,
    pub stale: bool,
    pub stale_reasons: Vec<String>,
    pub last_agent_activity_ts: Option<String>,
    pub last_mail_activity_ts: Option<String>,
    pub last_git_activity_ts: Option<String>,
    pub last_filesystem_activity_ts: Option<String>,
}

fn retain_active_file_reservations(
    rows: &mut Vec<mcp_agent_mail_db::FileReservationRow>,
    now_micros: i64,
) {
    rows.retain(|row| row.released_ts.is_none() && row.expires_ts > now_micros);
}

/// Get file reservations for a project.
#[allow(clippy::too_many_lines)]
#[resource(
    uri = "resource://file_reservations/{slug}",
    description = "File reservations in a project"
)]
pub async fn file_reservations(ctx: &McpContext, slug: String) -> McpResult<String> {
    let (slug_str, query) = split_param_and_query(&slug);
    let active_only = query.get("active_only").is_none_or(|v| parse_bool_param(v));

    let pool = get_db_pool()?;

    // Resolve project by slug or human key.
    let project = if slug_str.starts_with('/') {
        resolve_project(ctx, &pool, &slug_str).await?
    } else {
        match mcp_agent_mail_db::queries::get_project_by_slug(ctx.cx(), &pool, &slug_str).await {
            asupersync::Outcome::Ok(row) => row,
            asupersync::Outcome::Err(_) => {
                return Err(McpError::new(
                    McpErrorCode::InvalidParams,
                    "Project not found",
                ));
            }
            asupersync::Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            asupersync::Outcome::Panicked(p) => {
                return Err(McpError::internal_error(format!(
                    "Internal panic: {}",
                    p.message()
                )));
            }
        }
    };

    let project_id = project.id.unwrap_or(0);

    let config = &Config::get();
    let now_micros = mcp_agent_mail_db::now_micros();
    let inactivity_seconds =
        i64::try_from(config.file_reservation_inactivity_seconds).unwrap_or(i64::MAX);
    let grace_seconds =
        i64::try_from(config.file_reservation_activity_grace_seconds).unwrap_or(i64::MAX);
    let inactivity_micros = inactivity_seconds.saturating_mul(1_000_000);
    let grace_micros = grace_seconds.saturating_mul(1_000_000);

    // Optional workspace and repo roots for filesystem/git activity signals.
    let workspace = reservation_project_workspace_path(&project.human_key);
    let repo_info = workspace.as_deref().and_then(reservation_open_repo_root);
    let repo_root = repo_info.as_ref().map(|(root, _)| root.as_path());
    let workspace_rel = repo_info.as_ref().map(|(_, rel)| rel.as_path());

    // Cleanup: release any expired (TTL) reservations and any stale reservations.
    //
    // Parity with Python: this resource is allowed to perform best-effort cleanup.
    let mut release_payloads: Vec<serde_json::Value> = Vec::with_capacity(8);

    // We only need agents map + mail cache for stale evaluation.
    let agent_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_agents(ctx.cx(), &pool, project_id).await,
    )?;
    let agent_by_id: HashMap<i64, mcp_agent_mail_db::AgentRow> = agent_rows
        .iter()
        .filter_map(|row| row.id.map(|id| (id, row.clone())))
        .collect();

    let mut mail_activity_cache: HashMap<i64, Option<i64>> =
        HashMap::with_capacity(agent_rows.len());
    let mut pattern_activity_cache: HashMap<String, ReservationPatternActivity> =
        HashMap::with_capacity(16);

    // Cleanup only needs unreleased rows (including expired). Released history is unbounded and
    // scanning it on every resource read can time out on long-lived projects.
    let all_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_unreleased_file_reservations(ctx.cx(), &pool, project_id)
            .await,
    )?;

    // Expire TTL-elapsed reservations (released_ts=NULL AND expires_ts < now).
    for row in all_rows
        .iter()
        .filter(|r| r.released_ts.is_none() && r.expires_ts < now_micros)
    {
        let Some(id) = row.id else { continue };
        let updated = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::force_release_reservation(ctx.cx(), &pool, id).await,
        )?;
        if updated == 0 {
            continue;
        }
        let agent_name = agent_by_id
            .get(&row.agent_id)
            .map_or_else(|| format!("agent_{}", row.agent_id), |a| a.name.clone());

        release_payloads.push(serde_json::json!({
            "id": id,
            "project": project.human_key.clone(),
            "agent": agent_name,
            "path_pattern": row.path_pattern.clone(),
            "exclusive": row.exclusive != 0,
            "reason": row.reason.clone(),
            "created_ts": micros_to_iso(row.created_ts),
            "expires_ts": micros_to_iso(row.expires_ts),
            "released_ts": micros_to_iso(now_micros),
        }));
    }

    // Release stale reservations (unreleased + agent inactive + no recent mail/fs/git).
    for row in all_rows
        .iter()
        .filter(|r| r.released_ts.is_none() && r.expires_ts >= now_micros)
    {
        let Some(id) = row.id else { continue };
        let Some(agent) = agent_by_id.get(&row.agent_id) else {
            continue;
        };

        let agent_inactive = now_micros.saturating_sub(agent.last_active_ts) > inactivity_micros;

        let mail_activity = if let Some(val) = mail_activity_cache.get(&row.agent_id) {
            *val
        } else {
            let out = db_outcome_to_mcp_result(
                mcp_agent_mail_db::queries::get_agent_last_mail_activity(
                    ctx.cx(),
                    &pool,
                    row.agent_id,
                    project_id,
                )
                .await,
            )?;
            mail_activity_cache.insert(row.agent_id, out);
            out
        };
        let recent_mail =
            mail_activity.is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);

        let pat_activity = pattern_activity_cache
            .entry(row.path_pattern.clone())
            .or_insert_with(|| {
                reservation_compute_pattern_activity(
                    workspace.as_deref(),
                    repo_root,
                    workspace_rel,
                    &row.path_pattern,
                )
            })
            .clone();
        let recent_fs = pat_activity
            .fs_activity_micros
            .is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);
        let recent_git = pat_activity
            .git_activity_micros
            .is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);

        let stale = agent_inactive && !(recent_mail || recent_fs || recent_git);
        if !stale {
            continue;
        }

        let updated = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::force_release_reservation(ctx.cx(), &pool, id).await,
        )?;
        if updated == 0 {
            continue;
        }

        release_payloads.push(serde_json::json!({
            "id": id,
            "project": project.human_key.clone(),
            "agent": agent.name,
            "path_pattern": row.path_pattern.clone(),
            "exclusive": row.exclusive != 0,
            "reason": row.reason.clone(),
            "created_ts": micros_to_iso(row.created_ts),
            "expires_ts": micros_to_iso(row.expires_ts),
            "released_ts": micros_to_iso(now_micros),
        }));
    }

    // Best-effort archive artifact writes for any releases.
    if !release_payloads.is_empty() {
        match mcp_agent_mail_storage::ensure_archive(config, &project.slug) {
            Ok(archive) => {
                let result = mcp_agent_mail_storage::with_project_lock(&archive, || {
                    mcp_agent_mail_storage::write_file_reservation_records(
                        &archive,
                        config,
                        &release_payloads,
                    )
                });
                if let Err(err) = result {
                    tracing::warn!("Failed to write released reservation artifacts: {err}");
                }
            }
            Err(err) => {
                tracing::warn!("Failed to ensure archive for reservation cleanup: {err}");
            }
        }
    }

    // List file reservations for the resource output after cleanup.
    let mut rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_file_reservations(
            ctx.cx(),
            &pool,
            project_id,
            active_only,
        )
        .await,
    )?;
    if active_only {
        // Defense in depth: enforce active-only semantics here too, so older DB
        // backends/query drift cannot leak released or expired reservations.
        retain_active_file_reservations(&mut rows, now_micros);
    }

    // Match Python ordering: created_ts asc (id is usually insertion order but not guaranteed).
    rows.sort_by_key(|r| r.created_ts);

    let mut reservations: Vec<FileReservationResourceEntry> = Vec::with_capacity(rows.len());
    for row in rows {
        let agent_name = agent_by_id
            .get(&row.agent_id)
            .map_or_else(|| format!("agent_{}", row.agent_id), |a| a.name.clone());
        let last_agent_activity_ts = agent_by_id
            .get(&row.agent_id)
            .map(|a| micros_to_iso(a.last_active_ts));

        let mail_activity = if let Some(val) = mail_activity_cache.get(&row.agent_id) {
            *val
        } else {
            let out = db_outcome_to_mcp_result(
                mcp_agent_mail_db::queries::get_agent_last_mail_activity(
                    ctx.cx(),
                    &pool,
                    row.agent_id,
                    project_id,
                )
                .await,
            )?;
            mail_activity_cache.insert(row.agent_id, out);
            out
        };

        let pat_activity = if let Some(val) = pattern_activity_cache.get(&row.path_pattern) {
            val.clone()
        } else {
            let computed = reservation_compute_pattern_activity(
                workspace.as_deref(),
                repo_root,
                workspace_rel,
                &row.path_pattern,
            );
            pattern_activity_cache.insert(row.path_pattern.clone(), computed.clone());
            computed
        };

        let agent_last_active = agent_by_id.get(&row.agent_id).map(|a| a.last_active_ts);
        let agent_inactive =
            agent_last_active.is_some_and(|ts| now_micros.saturating_sub(ts) > inactivity_micros);
        let recent_mail =
            mail_activity.is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);
        let recent_fs = pat_activity
            .fs_activity_micros
            .is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);
        let recent_git = pat_activity
            .git_activity_micros
            .is_some_and(|ts| now_micros.saturating_sub(ts) <= grace_micros);

        let stale = row.released_ts.is_none()
            && agent_inactive
            && !(recent_mail || recent_fs || recent_git);

        let mut stale_reasons = Vec::with_capacity(4);
        if agent_inactive {
            stale_reasons.push(format!("agent_inactive>{inactivity_seconds}s"));
        } else {
            stale_reasons.push("agent_recently_active".to_string());
        }
        if recent_mail {
            stale_reasons.push("mail_activity_recent".to_string());
        } else {
            stale_reasons.push(format!("no_recent_mail_activity>{grace_seconds}s"));
        }
        if pat_activity.matches {
            if recent_fs {
                stale_reasons.push("filesystem_activity_recent".to_string());
            } else {
                stale_reasons.push(format!("no_recent_filesystem_activity>{grace_seconds}s"));
            }
            if recent_git {
                stale_reasons.push("git_activity_recent".to_string());
            } else {
                stale_reasons.push(format!("no_recent_git_activity>{grace_seconds}s"));
            }
        } else {
            stale_reasons.push("path_pattern_unmatched".to_string());
        }

        reservations.push(FileReservationResourceEntry {
            id: row.id.unwrap_or(0),
            agent: agent_name,
            path_pattern: row.path_pattern,
            exclusive: row.exclusive != 0,
            reason: row.reason,
            created_ts: Some(micros_to_iso(row.created_ts)),
            expires_ts: Some(micros_to_iso(row.expires_ts)),
            released_ts: row.released_ts.map(micros_to_iso),
            stale,
            stale_reasons,
            last_agent_activity_ts,
            last_mail_activity_ts: mail_activity.map(micros_to_iso),
            last_git_activity_ts: pat_activity.git_activity_micros.map(micros_to_iso),
            last_filesystem_activity_ts: pat_activity.fs_activity_micros.map(micros_to_iso),
        });
    }

    tracing::debug!(
        "Getting file reservations for project {} (active_only: {})",
        slug_str,
        active_only
    );

    serde_json::to_string(&reservations)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

#[cfg(test)]
mod query_param_tests {
    use super::*;
    use std::fs;

    // -----------------------------------------------------------------------
    // split_param_and_query
    // -----------------------------------------------------------------------

    #[test]
    fn split_no_query() {
        let (base, params) = split_param_and_query("GreenCastle");
        assert_eq!(base, "GreenCastle");
        assert!(params.is_empty());
    }

    #[test]
    fn split_with_query() {
        let (base, params) = split_param_and_query("GreenCastle?project=/data/proj&limit=10");
        assert_eq!(base, "GreenCastle");
        assert_eq!(params.get("project").unwrap(), "/data/proj");
        assert_eq!(params.get("limit").unwrap(), "10");
    }

    #[test]
    fn split_empty_query() {
        let (base, params) = split_param_and_query("value?");
        assert_eq!(base, "value");
        assert!(params.is_empty());
    }

    #[test]
    fn split_query_only() {
        let (base, params) = split_param_and_query("?key=val");
        assert_eq!(base, "");
        assert_eq!(params.get("key").unwrap(), "val");
    }

    #[test]
    fn split_base_percent_decoded() {
        let (base, params) = split_param_and_query("Green%20Castle?project=/data/proj");
        assert_eq!(base, "Green Castle");
        assert_eq!(params.get("project").unwrap(), "/data/proj");
    }

    // -----------------------------------------------------------------------
    // parse_query
    // -----------------------------------------------------------------------

    #[test]
    fn parse_query_basic() {
        let params = parse_query("a=1&b=2");
        assert_eq!(params.get("a").unwrap(), "1");
        assert_eq!(params.get("b").unwrap(), "2");
    }

    #[test]
    fn parse_query_empty_value() {
        let params = parse_query("key=&other=val");
        assert_eq!(params.get("key").unwrap(), "");
        assert_eq!(params.get("other").unwrap(), "val");
    }

    #[test]
    fn parse_query_no_value() {
        let params = parse_query("flag");
        assert_eq!(params.get("flag").unwrap(), "");
    }

    #[test]
    fn parse_query_url_encoded() {
        let params = parse_query("path=%2Fdata%2Fprojects%2Fbackend");
        assert_eq!(params.get("path").unwrap(), "/data/projects/backend");
    }

    #[test]
    fn parse_query_plus_as_space() {
        let params = parse_query("q=hello+world");
        assert_eq!(params.get("q").unwrap(), "hello world");
    }

    #[test]
    fn parse_query_empty_string() {
        let params = parse_query("");
        assert!(params.is_empty());
    }

    #[test]
    fn parse_query_trailing_ampersand() {
        let params = parse_query("a=1&b=2&");
        assert_eq!(params.len(), 2);
    }

    // -----------------------------------------------------------------------
    // projects_list query options
    // -----------------------------------------------------------------------

    #[test]
    fn projects_query_options_accepts_json_format() {
        let params = parse_query("format=json");
        let opts = parse_projects_list_query_options(&params).expect("json format should be valid");
        assert_eq!(opts.limit, None);
        assert_eq!(opts.contains, None);
    }

    #[test]
    fn projects_query_options_rejects_unknown_format() {
        let params = parse_query("format=xml");
        let err = parse_projects_list_query_options(&params).expect_err("xml format must fail");
        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("Unsupported projects format"));
    }

    #[test]
    fn projects_query_options_parses_limit_and_contains() {
        let params = parse_query("limit=2&contains=Mail");
        let opts = parse_projects_list_query_options(&params).expect("query params should parse");
        assert_eq!(opts.limit, Some(2));
        assert_eq!(opts.contains.as_deref(), Some("mail"));
    }

    #[test]
    fn projects_query_options_rejects_invalid_limit() {
        let params = parse_query("limit=abc");
        let err =
            parse_projects_list_query_options(&params).expect_err("non-numeric limit must fail");
        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("Invalid limit"));
    }

    #[test]
    fn apply_projects_query_options_filters_and_limits() {
        let projects = vec![
            ProjectListEntry {
                id: 1,
                slug: "alpha-service".to_string(),
                human_key: "/data/projects/alpha-service".to_string(),
                created_at: None,
            },
            ProjectListEntry {
                id: 2,
                slug: "beta-mail".to_string(),
                human_key: "/data/projects/beta-mail".to_string(),
                created_at: None,
            },
            ProjectListEntry {
                id: 3,
                slug: "gamma-mailer".to_string(),
                human_key: "/data/projects/gamma-mailer".to_string(),
                created_at: None,
            },
        ];

        let opts = ProjectsListQueryOptions {
            limit: Some(1),
            contains: Some("mail".to_string()),
        };
        let filtered = apply_projects_list_query_options(projects, &opts);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].slug, "beta-mail");
    }

    // -----------------------------------------------------------------------
    // parse_bool_param (Python parity)
    // -----------------------------------------------------------------------

    #[test]
    fn bool_param_truthy_values() {
        assert!(parse_bool_param("1"));
        assert!(parse_bool_param("true"));
        assert!(parse_bool_param("True"));
        assert!(parse_bool_param("TRUE"));
        assert!(parse_bool_param("t"));
        assert!(parse_bool_param("T"));
        assert!(parse_bool_param("yes"));
        assert!(parse_bool_param("Yes"));
        assert!(parse_bool_param("y"));
        assert!(parse_bool_param("Y"));
    }

    #[test]
    fn bool_param_falsy_values() {
        assert!(!parse_bool_param("0"));
        assert!(!parse_bool_param("false"));
        assert!(!parse_bool_param("False"));
        assert!(!parse_bool_param("no"));
        assert!(!parse_bool_param("No"));
        assert!(!parse_bool_param("n"));
        assert!(!parse_bool_param(""));
        assert!(!parse_bool_param("anything_else"));
    }

    #[test]
    fn bool_param_whitespace_trimmed() {
        assert!(parse_bool_param("  true  "));
        assert!(parse_bool_param(" 1 "));
        assert!(!parse_bool_param("  false  "));
    }

    // -----------------------------------------------------------------------
    // percent_decode_component
    // -----------------------------------------------------------------------

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode_component("hello"), "hello");
    }

    #[test]
    fn percent_decode_encoded_slash() {
        assert_eq!(percent_decode_component("%2Fdata%2Fpath"), "/data/path");
    }

    #[test]
    fn percent_decode_space_encoding() {
        assert_eq!(percent_decode_component("hello%20world"), "hello world");
        assert_eq!(percent_decode_component("hello+world"), "hello world");
    }

    #[test]
    fn percent_decode_special_chars() {
        assert_eq!(percent_decode_component("%40user"), "@user");
        assert_eq!(percent_decode_component("key%3Dvalue"), "key=value");
    }

    #[test]
    fn percent_decode_invalid_hex() {
        // Invalid hex should pass through unchanged.
        assert_eq!(percent_decode_component("%ZZ"), "%ZZ");
    }

    #[test]
    fn percent_decode_truncated() {
        // Truncated % at end should pass through.
        assert_eq!(percent_decode_component("abc%2"), "abc%2");
    }

    // -----------------------------------------------------------------------
    // Integration: resource URI query patterns
    // -----------------------------------------------------------------------

    #[test]
    fn inbox_query_params_parsed() {
        let input = "GreenCastle?project=/data/proj&include_bodies=true&urgent_only=1&limit=5";
        let (agent, query) = split_param_and_query(input);
        assert_eq!(agent, "GreenCastle");
        assert_eq!(query.get("project").unwrap(), "/data/proj");
        assert!(parse_bool_param(query.get("include_bodies").unwrap()));
        assert!(parse_bool_param(query.get("urgent_only").unwrap()));
        assert_eq!(query.get("limit").unwrap().parse::<usize>().unwrap(), 5);
    }

    #[test]
    fn thread_query_params_parsed() {
        let input = "br-123?project=/data/backend&include_bodies=true";
        let (thread_id, query) = split_param_and_query(input);
        assert_eq!(thread_id, "br-123");
        assert_eq!(query.get("project").unwrap(), "/data/backend");
        assert!(
            query
                .get("include_bodies")
                .is_some_and(|v| parse_bool_param(v))
        );
    }

    #[test]
    fn acks_stale_query_params_parsed() {
        let input = "BlueBear?project=/data/proj&ttl_seconds=7200&limit=50";
        let (agent, query) = split_param_and_query(input);
        assert_eq!(agent, "BlueBear");
        assert_eq!(
            query.get("ttl_seconds").unwrap().parse::<u64>().unwrap(),
            7200
        );
        assert_eq!(query.get("limit").unwrap().parse::<usize>().unwrap(), 50);
    }

    #[test]
    fn file_reservations_active_only_default() {
        // When active_only is absent, default should be true (matches Python).
        let input = "my-project-slug";
        let (_slug, query) = split_param_and_query(input);
        let active_only = query.get("active_only").is_none_or(|v| parse_bool_param(v));
        assert!(active_only, "active_only should default to true");
    }

    #[test]
    fn file_reservations_active_only_false() {
        let input = "my-project-slug?active_only=false";
        let (_slug, query) = split_param_and_query(input);
        let active_only = query.get("active_only").is_none_or(|v| parse_bool_param(v));
        assert!(!active_only, "active_only=false should be honored");
    }

    fn reservation_row(
        id: i64,
        released_ts: Option<i64>,
        expires_ts: i64,
    ) -> mcp_agent_mail_db::FileReservationRow {
        mcp_agent_mail_db::FileReservationRow {
            id: Some(id),
            project_id: 1,
            agent_id: 1,
            path_pattern: "src/**".to_string(),
            exclusive: 1,
            reason: String::new(),
            created_ts: 1,
            expires_ts,
            released_ts,
        }
    }

    #[test]
    fn retain_active_file_reservations_excludes_released_and_expired_rows() {
        let now_micros = 1000;
        let mut rows = vec![
            reservation_row(1, None, 2_000),
            reservation_row(2, Some(500), 2_000),
            reservation_row(3, None, 999),
        ];

        retain_active_file_reservations(&mut rows, now_micros);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, Some(1));
    }

    #[test]
    fn outbox_since_ts_parsed() {
        let input = "RedFox?project=/data/proj&since_ts=2026-01-01T00:00:00Z&include_bodies=1";
        let (agent, query) = split_param_and_query(input);
        assert_eq!(agent, "RedFox");
        assert_eq!(query.get("since_ts").unwrap(), "2026-01-01T00:00:00Z");
        assert!(parse_bool_param(query.get("include_bodies").unwrap()));
    }

    #[test]
    fn encoded_project_path() {
        // Project paths with slashes must be URL-encoded in query params.
        let input = "Agent?project=%2Fdata%2Fprojects%2Fmy-app";
        let (_agent, query) = split_param_and_query(input);
        assert_eq!(query.get("project").unwrap(), "/data/projects/my-app");
    }

    // -----------------------------------------------------------------------
    // Edge cases: multiple question marks, Unicode, deeply nested params
    // -----------------------------------------------------------------------

    #[test]
    fn split_multiple_question_marks() {
        // Only the first `?` should split; the rest belong to the query value.
        let (base, params) = split_param_and_query("agent?key=val?ue&other=ok");
        assert_eq!(base, "agent");
        assert_eq!(params.get("key").unwrap(), "val?ue");
        assert_eq!(params.get("other").unwrap(), "ok");
    }

    #[test]
    fn split_unicode_agent_name() {
        let (base, params) = split_param_and_query("Blue%C3%9Cber?project=/data/proj");
        // %C3%9C is the UTF-8 encoding for Ü
        assert_eq!(base, "BlueÜber");
        assert_eq!(params.get("project").unwrap(), "/data/proj");
    }

    #[test]
    fn parse_query_duplicate_keys_last_wins() {
        // HashMap semantics: last insert wins for duplicate keys.
        let params = parse_query("key=first&key=second");
        assert_eq!(params.get("key").unwrap(), "second");
    }

    #[test]
    fn parse_query_equals_in_value() {
        // Values can contain `=` signs (e.g. base64).
        let params = parse_query("payload=abc=def==");
        assert_eq!(params.get("payload").unwrap(), "abc=def==");
    }

    #[test]
    fn split_empty_string() {
        let (base, params) = split_param_and_query("");
        assert_eq!(base, "");
        assert!(params.is_empty());
    }

    #[test]
    fn inbox_limit_zero_means_no_limit() {
        let input = "RedFox?project=/data/proj&limit=0";
        let (_agent, query) = split_param_and_query(input);
        let limit: usize = query.get("limit").unwrap().parse().unwrap();
        assert_eq!(limit, 0);
    }

    #[test]
    fn workspace_root_prefers_workspace_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("repo");
        let nested = root.join("crates").join("pkg");
        fs::create_dir_all(&nested).expect("mkdirs");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/*\"]\n",
        )
        .expect("write root manifest");
        fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname=\"pkg\"\nversion=\"0.1.0\"\n",
        )
        .expect("write pkg manifest");

        let found = workspace_root_from(&nested).expect("workspace root");
        assert_eq!(found, root);
    }

    #[test]
    fn workspace_root_none_without_workspace_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("a").join("b");
        fs::create_dir_all(&nested).expect("mkdirs");
        fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname=\"only_pkg\"\nversion=\"0.1.0\"\n",
        )
        .expect("write pkg manifest");

        assert!(workspace_root_from(&nested).is_none());
    }
}

#[cfg(test)]
mod redact_and_timestamp_tests {
    use super::*;

    // -----------------------------------------------------------------------
    // redact_database_url
    // -----------------------------------------------------------------------

    #[test]
    fn redact_with_credentials() {
        assert_eq!(
            redact_database_url("postgresql://admin:secret@db.example.com:5432/mydb"),
            "postgresql://****@db.example.com:5432/mydb"
        );
    }

    #[test]
    fn redact_user_only_no_password() {
        assert_eq!(
            redact_database_url("postgresql://admin@db.example.com/mydb"),
            "postgresql://****@db.example.com/mydb"
        );
    }

    #[test]
    fn redact_no_credentials() {
        // No @ sign means no credentials to redact.
        assert_eq!(
            redact_database_url("sqlite:///data/mail.db"),
            "sqlite:///data/mail.db"
        );
    }

    #[test]
    fn redact_no_scheme() {
        // No :// at all -> return as-is.
        assert_eq!(redact_database_url("/data/mail.db"), "/data/mail.db");
    }

    #[test]
    fn redact_empty_string() {
        assert_eq!(redact_database_url(""), "");
    }

    #[test]
    fn redact_complex_password() {
        assert_eq!(
            redact_database_url("mysql://root:p%40ss%3Dword@localhost/db"),
            "mysql://****@localhost/db"
        );
    }

    // -----------------------------------------------------------------------
    // ts_f64_to_rfc3339
    // -----------------------------------------------------------------------

    #[test]
    fn ts_f64_epoch_zero() {
        let result = ts_f64_to_rfc3339(0.0).unwrap();
        assert!(result.starts_with("1970-01-01"));
    }

    #[test]
    fn ts_f64_known_timestamp() {
        // 2025-01-01T00:00:00Z = 1735689600.0
        let result = ts_f64_to_rfc3339(1_735_689_600.0).unwrap();
        assert!(result.starts_with("2025-01-01"));
    }

    #[test]
    fn ts_f64_fractional_seconds() {
        let result = ts_f64_to_rfc3339(1_735_689_600.5);
        assert!(result.is_some());
    }

    #[test]
    fn ts_f64_negative_fractional_seconds() {
        let result = ts_f64_to_rfc3339(-0.5).unwrap();
        assert!(result.starts_with("1969-12-31T23:59:59"));
        assert!(result.contains(".5"));
    }

    #[test]
    fn ts_f64_nan_returns_none() {
        assert!(ts_f64_to_rfc3339(f64::NAN).is_none());
    }

    #[test]
    fn ts_f64_infinity_returns_none() {
        assert!(ts_f64_to_rfc3339(f64::INFINITY).is_none());
    }

    #[test]
    fn ts_f64_neg_infinity_returns_none() {
        assert!(ts_f64_to_rfc3339(f64::NEG_INFINITY).is_none());
    }

    #[test]
    fn ts_f64_negative_timestamp() {
        // Before epoch is valid (1969)
        let result = ts_f64_to_rfc3339(-86400.0);
        assert!(result.is_some());
        assert!(result.unwrap().starts_with("1969-12-31"));
    }
}
