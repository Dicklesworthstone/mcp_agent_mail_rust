//! Messaging cluster tools
//!
//! Tools for message sending and inbox management:
//! - `send_message`: Send a message to recipients
//! - `reply_message`: Reply to an existing message
//! - `fetch_inbox`: Retrieve inbox messages
//! - `mark_message_read`: Mark message as read
//! - `acknowledge_message`: Acknowledge a message

use fastmcp::McpErrorCode;
use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::micros_to_iso;
use serde::{Deserialize, Serialize};

use crate::tool_util::{db_outcome_to_mcp_result, get_db_pool, resolve_agent, resolve_project};

/// Write a message bundle to the git archive (best-effort, non-blocking).
/// Failures are logged but never fail the tool call.
fn try_write_message_archive(
    project_slug: &str,
    message_json: &serde_json::Value,
    body_md: &str,
    sender: &str,
    all_recipient_names: &[String],
) {
    let config = Config::from_env();
    match mcp_agent_mail_storage::ensure_archive(&config, project_slug) {
        Ok(archive) => {
            if let Err(e) = mcp_agent_mail_storage::write_message_bundle(
                &archive,
                &config,
                message_json,
                body_md,
                sender,
                all_recipient_names,
                &[],
                None,
            ) {
                tracing::warn!("Failed to write message bundle to archive: {e}");
            }
        }
        Err(e) => {
            tracing::warn!("Failed to ensure archive for message write: {e}");
        }
    }
}

/// Message delivery result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub project: String,
    pub payload: MessagePayload,
}

/// Send message response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub deliveries: Vec<DeliveryResult>,
    pub count: usize,
    pub attachments: Vec<String>,
}

/// Message payload in responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePayload {
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
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

/// Inbox message summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
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
}

/// Read status response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadStatusResponse {
    pub message_id: i64,
    pub read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
}

/// Acknowledge status response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckStatusResponse {
    pub message_id: i64,
    pub acknowledged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
}

/// Reply message response (includes both message fields and deliveries)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyMessageResponse {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: Option<String>,
    pub attachments: Vec<String>,
    pub body_md: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: i64,
    pub deliveries: Vec<DeliveryResult>,
    pub count: usize,
}

/// Send a message to one or more recipients.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `sender_name`: Sender agent name
/// - `to`: Primary recipients (required, at least one)
/// - `subject`: Message subject
/// - `body_md`: Message body in Markdown
/// - `cc`: CC recipients (optional)
/// - `bcc`: BCC recipients (optional)
/// - `attachment_paths`: File paths to attach (optional)
/// - `convert_images`: Override image conversion (optional)
/// - `importance`: Message importance: low, normal, high, urgent (default: normal)
/// - `ack_required`: Request acknowledgement (default: false)
/// - `thread_id`: Associate with existing thread (optional)
/// - `auto_contact_if_blocked`: Auto-request contact if blocked (optional)
#[allow(
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::too_many_lines
)]
#[tool(description = "Send a Markdown message to one or more recipients.")]
pub async fn send_message(
    ctx: &McpContext,
    project_key: String,
    sender_name: String,
    to: Vec<String>,
    subject: String,
    body_md: String,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    attachment_paths: Option<Vec<String>>,
    convert_images: Option<bool>,
    importance: Option<String>,
    ack_required: Option<bool>,
    thread_id: Option<String>,
    auto_contact_if_blocked: Option<bool>,
) -> McpResult<String> {
    // Validate recipients
    if to.is_empty() {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "At least one recipient (to) is required",
        ));
    }

    // Validate importance
    let importance_val = importance.unwrap_or_else(|| "normal".to_string());
    if !["low", "normal", "high", "urgent"].contains(&importance_val.as_str()) {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!("Invalid importance '{importance_val}'. Must be: low, normal, high, or urgent"),
        ));
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Resolve sender
    let sender = resolve_agent(ctx, &pool, project_id, &sender_name).await?;
    let sender_id = sender.id.unwrap_or(0);

    // Resolve all recipients (to, cc, bcc)
    let cc_list = cc.unwrap_or_default();
    let bcc_list = bcc.unwrap_or_default();

    let mut all_recipients: Vec<(i64, &str)> = Vec::new();
    let mut resolved_to: Vec<String> = Vec::new();
    let mut resolved_cc_recipients: Vec<String> = Vec::new();
    let mut resolved_bcc_recipients: Vec<String> = Vec::new();

    for name in &to {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "to"));
        resolved_to.push(agent.name);
    }
    for name in &cc_list {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "cc"));
        resolved_cc_recipients.push(agent.name);
    }
    for name in &bcc_list {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "bcc"));
        resolved_bcc_recipients.push(agent.name);
    }

    // Log optional parameters for debugging
    if let Some(paths) = &attachment_paths {
        tracing::debug!("Attachments: {:?}", paths);
    }
    if let Some(convert) = convert_images {
        tracing::debug!("Convert images: {}", convert);
    }
    if let Some(auto_contact) = auto_contact_if_blocked {
        tracing::debug!("Auto contact if blocked: {}", auto_contact);
    }

    // Serialize attachments as JSON array
    let attachments_json = serde_json::to_string(&attachment_paths.clone().unwrap_or_default())
        .unwrap_or_else(|_| "[]".to_string());

    // Create message in DB
    let message = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::create_message(
            ctx.cx(),
            &pool,
            project_id,
            sender_id,
            &subject,
            &body_md,
            thread_id.as_deref(),
            &importance_val,
            ack_required.unwrap_or(false),
            &attachments_json,
        )
        .await,
    )?;

    let message_id = message.id.unwrap_or(0);

    // Add recipients
    db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::add_recipients(ctx.cx(), &pool, message_id, &all_recipients)
            .await,
    )?;

    let attachments = attachment_paths.unwrap_or_default();

    // Write message bundle to git archive (best-effort)
    {
        let mut all_recipient_names: Vec<String> = resolved_to.clone();
        all_recipient_names.extend(resolved_cc_recipients.clone());
        all_recipient_names.extend(resolved_bcc_recipients.clone());

        let msg_json = serde_json::json!({
            "id": message_id,
            "from": &sender_name,
            "to": &resolved_to,
            "cc": &resolved_cc_recipients,
            "bcc": &resolved_bcc_recipients,
            "subject": &message.subject,
            "created": micros_to_iso(message.created_ts),
            "thread_id": &message.thread_id,
            "project": &project.human_key,
            "project_slug": &project.slug,
            "importance": &message.importance,
            "ack_required": message.ack_required != 0,
            "attachments": &attachments,
        });
        try_write_message_archive(
            &project.slug,
            &msg_json,
            &message.body_md,
            &sender_name,
            &all_recipient_names,
        );
    }

    let payload = MessagePayload {
        id: message_id,
        project_id,
        sender_id,
        thread_id: message.thread_id,
        subject: message.subject,
        body_md: message.body_md,
        importance: message.importance,
        ack_required: message.ack_required != 0,
        created_ts: Some(micros_to_iso(message.created_ts)),
        attachments: attachments.clone(),
        from: sender_name.clone(),
        to: resolved_to,
        cc: resolved_cc_recipients,
        bcc: resolved_bcc_recipients,
    };

    let response = SendMessageResponse {
        deliveries: vec![DeliveryResult {
            project: project.human_key.clone(),
            payload,
        }],
        count: 1,
        attachments,
    };

    tracing::debug!(
        "Sent message {} from {} to {:?} in project {}",
        message_id,
        sender_name,
        to,
        project_key
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Reply to an existing message, preserving or establishing a thread.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `message_id`: ID of message to reply to
/// - `sender_name`: Sender agent name
/// - `body_md`: Reply body in Markdown
/// - `to`: Override recipients (defaults to original sender)
/// - `cc`: CC recipients
/// - `bcc`: BCC recipients
/// - `subject_prefix`: Prefix for subject (default: "Re:")
#[allow(
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::too_many_lines
)]
#[tool(description = "Reply to an existing message, preserving thread context.")]
pub async fn reply_message(
    ctx: &McpContext,
    project_key: String,
    message_id: i64,
    sender_name: String,
    body_md: String,
    to: Option<Vec<String>>,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    subject_prefix: Option<String>,
) -> McpResult<String> {
    let prefix = subject_prefix.unwrap_or_else(|| "Re:".to_string());

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Fetch original message to inherit properties
    let original = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_message(ctx.cx(), &pool, message_id).await,
    )?;
    if original.project_id != project_id {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            "Message not found",
        ));
    }

    // Resolve sender
    let sender = resolve_agent(ctx, &pool, project_id, &sender_name).await?;
    let sender_id = sender.id.unwrap_or(0);

    // Resolve original sender name for default recipient
    let original_sender = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_agent_by_id(ctx.cx(), &pool, original.sender_id).await,
    )?;

    // Determine thread_id: use original's thread_id, or the original message id as string
    let thread_id = original
        .thread_id
        .clone()
        .unwrap_or_else(|| message_id.to_string());

    // Apply subject prefix if not already present
    let subject = if original.subject.starts_with(&prefix) {
        original.subject.clone()
    } else {
        format!("{prefix} {}", original.subject)
    };

    // Default to to original sender if not specified
    let to_names = to.unwrap_or_else(|| vec![original_sender.name.clone()]);
    let cc_names = cc.unwrap_or_default();
    let bcc_names = bcc.unwrap_or_default();

    // Resolve all recipients
    let mut all_recipients: Vec<(i64, &str)> = Vec::new();
    let mut resolved_to: Vec<String> = Vec::new();
    let mut resolved_cc_recipients: Vec<String> = Vec::new();
    let mut resolved_bcc_recipients: Vec<String> = Vec::new();

    for name in &to_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "to"));
        resolved_to.push(agent.name);
    }
    for name in &cc_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "cc"));
        resolved_cc_recipients.push(agent.name);
    }
    for name in &bcc_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "bcc"));
        resolved_bcc_recipients.push(agent.name);
    }

    // Create reply message - inherit importance and ack_required from original
    let reply = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::create_message(
            ctx.cx(),
            &pool,
            project_id,
            sender_id,
            &subject,
            &body_md,
            Some(&thread_id),
            &original.importance,
            original.ack_required != 0,
            "[]", // No attachments for reply by default
        )
        .await,
    )?;

    let reply_id = reply.id.unwrap_or(0);

    // Add recipients
    db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::add_recipients(ctx.cx(), &pool, reply_id, &all_recipients)
            .await,
    )?;

    // Write reply message bundle to git archive (best-effort)
    {
        let mut all_recipient_names: Vec<String> = resolved_to.clone();
        all_recipient_names.extend(resolved_cc_recipients.clone());
        all_recipient_names.extend(resolved_bcc_recipients.clone());

        let msg_json = serde_json::json!({
            "id": reply_id,
            "from": &sender_name,
            "to": &resolved_to,
            "cc": &resolved_cc_recipients,
            "bcc": &resolved_bcc_recipients,
            "subject": &reply.subject,
            "created": micros_to_iso(reply.created_ts),
            "thread_id": &thread_id,
            "project": &project.human_key,
            "project_slug": &project.slug,
            "importance": &reply.importance,
            "ack_required": reply.ack_required != 0,
            "attachments": serde_json::Value::Array(vec![]),
            "reply_to": message_id,
        });
        try_write_message_archive(
            &project.slug,
            &msg_json,
            &reply.body_md,
            &sender_name,
            &all_recipient_names,
        );
    }

    let payload = MessagePayload {
        id: reply_id,
        project_id,
        sender_id,
        thread_id: Some(thread_id.clone()),
        subject: reply.subject.clone(),
        body_md: reply.body_md.clone(),
        importance: reply.importance.clone(),
        ack_required: reply.ack_required != 0,
        created_ts: Some(micros_to_iso(reply.created_ts)),
        attachments: vec![],
        from: sender_name.clone(),
        to: resolved_to.clone(),
        cc: resolved_cc_recipients.clone(),
        bcc: resolved_bcc_recipients.clone(),
    };

    let response = ReplyMessageResponse {
        id: reply_id,
        project_id,
        sender_id,
        thread_id: Some(thread_id),
        subject: reply.subject,
        importance: reply.importance,
        ack_required: reply.ack_required != 0,
        created_ts: Some(micros_to_iso(reply.created_ts)),
        attachments: vec![],
        body_md: reply.body_md,
        from: sender_name.clone(),
        to: resolved_to,
        cc: resolved_cc_recipients,
        bcc: resolved_bcc_recipients,
        reply_to: message_id,
        deliveries: vec![DeliveryResult {
            project: project.human_key.clone(),
            payload,
        }],
        count: 1,
    };

    tracing::debug!(
        "Replied to message {} with message {} from {} in project {}",
        message_id,
        reply_id,
        sender_name,
        project_key
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Retrieve recent messages for an agent without mutating read/ack state.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent to fetch inbox for
/// - `urgent_only`: Only high/urgent importance (default: false)
/// - `since_ts`: Only messages after this timestamp
/// - `limit`: Max messages to return (default: 20)
/// - `include_bodies`: Include full message bodies (default: false)
#[tool(description = "Retrieve recent messages for an agent without mutating state.")]
pub async fn fetch_inbox(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    urgent_only: Option<bool>,
    since_ts: Option<String>,
    limit: Option<i32>,
    include_bodies: Option<bool>,
) -> McpResult<String> {
    let mut msg_limit = limit.unwrap_or(20);
    if msg_limit < 1 {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!("limit must be at least 1, got {msg_limit}"),
        ));
    }
    if msg_limit > 1000 {
        tracing::info!(
            "fetch_inbox limit {} is very large; capping at 1000",
            msg_limit
        );
        msg_limit = 1000;
    }
    let msg_limit = usize::try_from(msg_limit)
        .map_err(|_| McpError::new(McpErrorCode::InvalidParams, "limit exceeds supported range"))?;
    let include_body = include_bodies.unwrap_or(false);
    let urgent = urgent_only.unwrap_or(false);

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    // Parse since_ts if provided (ISO-8601 to micros)
    let since_micros: Option<i64> = if let Some(ts) = &since_ts {
        Some(mcp_agent_mail_db::iso_to_micros(ts).ok_or_else(|| {
            McpError::new(
                McpErrorCode::InvalidParams,
                format!("Invalid since_ts format: {ts}"),
            )
        })?)
    } else {
        None
    };

    let inbox_rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::fetch_inbox(
            ctx.cx(),
            &pool,
            project_id,
            agent_id,
            urgent,
            since_micros,
            msg_limit,
        )
        .await,
    )?;

    let messages: Vec<InboxMessage> = inbox_rows
        .into_iter()
        .map(|row| {
            let attachments: Vec<String> =
                serde_json::from_str(&row.message.attachments).unwrap_or_default();
            InboxMessage {
                id: row.message.id.unwrap_or(0),
                project_id: row.message.project_id,
                sender_id: row.message.sender_id,
                thread_id: row.message.thread_id,
                subject: row.message.subject,
                importance: row.message.importance,
                ack_required: row.message.ack_required != 0,
                from: row.sender_name,
                created_ts: Some(micros_to_iso(row.message.created_ts)),
                kind: row.kind,
                attachments,
                body_md: if include_body {
                    Some(row.message.body_md)
                } else {
                    None
                },
            }
        })
        .collect();

    tracing::debug!(
        "Fetched {} messages for {} in project {} (limit: {}, urgent: {}, since: {:?})",
        messages.len(),
        agent_name,
        project_key,
        msg_limit,
        urgent,
        since_ts
    );

    serde_json::to_string(&messages)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Mark a message as read for the given agent.
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent marking as read
/// - `message_id`: Message to mark
///
/// # Returns
/// Read status with timestamp
#[tool(description = "Mark a specific message as read for the given agent.")]
pub async fn mark_message_read(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    message_id: i64,
) -> McpResult<String> {
    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    // Idempotent - returns timestamp when read (new or existing)
    let read_ts = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::mark_message_read(ctx.cx(), &pool, agent_id, message_id).await,
    )?;

    let response = ReadStatusResponse {
        message_id,
        read: true,
        read_at: Some(micros_to_iso(read_ts)),
    };

    tracing::debug!(
        "Marked message {} as read for {} in project {}",
        message_id,
        agent_name,
        project_key
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}

/// Acknowledge a message (also marks as read).
///
/// # Parameters
/// - `project_key`: Project identifier
/// - `agent_name`: Agent acknowledging
/// - `message_id`: Message to acknowledge
///
/// # Returns
/// Acknowledgement status with timestamps
#[tool(description = "Acknowledge a message addressed to an agent (and mark as read).")]
pub async fn acknowledge_message(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    message_id: i64,
) -> McpResult<String> {
    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(ctx, &pool, project_id, &agent_name).await?;
    let agent_id = agent.id.unwrap_or(0);

    // Sets both read_ts and ack_ts - idempotent
    let (read_ts, ack_ts) = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::acknowledge_message(ctx.cx(), &pool, agent_id, message_id)
            .await,
    )?;

    let response = AckStatusResponse {
        message_id,
        acknowledged: true,
        acknowledged_at: Some(micros_to_iso(ack_ts)),
        read_at: Some(micros_to_iso(read_ts)),
    };

    tracing::debug!(
        "Acknowledged message {} for {} in project {}",
        message_id,
        agent_name,
        project_key
    );

    serde_json::to_string(&response)
        .map_err(|e| McpError::new(McpErrorCode::InternalError, format!("JSON error: {e}")))
}
