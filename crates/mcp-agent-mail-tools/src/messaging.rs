//! Messaging cluster tools
//!
//! Tools for message sending and inbox management:
//! - `send_message`: Send a message to recipients
//! - `reply_message`: Reply to an existing message
//! - `fetch_inbox`: Retrieve inbox messages
//! - `mark_message_read`: Mark message as read
//! - `acknowledge_message`: Acknowledge a message

use asupersync::Outcome;
use fastmcp::McpErrorCode;
use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbError, micros_to_iso};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::pattern_overlap::CompiledPattern;
use crate::tool_util::{
    db_error_to_mcp_error, db_outcome_to_mcp_result, get_db_pool, resolve_agent, resolve_project,
};

/// Write a message bundle to the git archive (best-effort, non-blocking).
/// Failures are logged but never fail the tool call.
///
/// Uses the write-behind queue when available; falls back to synchronous
/// write if the queue is full.
fn try_write_message_archive(
    config: &Config,
    project_slug: &str,
    message_json: &serde_json::Value,
    body_md: &str,
    sender: &str,
    all_recipient_names: &[String],
) {
    let op = mcp_agent_mail_storage::WriteOp::MessageBundle {
        project_slug: project_slug.to_string(),
        config: config.clone(),
        message_json: message_json.clone(),
        body_md: body_md.to_string(),
        sender: sender.to_string(),
        recipients: all_recipient_names.to_vec(),
    };
    if !mcp_agent_mail_storage::wbq_enqueue(op) {
        // Fallback: synchronous write
        match mcp_agent_mail_storage::ensure_archive(config, project_slug) {
            Ok(archive) => {
                if let Err(e) = mcp_agent_mail_storage::write_message_bundle(
                    &archive,
                    config,
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
}

async fn resolve_or_register_agent(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::DbPool,
    project_id: i64,
    agent_name: &str,
    sender: &mcp_agent_mail_db::AgentRow,
    config: &Config,
) -> McpResult<mcp_agent_mail_db::AgentRow> {
    match mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, agent_name).await {
        Outcome::Ok(agent) => Ok(agent),
        Outcome::Err(DbError::NotFound { .. }) if config.messaging_auto_register_recipients => {
            let _ = db_outcome_to_mcp_result(
                mcp_agent_mail_db::queries::register_agent(
                    ctx.cx(),
                    pool,
                    project_id,
                    agent_name,
                    &sender.program,
                    &sender.model,
                    Some(sender.task_description.as_str()),
                    Some(sender.attachments_policy.as_str()),
                )
                .await,
            )?;
            db_outcome_to_mcp_result(
                mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, agent_name).await,
            )
        }
        Outcome::Err(e) => Err(db_error_to_mcp_error(e)),
        Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
        Outcome::Panicked(p) => Err(McpError::internal_error(format!(
            "Internal panic: {}",
            p.message()
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn push_recipient(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::DbPool,
    project_id: i64,
    name: &str,
    kind: &str,
    sender: &mcp_agent_mail_db::AgentRow,
    config: &Config,
    recipient_map: &mut HashMap<String, mcp_agent_mail_db::AgentRow>,
    all_recipients: &mut Vec<(i64, String)>,
    resolved_list: &mut Vec<String>,
) -> McpResult<()> {
    let name_key = name.to_lowercase();
    let agent = if let Some(existing) = recipient_map.get(&name_key) {
        existing.clone()
    } else {
        let agent = resolve_or_register_agent(ctx, pool, project_id, name, sender, config).await?;
        let key = agent.name.to_lowercase();
        recipient_map.insert(key, agent.clone());
        agent
    };
    let agent_id = agent.id.unwrap_or(0);
    all_recipients.push((agent_id, kind.to_string()));
    resolved_list.push(agent.name);
    Ok(())
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

    // Truncate subject at 200 chars (parity with Python legacy)
    let subject = if subject.len() > 200 {
        tracing::warn!(
            "Subject exceeds 200 characters ({}); truncating",
            subject.len()
        );
        subject[..200].to_string()
    } else {
        subject
    };

    // Validate importance
    let importance_val = importance.unwrap_or_else(|| "normal".to_string());
    if !["low", "normal", "high", "urgent"].contains(&importance_val.as_str()) {
        return Err(McpError::new(
            McpErrorCode::InvalidParams,
            format!("Invalid importance '{importance_val}'. Must be: low, normal, high, or urgent"),
        ));
    }

    let config = Config::from_env();

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Resolve sender
    let sender = resolve_agent(ctx, &pool, project_id, &sender_name).await?;
    let sender_id = sender.id.unwrap_or(0);

    // Resolve all recipients (to, cc, bcc) with optional auto-registration
    let cc_list = cc.unwrap_or_default();
    let bcc_list = bcc.unwrap_or_default();

    let mut all_recipients: Vec<(i64, String)> = Vec::new();
    let mut resolved_to: Vec<String> = Vec::new();
    let mut resolved_cc_recipients: Vec<String> = Vec::new();
    let mut resolved_bcc_recipients: Vec<String> = Vec::new();
    let mut recipient_map: HashMap<String, mcp_agent_mail_db::AgentRow> = HashMap::new();

    for name in &to {
        push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "to",
            &sender,
            &config,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_to,
        )
        .await?;
    }
    for name in &cc_list {
        push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "cc",
            &sender,
            &config,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_cc_recipients,
        )
        .await?;
    }
    for name in &bcc_list {
        push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "bcc",
            &sender,
            &config,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_bcc_recipients,
        )
        .await?;
    }

    // Determine attachment processing settings
    let embed_policy =
        mcp_agent_mail_storage::EmbedPolicy::from_str_policy(&sender.attachments_policy);
    let sender_forces_convert = matches!(
        embed_policy,
        mcp_agent_mail_storage::EmbedPolicy::Inline | mcp_agent_mail_storage::EmbedPolicy::File
    );
    let do_convert = if sender_forces_convert {
        true
    } else {
        convert_images.unwrap_or(config.convert_images)
    };

    // Process attachments and markdown images
    let mut final_body = body_md.clone();
    let mut all_attachment_meta: Vec<serde_json::Value> = Vec::new();

    if do_convert {
        let slug = &project.slug;
        let archive = mcp_agent_mail_storage::ensure_archive(&config, slug);
        if let Ok(archive) = archive {
            // Process inline markdown images
            if let Ok((updated_body, md_meta, _rel_paths)) =
                mcp_agent_mail_storage::process_markdown_images(
                    &archive,
                    &config,
                    &body_md,
                    embed_policy,
                )
            {
                final_body = updated_body;
                for m in &md_meta {
                    if let Ok(v) = serde_json::to_value(m) {
                        all_attachment_meta.push(v);
                    }
                }
            }

            // Process explicit attachment_paths
            if let Some(ref paths) = attachment_paths {
                if !paths.is_empty() {
                    if let Ok((att_meta, _rel_paths)) = mcp_agent_mail_storage::process_attachments(
                        &archive,
                        &config,
                        paths,
                        embed_policy,
                    ) {
                        for m in &att_meta {
                            if let Ok(v) = serde_json::to_value(m) {
                                all_attachment_meta.push(v);
                            }
                        }
                    }
                }
            }
        }
    } else if let Some(ref paths) = attachment_paths {
        // No conversion: store raw path references
        for p in paths {
            all_attachment_meta.push(serde_json::json!({
                "type": "file",
                "path": p,
                "media_type": "application/octet-stream",
            }));
        }
    }

    if let Some(auto_contact) = auto_contact_if_blocked {
        tracing::debug!("Auto contact if blocked: {}", auto_contact);
    }

    // Enforce contact policies (best-effort parity with legacy)
    if config.contact_enforcement_enabled {
        let mut auto_ok_names: HashSet<String> = HashSet::new();

        if let Some(thread) = thread_id.as_deref() {
            let thread = thread.trim();
            if !thread.is_empty() {
                let thread_rows = db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::list_thread_messages(
                        ctx.cx(),
                        &pool,
                        project_id,
                        thread,
                        Some(500),
                    )
                    .await,
                )
                .unwrap_or_default();
                let mut message_ids: Vec<i64> = Vec::new();
                for row in &thread_rows {
                    auto_ok_names.insert(row.from.clone());
                    message_ids.push(row.id);
                }
                let recipients = db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::list_message_recipient_names_for_messages(
                        ctx.cx(),
                        &pool,
                        project_id,
                        &message_ids,
                    )
                    .await,
                )
                .unwrap_or_default();
                for name in recipients {
                    auto_ok_names.insert(name);
                }
            }
        }

        // Allow if sender and recipient share overlapping active file reservations.
        let reservations = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::get_active_reservations(ctx.cx(), &pool, project_id).await,
        )
        .unwrap_or_default();
        let mut patterns_by_agent: HashMap<i64, Vec<CompiledPattern>> = HashMap::new();
        for res in reservations {
            patterns_by_agent
                .entry(res.agent_id)
                .or_default()
                .push(CompiledPattern::new(&res.path_pattern));
        }
        if let Some(sender_patterns) = patterns_by_agent.get(&sender_id) {
            for agent in recipient_map.values() {
                if let Some(rec_id) = agent.id {
                    if let Some(rec_patterns) = patterns_by_agent.get(&rec_id) {
                        let overlaps = sender_patterns
                            .iter()
                            .any(|a| rec_patterns.iter().any(|b| a.overlaps(b)));
                        if overlaps {
                            auto_ok_names.insert(agent.name.clone());
                        }
                    }
                }
            }
        }

        let now_micros = mcp_agent_mail_db::now_micros();
        let ttl_seconds = i64::try_from(config.contact_auto_ttl_seconds).unwrap_or(i64::MAX);
        let ttl_micros = ttl_seconds.saturating_mul(1_000_000);
        let since_ts = now_micros.saturating_sub(ttl_micros);

        let mut candidate_ids: Vec<i64> = recipient_map
            .values()
            .filter_map(|agent| agent.id)
            .filter(|id| *id != sender_id)
            .collect();
        candidate_ids.sort_unstable();
        candidate_ids.dedup();

        let recent_ids = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::list_recent_contact_agent_ids(
                ctx.cx(),
                &pool,
                project_id,
                sender_id,
                &candidate_ids,
                since_ts,
            )
            .await,
        )
        .unwrap_or_default();
        let recent_set: HashSet<i64> = recent_ids.into_iter().collect();

        let approved_ids = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::list_approved_contact_ids(
                ctx.cx(),
                &pool,
                project_id,
                sender_id,
                &candidate_ids,
            )
            .await,
        )
        .unwrap_or_default();
        let approved_set: HashSet<i64> = approved_ids.into_iter().collect();

        let mut blocked: Vec<String> = Vec::new();
        for agent in recipient_map.values() {
            if agent.name == sender.name {
                continue;
            }
            if auto_ok_names.contains(&agent.name) {
                continue;
            }
            let rec_id = agent.id.unwrap_or(0);
            let mut policy = agent.contact_policy.to_lowercase();
            if !["open", "auto", "contacts_only", "block_all"].contains(&policy.as_str()) {
                policy = "auto".to_string();
            }
            if policy == "open" {
                continue;
            }
            if policy == "block_all" {
                return Err(McpError::new(
                    McpErrorCode::InvalidParams,
                    "CONTACT_BLOCKED: Recipient is not accepting messages.",
                ));
            }
            let approved = approved_set.contains(&rec_id);
            let recent = recent_set.contains(&rec_id);
            if policy == "auto" {
                if approved || recent {
                    continue;
                }
            } else if policy == "contacts_only" && approved {
                continue;
            }
            blocked.push(agent.name.clone());
        }

        if !blocked.is_empty() {
            let effective_auto_contact =
                auto_contact_if_blocked.unwrap_or(config.messaging_auto_handshake_on_block);
            if effective_auto_contact {
                for name in &blocked {
                    let _ = Box::pin(crate::macros::macro_contact_handshake(
                        ctx,
                        project.human_key.clone(),
                        Some(sender.name.clone()),
                        Some(name.clone()),
                        None,
                        None,
                        None,
                        Some("auto-handshake by send_message".to_string()),
                        Some(true),
                        Some(ttl_seconds),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    ))
                    .await;
                }

                let approved_ids = db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::list_approved_contact_ids(
                        ctx.cx(),
                        &pool,
                        project_id,
                        sender_id,
                        &candidate_ids,
                    )
                    .await,
                )
                .unwrap_or_default();
                let approved_set: HashSet<i64> = approved_ids.into_iter().collect();

                blocked.retain(|name| {
                    if let Some(agent) = recipient_map.get(&name.to_lowercase()) {
                        let rec_id = agent.id.unwrap_or(0);
                        let mut policy = agent.contact_policy.to_lowercase();
                        if !["open", "auto", "contacts_only", "block_all"]
                            .contains(&policy.as_str())
                        {
                            policy = "auto".to_string();
                        }
                        let approved = approved_set.contains(&rec_id);
                        if policy == "open" {
                            return false;
                        }
                        if policy == "auto" && approved {
                            return false;
                        }
                        if policy == "contacts_only" && approved {
                            return false;
                        }
                    }
                    true
                });
            }
        }

        if !blocked.is_empty() {
            return Err(McpError::new(
                McpErrorCode::InvalidParams,
                format!(
                    "CONTACT_BLOCKED: Missing contact approval for {}",
                    blocked.join(", ")
                ),
            ));
        }
    }

    // Serialize processed attachment metadata as JSON array
    let attachments_json =
        serde_json::to_string(&all_attachment_meta).unwrap_or_else(|_| "[]".to_string());

    // Create message in DB (use processed body with inline replacements)
    let message = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::create_message(
            ctx.cx(),
            &pool,
            project_id,
            sender_id,
            &subject,
            &final_body,
            thread_id.as_deref(),
            &importance_val,
            ack_required.unwrap_or(false),
            &attachments_json,
        )
        .await,
    )?;

    let message_id = message.id.unwrap_or(0);

    // Add recipients
    let recipient_refs: Vec<(i64, &str)> = all_recipients
        .iter()
        .map(|(id, kind)| (*id, kind.as_str()))
        .collect();
    db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::add_recipients(ctx.cx(), &pool, message_id, &recipient_refs)
            .await,
    )?;

    // Emit notification signals for to/cc recipients only (never bcc).
    let notification_meta = mcp_agent_mail_storage::NotificationMessage {
        id: Some(message_id),
        from: Some(sender_name.clone()),
        subject: Some(message.subject.clone()),
        importance: Some(message.importance.clone()),
    };
    let mut notified = HashSet::new();
    for name in resolved_to.iter().chain(resolved_cc_recipients.iter()) {
        if notified.insert(name.clone()) {
            let op = mcp_agent_mail_storage::WriteOp::NotificationSignal {
                config: config.clone(),
                project_slug: project.slug.clone(),
                agent_name: name.clone(),
                metadata: Some(notification_meta.clone()),
            };
            if !mcp_agent_mail_storage::wbq_enqueue(op) {
                let _ = mcp_agent_mail_storage::emit_notification_signal(
                    &config,
                    &project.slug,
                    name,
                    Some(&notification_meta),
                );
            }
        }
    }

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
            "attachments": &all_attachment_meta,
        });
        try_write_message_archive(
            &config,
            &project.slug,
            &msg_json,
            &message.body_md,
            &sender_name,
            &all_recipient_names,
        );
    }

    // Extract path strings from processed metadata for response format
    let attachment_paths_out: Vec<String> = all_attachment_meta
        .iter()
        .filter_map(|m| m.get("path").and_then(|p| p.as_str()).map(str::to_string))
        .collect();

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
        attachments: attachment_paths_out.clone(),
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
        attachments: attachment_paths_out,
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
    let config = Config::from_env();

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

    // Apply subject prefix if not already present (case-insensitive)
    let subject = if original
        .subject
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
    {
        original.subject.clone()
    } else {
        format!("{prefix} {}", original.subject)
    };
    // Truncate subject at 200 chars (parity with Python legacy)
    let subject = if subject.len() > 200 {
        tracing::warn!(
            "Reply subject exceeds 200 characters ({}); truncating",
            subject.len()
        );
        subject[..200].to_string()
    } else {
        subject
    };

    // Default to to original sender if not specified
    let to_names = to.unwrap_or_else(|| vec![original_sender.name.clone()]);
    let cc_names = cc.unwrap_or_default();
    let bcc_names = bcc.unwrap_or_default();

    // Resolve all recipients
    let mut all_recipients: Vec<(i64, String)> = Vec::new();
    let mut resolved_to: Vec<String> = Vec::new();
    let mut resolved_cc_recipients: Vec<String> = Vec::new();
    let mut resolved_bcc_recipients: Vec<String> = Vec::new();

    for name in &to_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "to".to_string()));
        resolved_to.push(agent.name);
    }
    for name in &cc_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "cc".to_string()));
        resolved_cc_recipients.push(agent.name);
    }
    for name in &bcc_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        all_recipients.push((agent.id.unwrap_or(0), "bcc".to_string()));
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
    let recipient_refs: Vec<(i64, &str)> = all_recipients
        .iter()
        .map(|(id, kind)| (*id, kind.as_str()))
        .collect();
    db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::add_recipients(ctx.cx(), &pool, reply_id, &recipient_refs)
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
            &config,
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

    // Clear notification signal (best-effort).
    let config = Config::from_env();
    let _ = mcp_agent_mail_storage::clear_notification_signal(&config, &project.slug, &agent.name);

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
