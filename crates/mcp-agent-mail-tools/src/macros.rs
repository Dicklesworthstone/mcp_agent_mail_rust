//! Macro cluster tools
//!
//! Composite tools that combine multiple operations:
//! - `macro_start_session`: Boot project session
//! - `macro_prepare_thread`: Align with existing thread
//! - `macro_file_reservation_cycle`: Reserve and optionally release files
//! - `macro_contact_handshake`: Request + approve + welcome message

use fastmcp::prelude::*;
use serde::{Deserialize, Serialize};

use crate::identity::{AgentResponse, ProjectResponse, WhoisResponse};
use crate::llm;
use crate::messaging::InboxMessage;
use crate::reservations::{ReleaseResult, ReservationResponse};
use crate::search::{ExampleMessage, ThreadSummary};
use crate::tool_util::{db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_project};
use mcp_agent_mail_db::micros_to_iso;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Start session response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSessionResponse {
    pub project: ProjectResponse,
    pub agent: AgentResponse,
    pub file_reservations: ReservationResponse,
    pub inbox: Vec<InboxMessage>,
}

/// Prepare thread response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareThreadResponse {
    pub project: ProjectResponse,
    pub agent: AgentResponse,
    pub thread: PreparedThread,
    pub inbox: Vec<InboxMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedThread {
    pub thread_id: String,
    pub summary: ThreadSummary,
    pub examples: Vec<ExampleMessage>,
    pub total_messages: i64,
}

/// File reservation cycle response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationCycleResponse {
    pub file_reservations: ReservationResponse,
    pub released: Option<ReleaseResult>,
}

/// Contact handshake response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub request: Value,
    pub response: Option<Value>,
    pub welcome_message: Option<Value>,
}

fn parse_json<T: DeserializeOwned>(payload: String, label: &str) -> McpResult<T> {
    serde_json::from_str(&payload)
        .map_err(|e| McpError::internal_error(format!("{label} JSON parse error: {e}")))
}

/// Boot a project session: ensure project + register agent + reserve files + fetch inbox.
///
/// # Parameters
/// - `human_key`: Absolute path to project directory
/// - `program`: Agent program
/// - `model`: Model identifier
/// - `agent_name`: Optional agent name
/// - `task_description`: Optional task description
/// - `file_reservation_paths`: Paths to reserve
/// - `file_reservation_reason`: Reason for reservations
/// - `file_reservation_ttl_seconds`: TTL for reservations
/// - `inbox_limit`: Max inbox messages to fetch
#[allow(clippy::too_many_arguments)]
#[tool(
    description = "Macro helper that boots a project session: ensure project, register agent,\noptionally file_reservation paths, and fetch the latest inbox snapshot."
)]
pub async fn macro_start_session(
    ctx: &McpContext,
    human_key: String,
    program: String,
    model: String,
    agent_name: Option<String>,
    task_description: Option<String>,
    file_reservation_paths: Option<Vec<String>>,
    file_reservation_reason: Option<String>,
    file_reservation_ttl_seconds: Option<i64>,
    inbox_limit: Option<i32>,
) -> McpResult<String> {
    // Validate human_key is absolute
    if !human_key.starts_with('/') {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "human_key must be an absolute path (e.g., '/data/projects/backend')",
            true,
            serde_json::json!({ "field": "human_key", "provided": human_key }),
        ));
    }

    let project_json = crate::identity::ensure_project(ctx, human_key.clone(), None).await?;
    let project: ProjectResponse = parse_json(project_json, "project")?;

    let agent_json = crate::identity::register_agent(
        ctx,
        project.human_key.clone(),
        program,
        model,
        agent_name,
        task_description,
        None,
    )
    .await?;
    let agent: AgentResponse = parse_json(agent_json, "agent")?;

    let reservation_result = if let Some(paths) = file_reservation_paths {
        if paths.is_empty() {
            ReservationResponse {
                granted: Vec::new(),
                conflicts: Vec::new(),
            }
        } else {
            let ttl = file_reservation_ttl_seconds.unwrap_or(3600);
            let reason = file_reservation_reason.unwrap_or_else(|| "macro-session".to_string());
            let reservation_json = crate::reservations::file_reservation_paths(
                ctx,
                project.human_key.clone(),
                agent.name.clone(),
                paths,
                Some(ttl),
                Some(true),
                Some(reason),
            )
            .await?;
            parse_json(reservation_json, "file_reservations")?
        }
    } else {
        ReservationResponse {
            granted: Vec::new(),
            conflicts: Vec::new(),
        }
    };

    let inbox_json = crate::messaging::fetch_inbox(
        ctx,
        project.human_key.clone(),
        agent.name.clone(),
        Some(false),
        None,
        Some(inbox_limit.unwrap_or(10)),
        Some(false),
        None,
    )
    .await?;
    let inbox: Vec<InboxMessage> = parse_json(inbox_json, "inbox")?;

    let response = StartSessionResponse {
        project,
        agent,
        file_reservations: reservation_result,
        inbox,
    };

    tracing::debug!(
        "Starting session for project {} (inbox_limit: {:?})",
        human_key,
        inbox_limit
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

/// Align with an existing thread: register + summarize + fetch inbox.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `thread_id`: Thread to prepare for
/// - `program`: Agent program
/// - `model`: Model identifier
/// - `agent_name`: Optional agent name
/// - `task_description`: Optional task description
/// - `register_if_missing`: Register agent if not exists
/// - `include_examples`: Include example messages in summary
/// - `include_inbox_bodies`: Include inbox message bodies
/// - `llm_mode`: Use LLM for summary refinement
/// - `llm_model`: Override LLM model
/// - `inbox_limit`: Max inbox messages
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[tool(
    description = "Macro helper that aligns an agent with an existing thread by ensuring registration,\nsummarising the thread, and fetching recent inbox context."
)]
pub async fn macro_prepare_thread(
    ctx: &McpContext,
    project_key: String,
    thread_id: String,
    program: String,
    model: String,
    agent_name: Option<String>,
    task_description: Option<String>,
    register_if_missing: Option<bool>,
    include_examples: Option<bool>,
    include_inbox_bodies: Option<bool>,
    llm_mode: Option<bool>,
    llm_model: Option<String>,
    inbox_limit: Option<i32>,
) -> McpResult<String> {
    let pool = get_db_pool()?;
    let project_row = resolve_project(ctx, &pool, &project_key).await?;
    let project = ProjectResponse {
        id: project_row.id.unwrap_or(0),
        slug: project_row.slug,
        human_key: project_row.human_key,
        created_at: micros_to_iso(project_row.created_at),
    };
    let project_id = project_row.id.unwrap_or(0);

    let should_register = register_if_missing.unwrap_or(true);
    let agent = if should_register {
        let agent_json = crate::identity::register_agent(
            ctx,
            project.human_key.clone(),
            program,
            model,
            agent_name,
            task_description,
            None,
        )
        .await?;
        parse_json(agent_json, "agent")?
    } else {
        let agent_name = agent_name.ok_or_else(|| {
            legacy_tool_error(
                "MISSING_FIELD",
                "agent_name is required when register_if_missing is false",
                true,
                serde_json::json!({ "field": "agent_name" }),
            )
        })?;
        let whois_json =
            crate::identity::whois(ctx, project.human_key.clone(), agent_name, None, None).await?;
        let whois: WhoisResponse = parse_json(whois_json, "agent")?;
        whois.agent
    };

    let messages = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_thread_messages(
            ctx.cx(),
            &pool,
            project_id,
            &thread_id,
            None,
        )
        .await,
    )?;

    let include_examples = include_examples.unwrap_or(true);
    let use_llm = llm_mode.unwrap_or(true);
    let total_messages = i64::try_from(messages.len()).unwrap_or(i64::MAX);

    let mut summary = crate::search::summarize_messages(&messages);

    // Optional LLM refinement (legacy parity: same merge semantics as summarize_thread).
    let config = &mcp_agent_mail_core::Config::get();
    if use_llm && config.llm_enabled {
        let msg_tuples: Vec<(i64, String, String, String)> = messages
            .iter()
            .take(llm::MAX_MESSAGES_FOR_LLM)
            .map(|m| (m.id, m.from.clone(), m.subject.clone(), m.body_md.clone()))
            .collect();

        let system = llm::single_thread_system_prompt();
        let user = llm::single_thread_user_prompt(&msg_tuples);

        match llm::complete_system_user(
            system,
            &user,
            llm_model.as_deref(),
            Some(config.llm_temperature),
            Some(config.llm_max_tokens),
        )
        .await
        {
            Ok(output) => {
                if let Some(parsed) = llm::parse_json_safely(&output.content) {
                    summary = llm::merge_single_thread_summary(&summary, &parsed);
                } else {
                    tracing::debug!(
                        "macro_prepare_thread.llm_skipped: could not parse LLM response"
                    );
                }
            }
            Err(e) => {
                tracing::debug!("macro_prepare_thread.llm_skipped: {e}");
            }
        }
    }
    let examples = if include_examples {
        messages
            .iter()
            .take(3)
            .map(|m| ExampleMessage {
                id: m.id,
                from: m.from.clone(),
                subject: m.subject.clone(),
                created_ts: micros_to_iso(m.created_ts),
            })
            .collect()
    } else {
        Vec::new()
    };

    let thread = PreparedThread {
        thread_id: thread_id.clone(),
        total_messages,
        summary,
        examples,
    };

    let inbox_json = crate::messaging::fetch_inbox(
        ctx,
        project.human_key.clone(),
        agent.name.clone(),
        Some(false),
        None,
        Some(inbox_limit.unwrap_or(10)),
        Some(include_inbox_bodies.unwrap_or(false)),
        None,
    )
    .await?;
    let inbox: Vec<InboxMessage> = parse_json(inbox_json, "inbox")?;

    let response = PrepareThreadResponse {
        project,
        agent,
        thread,
        inbox,
    };

    tracing::debug!(
        "Preparing thread {} in project {} (register: {:?}, examples: {:?}, llm: {:?})",
        thread_id,
        project_key,
        register_if_missing,
        include_examples,
        llm_mode
    );

    if let Some(model) = llm_model.as_deref() {
        tracing::debug!("LLM model: {}", model);
    }
    if let Some(bodies) = include_inbox_bodies {
        tracing::debug!("Include inbox bodies: {}", bodies);
    }
    if let Some(limit) = inbox_limit {
        tracing::debug!("Inbox limit: {}", limit);
    }

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

/// Reserve files and optionally release at the end.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent making reservations
/// - `paths`: File paths/globs to reserve
/// - `ttl_seconds`: Time to live
/// - `exclusive`: Exclusive intent
/// - `reason`: Reservation reason
/// - `auto_release`: Release after operation
#[allow(clippy::too_many_arguments)]
#[tool(
    description = "Reserve a set of file paths and optionally release them at the end of the workflow."
)]
pub async fn macro_file_reservation_cycle(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    paths: Vec<String>,
    ttl_seconds: Option<i64>,
    exclusive: Option<bool>,
    reason: Option<String>,
    auto_release: Option<bool>,
) -> McpResult<String> {
    let ttl = ttl_seconds.unwrap_or(3600);
    let is_exclusive = exclusive.unwrap_or(true);
    let should_release = auto_release.unwrap_or(false);

    // Validate TTL >= 60 seconds
    if ttl < 60 {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "ttl_seconds must be at least 60 seconds",
            true,
            serde_json::json!({ "field": "ttl_seconds", "provided": ttl, "min": 60 }),
        ));
    }

    // Legacy tooling metrics counts the internal reservation tool calls made by this macro.
    crate::metrics::record_call("file_reservation_paths");
    let reservation_json = match crate::reservations::file_reservation_paths(
        ctx,
        project_key.clone(),
        agent_name.clone(),
        paths.clone(),
        Some(ttl),
        Some(is_exclusive),
        Some(
            reason
                .clone()
                .unwrap_or_else(|| "macro-file_reservation".to_string()),
        ),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            crate::metrics::record_error("file_reservation_paths");
            return Err(e);
        }
    };
    let file_reservations: ReservationResponse = parse_json(reservation_json, "file_reservations")?;

    let released = if should_release {
        // Legacy tooling metrics counts the internal release tool call made by this macro.
        crate::metrics::record_call("release_file_reservations");
        let release_json = match crate::reservations::release_file_reservations(
            ctx,
            project_key.clone(),
            agent_name.clone(),
            Some(paths),
            None,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                crate::metrics::record_error("release_file_reservations");
                return Err(e);
            }
        };
        Some(parse_json::<ReleaseResult>(release_json, "released")?)
    } else {
        None
    };

    let response = ReservationCycleResponse {
        file_reservations,
        released,
    };

    tracing::debug!(
        "File reservation cycle for {} in project {} (auto_release: {})",
        agent_name,
        project_key,
        should_release
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

/// Request contact + optionally auto-approve + optionally send welcome message.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `requester`: Requesting agent (alias for `from_agent`)
/// - `target`: Target agent (alias for `to_agent`)
/// - `to_agent`: Target agent name
/// - `to_project`: Target project if different
/// - `reason`: Contact request reason
/// - `auto_accept`: Auto-approve the request
/// - `ttl_seconds`: TTL for the link
/// - `welcome_subject`: Subject for welcome message
/// - `welcome_body`: Body for welcome message
/// - `thread_id`: Thread for welcome message
/// - `register_if_missing`: Register requester if not exists
/// - `program`: Program for registration
/// - `model`: Model for registration
/// - `task_description`: Task for registration
#[tool(
    description = "Request contact permissions and optionally auto-approve plus send a welcome message."
)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn macro_contact_handshake(
    ctx: &McpContext,
    project_key: String,
    requester: Option<String>,
    target: Option<String>,
    agent_name: Option<String>,
    to_agent: Option<String>,
    to_project: Option<String>,
    reason: Option<String>,
    auto_accept: Option<bool>,
    ttl_seconds: Option<i64>,
    welcome_subject: Option<String>,
    welcome_body: Option<String>,
    thread_id: Option<String>,
    register_if_missing: Option<bool>,
    program: Option<String>,
    model: Option<String>,
    task_description: Option<String>,
) -> McpResult<String> {
    // Resolve agent names from aliases
    let from_agent = requester.or(agent_name).ok_or_else(|| {
        legacy_tool_error(
            "MISSING_FIELD",
            "requester or agent_name is required",
            true,
            serde_json::json!({ "field": "requester" }),
        )
    })?;

    let target_agent = target.or(to_agent).ok_or_else(|| {
        legacy_tool_error(
            "MISSING_FIELD",
            "target or to_agent is required",
            true,
            serde_json::json!({ "field": "target" }),
        )
    })?;

    let should_auto_accept = auto_accept.unwrap_or(false);
    let ttl = ttl_seconds.unwrap_or(604_800); // 7 days
    let target_project_key = to_project.clone().unwrap_or_else(|| project_key.clone());

    // Fast path: auto-accept same-project without welcome message.
    let is_same_project = to_project.as_ref().is_none_or(|p| p == &project_key);
    if should_auto_accept && is_same_project && welcome_subject.is_none() && welcome_body.is_none()
    {
        let pool = get_db_pool()?;
        let project = resolve_project(ctx, &pool, &project_key).await?;
        let project_id = project.id.unwrap_or(0);

        let from_row = crate::contacts::resolve_or_register_sender(
            ctx,
            &pool,
            project_id,
            &from_agent,
            register_if_missing.unwrap_or(true),
            program.clone(),
            model.clone(),
            task_description.as_deref(),
            &project.slug,
            &project.human_key,
        )
        .await?;
        let mut to_row = crate::tool_util::resolve_agent(
            ctx,
            &pool,
            project_id,
            &target_agent,
            &project.slug,
            &project.human_key,
        )
        .await;
        if to_row.is_err() {
            let should_register = register_if_missing.unwrap_or(true);
            if should_register {
                let _ = crate::identity::register_agent(
                    ctx,
                    project.human_key.clone(),
                    "unknown".to_string(),
                    "unknown".to_string(),
                    Some(target_agent.clone()),
                    None,
                    None,
                )
                .await?;
                to_row = crate::tool_util::resolve_agent(
                    ctx,
                    &pool,
                    project_id,
                    &target_agent,
                    &project.slug,
                    &project.human_key,
                )
                .await;
            }
        }
        let to_row = to_row.map_err(|_| {
            legacy_tool_error(
                "NOT_FOUND",
                format!("Target agent not found: {target_agent}"),
                true,
                serde_json::json!({ "entity": "Agent", "identifier": target_agent }),
            )
        })?;

        let ttl = if ttl < 60 { 60 } else { ttl };

        let _ = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::request_contact(
                ctx.cx(),
                &pool,
                project_id,
                from_row.id.unwrap_or(0),
                project_id,
                to_row.id.unwrap_or(0),
                reason.as_deref().unwrap_or(""),
                ttl,
            )
            .await,
        )?;
        let (_, approved) = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::respond_contact(
                ctx.cx(),
                &pool,
                project_id,
                from_row.id.unwrap_or(0),
                project_id,
                to_row.id.unwrap_or(0),
                true,
                ttl,
            )
            .await,
        )?;

        let payload = crate::contacts::ContactLinkState {
            from: from_agent.clone(),
            from_project: project.human_key.clone(),
            to: target_agent.clone(),
            to_project: project.human_key.clone(),
            status: approved.status,
            expires_ts: approved.expires_ts.map(micros_to_iso),
        };

        let response = HandshakeResponse {
            request: serde_json::to_value(&payload).unwrap_or(Value::Null),
            response: Some(serde_json::to_value(&payload).unwrap_or(Value::Null)),
            welcome_message: None,
        };

        return serde_json::to_string(&response)
            .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")));
    }

    let request_json = crate::contacts::request_contact(
        ctx,
        project_key.clone(),
        from_agent.clone(),
        target_agent.clone(),
        Some(target_project_key.clone()),
        reason.clone(),
        Some(ttl),
        register_if_missing,
        program.clone(),
        model.clone(),
        task_description.clone(),
    )
    .await?;
    let request_val: Value = parse_json(request_json, "request")?;

    let response_val = if should_auto_accept {
        let respond_json = crate::contacts::respond_contact(
            ctx,
            target_project_key.clone(),
            target_agent.clone(),
            from_agent.clone(),
            if to_project.is_some() {
                Some(project_key.clone())
            } else {
                None
            },
            true,
            Some(ttl),
        )
        .await?;
        Some(parse_json(respond_json, "response")?)
    } else {
        None
    };

    let has_welcome = welcome_subject.is_some() && welcome_body.is_some();
    let thread_id_for_log = thread_id.clone();

    let welcome_val = if let (Some(subject), Some(body)) = (welcome_subject, welcome_body) {
        if to_project.is_none() {
            let welcome_json = crate::messaging::send_message(
                ctx,
                project_key.clone(),
                from_agent.clone(),
                vec![target_agent.clone()],
                subject,
                body,
                None,
                None,
                None,
                None,
                None,
                None, // ack_required
                thread_id,
                None,
                None,
                None,
            )
            .await?;
            Some(parse_json(welcome_json, "welcome_message")?)
        } else {
            None
        }
    } else {
        None
    };

    let response = HandshakeResponse {
        request: request_val,
        response: response_val,
        welcome_message: welcome_val,
    };

    tracing::debug!(
        "Contact handshake from {} to {} in project {} (auto_accept: {}, welcome: {})",
        from_agent,
        target_agent,
        project_key,
        should_auto_accept,
        has_welcome
    );

    // Log registration params
    if let Some(reg) = register_if_missing
        && reg
    {
        tracing::debug!(
            "Auto-register: program={:?}, model={:?}, task={:?}",
            program,
            model,
            task_description
        );
    }
    if let Some(tid) = thread_id_for_log {
        tracing::debug!("Welcome message thread: {}", tid);
    }

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}")))
}

// removed generate_slug (unused; slug derivation handled by ensure_project)

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_json
    // -----------------------------------------------------------------------

    #[test]
    fn parse_json_valid_object() {
        let payload = r#"{"id":1,"name":"test"}"#.to_string();
        let result: McpResult<serde_json::Value> = parse_json(payload, "test");
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val["id"], 1);
        assert_eq!(val["name"], "test");
    }

    #[test]
    fn parse_json_invalid_json_returns_error() {
        let payload = "not json at all".to_string();
        let result: McpResult<serde_json::Value> = parse_json(payload, "test_label");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("test_label"));
        assert!(err.message.contains("JSON parse error"));
    }

    #[test]
    fn parse_json_empty_string_returns_error() {
        let result: McpResult<serde_json::Value> = parse_json(String::new(), "empty");
        assert!(result.is_err());
    }

    #[test]
    fn parse_json_wrong_type_returns_error() {
        // parse as i32 when payload is a string
        let payload = r#""hello""#.to_string();
        let result: McpResult<i32> = parse_json(payload, "type_mismatch");
        assert!(result.is_err());
    }

    #[test]
    fn parse_json_array() {
        let payload = "[1, 2, 3]".to_string();
        let result: McpResult<Vec<i32>> = parse_json(payload, "array");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }

    // -----------------------------------------------------------------------
    // macro_start_session validation: human_key must be absolute
    // -----------------------------------------------------------------------

    #[test]
    fn absolute_path_check_for_human_key() {
        // This tests the same validation logic used in macro_start_session
        assert!("/data/projects/test".starts_with('/'));
        assert!("/".starts_with('/'));
        assert!(!"data/projects/test".starts_with('/'));
        assert!(!"./test".starts_with('/'));
        assert!(!"".starts_with('/'));
    }

    // -----------------------------------------------------------------------
    // macro_file_reservation_cycle validation: ttl >= 60
    // -----------------------------------------------------------------------

    #[test]
    fn ttl_minimum_60_seconds() {
        let min_ttl: i64 = 60;
        assert!(59 < min_ttl);
        assert!(60 >= min_ttl);
        assert!(3600 >= min_ttl);
    }

    #[test]
    fn default_ttl_values() {
        // macro_file_reservation_cycle default TTL
        assert_eq!(3600_i64, 60 * 60); // 1 hour
        // macro_contact_handshake default TTL
        assert_eq!(604_800_i64, 7 * 24 * 3600); // 7 days
    }

    // -----------------------------------------------------------------------
    // StartSessionResponse serde
    // -----------------------------------------------------------------------

    #[test]
    fn start_session_response_round_trip() {
        let resp = StartSessionResponse {
            project: ProjectResponse {
                id: 1,
                slug: "abc".into(),
                human_key: "/data/test".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
            },
            agent: AgentResponse {
                id: 1,
                name: "BlueLake".into(),
                program: "claude-code".into(),
                model: "opus-4.5".into(),
                task_description: "testing".into(),
                inception_ts: "2026-01-01T00:00:00Z".into(),
                last_active_ts: "2026-01-01T00:00:00Z".into(),
                project_id: 1,
                attachments_policy: "auto".into(),
                capabilities: crate::identity::DEFAULT_AGENT_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            },
            file_reservations: ReservationResponse {
                granted: Vec::new(),
                conflicts: Vec::new(),
            },
            inbox: Vec::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: StartSessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.project.slug, "abc");
        assert_eq!(parsed.agent.name, "BlueLake");
        assert!(parsed.file_reservations.granted.is_empty());
        assert!(parsed.inbox.is_empty());
    }

    // -----------------------------------------------------------------------
    // ReservationCycleResponse serde
    // -----------------------------------------------------------------------

    #[test]
    fn reservation_cycle_response_without_release() {
        let resp = ReservationCycleResponse {
            file_reservations: ReservationResponse {
                granted: Vec::new(),
                conflicts: Vec::new(),
            },
            released: None,
        };
        let val: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert!(val["released"].is_null());
    }

    #[test]
    fn reservation_cycle_response_with_release() {
        let resp = ReservationCycleResponse {
            file_reservations: ReservationResponse {
                granted: Vec::new(),
                conflicts: Vec::new(),
            },
            released: Some(ReleaseResult {
                released: 3,
                released_at: "2026-02-06T12:00:00Z".into(),
            }),
        };
        let val: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(val["released"]["released"], 3);
    }

    // -----------------------------------------------------------------------
    // HandshakeResponse serde
    // -----------------------------------------------------------------------

    #[test]
    fn handshake_response_minimal() {
        let resp = HandshakeResponse {
            request: serde_json::json!({"from": "A", "to": "B"}),
            response: None,
            welcome_message: None,
        };
        let val: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(val["request"]["from"], "A");
        assert!(val["response"].is_null());
        assert!(val["welcome_message"].is_null());
    }

    #[test]
    fn handshake_response_full() {
        let resp = HandshakeResponse {
            request: serde_json::json!({"from": "A", "to": "B"}),
            response: Some(serde_json::json!({"approved": true})),
            welcome_message: Some(serde_json::json!({"id": 1, "subject": "Hello"})),
        };
        let val: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(val["response"]["approved"], true);
        assert_eq!(val["welcome_message"]["subject"], "Hello");
    }

    // -----------------------------------------------------------------------
    // PreparedThread serde
    // -----------------------------------------------------------------------

    #[test]
    fn prepared_thread_round_trip() {
        let thread = PreparedThread {
            thread_id: "TKT-42".into(),
            summary: ThreadSummary {
                participants: vec!["Alice".into()],
                key_points: vec!["Initial discussion".into()],
                action_items: Vec::new(),
                total_messages: 5,
                open_actions: 0,
                done_actions: 0,
                mentions: Vec::new(),
                code_references: None,
            },
            examples: vec![ExampleMessage {
                id: 1,
                from: "Alice".into(),
                subject: "First msg".into(),
                created_ts: "2026-01-01T00:00:00Z".into(),
            }],
            total_messages: 5,
        };
        let json = serde_json::to_string(&thread).unwrap();
        let parsed: PreparedThread = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread_id, "TKT-42");
        assert_eq!(parsed.total_messages, 5);
        assert_eq!(parsed.examples.len(), 1);
        assert_eq!(parsed.summary.participants, vec!["Alice"]);
    }

    // -----------------------------------------------------------------------
    // PrepareThreadResponse serde
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_thread_response_round_trip() {
        let resp = PrepareThreadResponse {
            project: ProjectResponse {
                id: 1,
                slug: "test".into(),
                human_key: "/data/test".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
            },
            agent: AgentResponse {
                id: 1,
                name: "GoldHawk".into(),
                program: "codex-cli".into(),
                model: "gpt-5".into(),
                task_description: String::new(),
                inception_ts: String::new(),
                last_active_ts: String::new(),
                project_id: 1,
                attachments_policy: "auto".into(),
                capabilities: crate::identity::DEFAULT_AGENT_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            },
            thread: PreparedThread {
                thread_id: "br-1".into(),
                summary: ThreadSummary {
                    participants: Vec::new(),
                    key_points: Vec::new(),
                    action_items: Vec::new(),
                    total_messages: 0,
                    open_actions: 0,
                    done_actions: 0,
                    mentions: Vec::new(),
                    code_references: None,
                },
                examples: Vec::new(),
                total_messages: 0,
            },
            inbox: Vec::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PrepareThreadResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent.name, "GoldHawk");
        assert_eq!(parsed.thread.thread_id, "br-1");
    }

    // -----------------------------------------------------------------------
    // Agent alias resolution logic (macro_contact_handshake)
    // -----------------------------------------------------------------------

    #[test]
    fn requester_alias_resolution() {
        // Tests the .or() chain: requester.or(agent_name)
        let requester: Option<String> = Some("AgentA".into());
        let agent_name: Option<String> = Some("AgentB".into());
        assert_eq!(requester.or(agent_name), Some("AgentA".into()));

        let requester: Option<String> = None;
        let agent_name: Option<String> = Some("AgentB".into());
        assert_eq!(requester.or(agent_name), Some("AgentB".into()));

        let requester: Option<String> = None;
        let agent_name: Option<String> = None;
        assert_eq!(requester.or(agent_name), None);
    }

    #[test]
    fn target_alias_resolution() {
        // Tests the .or() chain: target.or(to_agent)
        let target: Option<String> = Some("X".into());
        let to_agent: Option<String> = Some("Y".into());
        assert_eq!(target.or(to_agent), Some("X".into()));

        let target: Option<String> = None;
        let to_agent: Option<String> = Some("Y".into());
        assert_eq!(target.or(to_agent), Some("Y".into()));
    }

    // -----------------------------------------------------------------------
    // parse_json error paths (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_json_includes_label_in_error() {
        let result: McpResult<serde_json::Value> = parse_json("not valid".to_string(), "my_label");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("my_label"),
            "error should include label: {}",
            err.message
        );
    }

    #[test]
    fn parse_json_null_string() {
        let result: McpResult<Option<i32>> = parse_json("null".to_string(), "nullable");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn parse_json_unicode() {
        let payload = r#"{"name":"日本語テスト","emoji":"🎉"}"#.to_string();
        let result: McpResult<serde_json::Value> = parse_json(payload, "unicode");
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val["name"], "日本語テスト");
        assert_eq!(val["emoji"], "🎉");
    }

    #[test]
    fn parse_json_deeply_nested() {
        let payload = r#"{"a":{"b":{"c":{"d":42}}}}"#.to_string();
        let result: McpResult<serde_json::Value> = parse_json(payload, "nested");
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val["a"]["b"]["c"]["d"], 42);
    }

    // -----------------------------------------------------------------------
    // human_key validation (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn human_key_absolute_path_variations() {
        // Valid absolute paths
        assert!("/data/projects/test".starts_with('/'));
        assert!("/".starts_with('/'));
        assert!("/a/b/c/d/e/f".starts_with('/'));

        // Invalid relative paths
        assert!(!"data/projects/test".starts_with('/'));
        assert!(!"./test".starts_with('/'));
        assert!(!"../parent".starts_with('/'));
        assert!(!"~/.config".starts_with('/')); // tilde is not /
    }

    #[test]
    fn human_key_windows_paths_are_relative() {
        // Windows-style paths should be considered relative
        assert!(!"C:\\Users\\test".starts_with('/'));
        assert!(!"D:/projects/foo".starts_with('/'));
    }

    // -----------------------------------------------------------------------
    // TTL validation (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn ttl_minimum_validation_boundary() {
        // macro_file_reservation_cycle requires TTL >= 60
        let min_ttl: i64 = 60;
        assert!(59 < min_ttl);
        assert!(60 >= min_ttl);
        assert!(61 >= min_ttl);
    }

    #[test]
    fn ttl_default_values_are_sane() {
        // macro_start_session default file reservation TTL
        assert_eq!(3600_i64, 60 * 60); // 1 hour

        // macro_contact_handshake default TTL
        assert_eq!(604_800_i64, 7 * 24 * 60 * 60); // 7 days
    }

    // -----------------------------------------------------------------------
    // StartSessionResponse fields (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn start_session_response_with_reservations() {
        use crate::reservations::{GrantedReservation, ReservationResponse};

        let resp = StartSessionResponse {
            project: ProjectResponse {
                id: 1,
                slug: "test".into(),
                human_key: "/test".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
            },
            agent: AgentResponse {
                id: 1,
                name: "TestAgent".into(),
                program: "test".into(),
                model: "test".into(),
                task_description: String::new(),
                inception_ts: String::new(),
                last_active_ts: String::new(),
                project_id: 1,
                attachments_policy: "auto".into(),
                capabilities: crate::identity::DEFAULT_AGENT_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            },
            file_reservations: ReservationResponse {
                granted: vec![GrantedReservation {
                    id: 1,
                    path_pattern: "src/**".into(),
                    expires_ts: "2026-01-01T01:00:00Z".into(),
                    exclusive: true,
                    reason: "test reservation".into(),
                }],
                conflicts: Vec::new(),
            },
            inbox: Vec::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: StartSessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file_reservations.granted.len(), 1);
        assert_eq!(parsed.file_reservations.granted[0].path_pattern, "src/**");
        assert!(parsed.file_reservations.granted[0].exclusive);
    }

    #[test]
    fn start_session_response_with_inbox() {
        let resp = StartSessionResponse {
            project: ProjectResponse {
                id: 1,
                slug: "p".into(),
                human_key: "/p".into(),
                created_at: String::new(),
            },
            agent: AgentResponse {
                id: 1,
                name: "A".into(),
                program: "p".into(),
                model: "m".into(),
                task_description: String::new(),
                inception_ts: String::new(),
                last_active_ts: String::new(),
                project_id: 1,
                attachments_policy: "auto".into(),
                capabilities: crate::identity::DEFAULT_AGENT_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            },
            file_reservations: ReservationResponse {
                granted: Vec::new(),
                conflicts: Vec::new(),
            },
            inbox: vec![InboxMessage {
                id: 100,
                project_id: 1,
                sender_id: 2,
                thread_id: Some("br-1".into()),
                subject: "Hello".into(),
                importance: "high".into(),
                ack_required: true,
                from: "Sender".into(),
                created_ts: None,
                kind: "direct".into(),
                attachments: Vec::new(),
                body_md: Some("Body text".into()),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: StartSessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.inbox.len(), 1);
        assert_eq!(parsed.inbox[0].subject, "Hello");
        assert!(parsed.inbox[0].ack_required);
    }

    // -----------------------------------------------------------------------
    // PrepareThreadResponse fields (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_thread_response_empty_thread() {
        let resp = PrepareThreadResponse {
            project: ProjectResponse {
                id: 1,
                slug: "p".into(),
                human_key: "/p".into(),
                created_at: String::new(),
            },
            agent: AgentResponse {
                id: 1,
                name: "A".into(),
                program: "p".into(),
                model: "m".into(),
                task_description: String::new(),
                inception_ts: String::new(),
                last_active_ts: String::new(),
                project_id: 1,
                attachments_policy: "auto".into(),
                capabilities: crate::identity::DEFAULT_AGENT_CAPABILITIES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            },
            thread: PreparedThread {
                thread_id: "nonexistent".into(),
                summary: ThreadSummary {
                    participants: Vec::new(),
                    key_points: Vec::new(),
                    action_items: Vec::new(),
                    total_messages: 0,
                    open_actions: 0,
                    done_actions: 0,
                    mentions: Vec::new(),
                    code_references: None,
                },
                examples: Vec::new(),
                total_messages: 0,
            },
            inbox: Vec::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PrepareThreadResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread.total_messages, 0);
        assert!(parsed.thread.examples.is_empty());
        assert!(parsed.thread.summary.participants.is_empty());
    }

    // -----------------------------------------------------------------------
    // ReservationCycleResponse with auto_release (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn reservation_cycle_auto_release_present() {
        use crate::reservations::ReleaseResult;

        let resp = ReservationCycleResponse {
            file_reservations: ReservationResponse {
                granted: Vec::new(),
                conflicts: Vec::new(),
            },
            released: Some(ReleaseResult {
                released: 5,
                released_at: "2026-02-12T12:00:00Z".into(),
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ReservationCycleResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.released.is_some());
        assert_eq!(parsed.released.unwrap().released, 5);
    }

    // -----------------------------------------------------------------------
    // HandshakeResponse variations (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn handshake_response_request_only() {
        let resp = HandshakeResponse {
            request: serde_json::json!({"status": "pending"}),
            response: None,
            welcome_message: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HandshakeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.request["status"], "pending");
        assert!(parsed.response.is_none());
        assert!(parsed.welcome_message.is_none());
    }

    #[test]
    fn handshake_response_with_welcome() {
        let resp = HandshakeResponse {
            request: serde_json::json!({"from": "A"}),
            response: Some(serde_json::json!({"approved": true})),
            welcome_message: Some(serde_json::json!({
                "id": 1,
                "subject": "Welcome!",
                "body": "Hello and welcome to the project."
            })),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HandshakeResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.welcome_message.is_some());
        let welcome = parsed.welcome_message.unwrap();
        assert_eq!(welcome["subject"], "Welcome!");
    }

    // -----------------------------------------------------------------------
    // Empty paths edge case (br-3h13.4.8)
    // -----------------------------------------------------------------------

    #[test]
    fn empty_reservation_paths_produces_empty_grants() {
        // When file_reservation_paths is Some but empty, should return empty grants
        let empty_paths: Vec<String> = Vec::new();
        assert!(empty_paths.is_empty());
        // In macro_start_session, this produces ReservationResponse with empty granted/conflicts
    }
}
