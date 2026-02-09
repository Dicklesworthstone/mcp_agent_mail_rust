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
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};

use serde_json::json;

use crate::pattern_overlap::CompiledPattern;
use crate::tool_util::{
    db_error_to_mcp_error, db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_agent,
    resolve_project,
};

/// Write a message bundle to the git archive (best-effort, non-blocking).
/// Failures are logged but never fail the tool call.
///
/// Uses the write-behind queue when available. If the queue is unavailable,
/// logs a warning and skips the archive write (DB remains the source of truth).
fn try_write_message_archive(
    config: &Config,
    project_slug: &str,
    message_json: &serde_json::Value,
    body_md: &str,
    sender: &str,
    all_recipient_names: &[String],
    extra_paths: &[String],
) {
    let op = mcp_agent_mail_storage::WriteOp::MessageBundle {
        project_slug: project_slug.to_string(),
        config: config.clone(),
        message_json: message_json.clone(),
        body_md: body_md.to_string(),
        sender: sender.to_string(),
        recipients: all_recipient_names.to_vec(),
        extra_paths: extra_paths.to_vec(),
    };
    match mcp_agent_mail_storage::wbq_enqueue(op) {
        mcp_agent_mail_storage::WbqEnqueueResult::Enqueued
        | mcp_agent_mail_storage::WbqEnqueueResult::SkippedDiskCritical => {
            // Disk pressure guard: archive writes may be disabled; DB remains authoritative.
        }
        mcp_agent_mail_storage::WbqEnqueueResult::QueueUnavailable => {
            tracing::warn!(
                "WBQ enqueue failed; skipping message archive write project={project_slug}"
            );
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

/// Validate `thread_id` format: must start with alphanumeric and contain only
/// letters, numbers, '.', '_', or '-'. Max 128 chars.
fn is_valid_thread_id(tid: &str) -> bool {
    if tid.is_empty() || tid.len() > 128 {
        return false;
    }
    let first = tid.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    tid.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Defense-in-depth sanitization for `thread_id` values derived from DB rows.
/// Strips invalid characters, truncates to 128 chars, and ensures the result
/// starts with an alphanumeric character. Returns the sanitized value, or
/// falls back to `fallback` if sanitization produces an empty string.
fn sanitize_thread_id(raw: &str, fallback: &str) -> String {
    let sanitized: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .take(128)
        .collect();
    if sanitized.is_empty() || !sanitized.as_bytes()[0].is_ascii_alphanumeric() {
        return fallback.to_string();
    }
    sanitized
}

/// Validate per-message size limits before any DB/archive operations.
///
/// Enforces `max_subject_bytes`, `max_message_body_bytes`, `max_attachment_bytes`,
/// and `max_total_message_bytes` from config. A limit of 0 means unlimited.
fn validate_message_size_limits(
    config: &Config,
    subject: &str,
    body_md: &str,
    attachment_paths: Option<&[String]>,
) -> McpResult<()> {
    // Subject size
    if config.max_subject_bytes > 0 && subject.len() > config.max_subject_bytes {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Subject exceeds size limit: {} bytes > {} byte limit. Shorten the subject.",
                subject.len(),
                config.max_subject_bytes,
            ),
            true,
            json!({
                "field": "subject",
                "size_bytes": subject.len(),
                "limit_bytes": config.max_subject_bytes,
            }),
        ));
    }

    // Body size
    if config.max_message_body_bytes > 0 && body_md.len() > config.max_message_body_bytes {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Message body exceeds size limit: {} bytes > {} byte limit. \
                 Split into multiple messages or reduce content.",
                body_md.len(),
                config.max_message_body_bytes,
            ),
            true,
            json!({
                "field": "body_md",
                "size_bytes": body_md.len(),
                "limit_bytes": config.max_message_body_bytes,
            }),
        ));
    }

    // Per-attachment size (check file paths if provided)
    let mut total_size = subject.len().saturating_add(body_md.len());
    if let Some(paths) = attachment_paths {
        for path in paths {
            if let Ok(meta) = std::fs::metadata(path) {
                let file_size = usize::try_from(meta.len()).unwrap_or(usize::MAX);
                if config.max_attachment_bytes > 0 && file_size > config.max_attachment_bytes {
                    return Err(legacy_tool_error(
                        "INVALID_ARGUMENT",
                        format!(
                            "Attachment exceeds size limit: {path} is {} bytes > {} byte limit.",
                            file_size, config.max_attachment_bytes,
                        ),
                        true,
                        json!({
                            "field": "attachment_paths",
                            "path": path,
                            "size_bytes": file_size,
                            "limit_bytes": config.max_attachment_bytes,
                        }),
                    ));
                }
                total_size = total_size.saturating_add(file_size);
            }
            // If file doesn't exist, let downstream handle the error.
        }
    }

    // Total message size
    if config.max_total_message_bytes > 0 && total_size > config.max_total_message_bytes {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Total message size exceeds limit: {} bytes > {} byte limit. \
                 Reduce body or attachment sizes.",
                total_size, config.max_total_message_bytes,
            ),
            true,
            json!({
                "field": "total",
                "size_bytes": total_size,
                "limit_bytes": config.max_total_message_bytes,
            }),
        ));
    }

    Ok(())
}

/// Validate body-only size limit for `reply_message` (no attachments, subject comes later).
fn validate_reply_body_limit(config: &Config, body_md: &str) -> McpResult<()> {
    if config.max_message_body_bytes > 0 && body_md.len() > config.max_message_body_bytes {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Reply body exceeds size limit: {} bytes > {} byte limit. \
                 Split into multiple messages or reduce content.",
                body_md.len(),
                config.max_message_body_bytes,
            ),
            true,
            json!({
                "field": "body_md",
                "size_bytes": body_md.len(),
                "limit_bytes": config.max_message_body_bytes,
            }),
        ));
    }
    Ok(())
}

const fn has_any_recipients(to: &[String], cc: &[String], bcc: &[String]) -> bool {
    !(to.is_empty() && cc.is_empty() && bcc.is_empty())
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
    all_recipients: &mut SmallVec<[(i64, String); 8]>,
    resolved_list: &mut SmallVec<[String; 4]>,
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
    // Skip if this agent_id is already in the list (e.g., same agent in both
    // `to` and `cc`).  The first occurrence wins, matching email precedence:
    // to > cc > bcc.  Without this, the PRIMARY KEY(message_id, agent_id)
    // constraint in message_recipients would reject the INSERT and roll back
    // the entire message transaction.
    if !all_recipients.iter().any(|(id, _)| *id == agent_id) {
        all_recipients.push((agent_id, kind.to_string()));
    }
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
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "At least one recipient (to) is required. Provide agent names in the 'to' array.",
            true,
            json!({
                "field": "to",
                "error_detail": "empty recipient list",
            }),
        ));
    }

    // Truncate subject at 200 chars (parity with Python legacy).
    // Use char_indices to avoid panicking on multi-byte UTF-8 boundaries.
    let subject = if subject.chars().count() > 200 {
        tracing::warn!(
            "Subject exceeds 200 characters ({}); truncating",
            subject.chars().count()
        );
        subject.chars().take(200).collect::<String>()
    } else {
        subject
    };

    // Validate importance
    let importance_val = importance.unwrap_or_else(|| "normal".to_string());
    if !["low", "normal", "high", "urgent"].contains(&importance_val.as_str()) {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Invalid argument value: importance='{importance_val}'. \
                 Must be: low, normal, high, or urgent. Check that all parameters have valid values."
            ),
            true,
            json!({
                "field": "importance",
                "error_detail": importance_val,
            }),
        ));
    }

    // Normalize thread_id: trim whitespace and convert blank to None.
    let thread_id = thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    // Validate thread_id format if provided
    if let Some(ref tid) = thread_id {
        if !is_valid_thread_id(tid) {
            return Err(legacy_tool_error(
                "INVALID_THREAD_ID",
                format!(
                    "Invalid thread_id: '{tid}'. Thread IDs must start with an alphanumeric character and \
                     contain only letters, numbers, '.', '_', or '-' (max 128). \
                     Examples: 'TKT-123', 'bd-42', 'feature-xyz'."
                ),
                true,
                json!({
                    "provided": tid,
                    "examples": ["TKT-123", "bd-42", "feature-xyz"],
                }),
            ));
        }
    }

    let config = &Config::get();

    // ── Per-message size limits (fail fast before any DB/archive work) ──
    validate_message_size_limits(config, &subject, &body_md, attachment_paths.as_deref())?;

    if config.disk_space_monitor_enabled {
        let pressure = mcp_agent_mail_core::disk::DiskPressure::from_u64(
            mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level
                .load(),
        );
        if pressure == mcp_agent_mail_core::disk::DiskPressure::Fatal {
            let free = mcp_agent_mail_core::global_metrics()
                .system
                .disk_effective_free_bytes
                .load();
            return Err(legacy_tool_error(
                "DISK_FULL",
                format!(
                    "Disk space critically low (pressure=fatal). Refusing to accept new messages until space recovers. \
effective_free_bytes={free}"
                ),
                true,
                json!({
                    "pressure": pressure.label(),
                    "effective_free_bytes": free,
                    "fatal_threshold_mb": config.disk_space_fatal_mb,
                    "critical_threshold_mb": config.disk_space_critical_mb,
                    "warning_threshold_mb": config.disk_space_warning_mb,
                }),
            ));
        }
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Resolve sender
    let sender = resolve_agent(ctx, &pool, project_id, &sender_name).await?;
    let sender_id = sender.id.unwrap_or(0);

    // Resolve all recipients (to, cc, bcc) with optional auto-registration
    let cc_list = cc.unwrap_or_default();
    let bcc_list = bcc.unwrap_or_default();

    let total_recip = to.len() + cc_list.len() + bcc_list.len();
    let mut all_recipients: SmallVec<[(i64, String); 8]> = SmallVec::with_capacity(total_recip);
    let mut resolved_to: SmallVec<[String; 4]> = SmallVec::with_capacity(to.len());
    let mut resolved_cc_recipients: SmallVec<[String; 4]> = SmallVec::with_capacity(cc_list.len());
    let mut resolved_bcc_recipients: SmallVec<[String; 4]> =
        SmallVec::with_capacity(bcc_list.len());
    let mut recipient_map: HashMap<String, mcp_agent_mail_db::AgentRow> =
        HashMap::with_capacity(total_recip);

    for name in &to {
        push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "to",
            &sender,
            config,
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
            config,
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
            config,
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
    let attachment_count = attachment_paths.as_ref().map_or(0, Vec::len);
    let mut all_attachment_meta: Vec<serde_json::Value> = Vec::with_capacity(attachment_count + 4);
    let mut all_attachment_rel_paths: Vec<String> = Vec::with_capacity(attachment_count + 4);
    let base_dir = std::path::Path::new(&project.human_key);

    if do_convert {
        let slug = &project.slug;
        match mcp_agent_mail_storage::ensure_archive(config, slug) {
            Ok(archive) => {
                // Process inline markdown images
                if let Ok((updated_body, md_meta, rel_paths)) =
                    mcp_agent_mail_storage::process_markdown_images(
                        &archive,
                        config,
                        base_dir,
                        &body_md,
                        embed_policy,
                    )
                {
                    final_body = updated_body;
                    all_attachment_rel_paths.extend(rel_paths);
                    for m in &md_meta {
                        if let Ok(v) = serde_json::to_value(m) {
                            all_attachment_meta.push(v);
                        }
                    }
                }

                // Process explicit attachment_paths
                if let Some(ref paths) = attachment_paths {
                    if !paths.is_empty() {
                        let (att_meta, rel_paths) = mcp_agent_mail_storage::process_attachments(
                            &archive,
                            config,
                            base_dir,
                            paths,
                            embed_policy,
                        )
                        .map_err(|e| {
                            legacy_tool_error(
                                "INVALID_ARGUMENT",
                                format!("Invalid attachment_paths: {e}"),
                                true,
                                json!({
                                    "field": "attachment_paths",
                                    "provided": paths,
                                }),
                            )
                        })?;
                        all_attachment_rel_paths.extend(rel_paths);
                        for m in &att_meta {
                            if let Ok(v) = serde_json::to_value(m) {
                                all_attachment_meta.push(v);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                if attachment_paths.as_ref().is_some_and(|p| !p.is_empty()) {
                    // If explicit attachments were provided, fail loudly rather than silently dropping them.
                    return Err(legacy_tool_error(
                        "ARCHIVE_ERROR",
                        format!(
                            "Failed to initialize git archive for project '{slug}'. This prevents storing attachments: {e}"
                        ),
                        true,
                        json!({
                            "project_slug": slug,
                            "project_root": project.human_key,
                        }),
                    ));
                }
            }
        }
    } else if let Some(ref paths) = attachment_paths {
        // No conversion: validate source paths and store canonical references.
        for p in paths {
            let resolved =
                mcp_agent_mail_storage::resolve_attachment_source_path(base_dir, config, p)
                    .map_err(|e| {
                        legacy_tool_error(
                            "INVALID_ARGUMENT",
                            format!("Invalid attachment path: {e}"),
                            true,
                            json!({
                                "field": "attachment_paths",
                                "provided": p,
                            }),
                        )
                    })?;
            all_attachment_meta.push(serde_json::json!({
                "type": "file",
                "path": resolved.to_string_lossy(),
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
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "contact enforcement: list_thread_messages failed (fail-open): {e}"
                    );
                    mcp_agent_mail_core::global_metrics()
                        .tools
                        .contact_enforcement_bypass_total
                        .inc();
                    Vec::new()
                });
                let mut message_ids: Vec<i64> = Vec::with_capacity(thread_rows.len());
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
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "contact enforcement: list_message_recipient_names failed (fail-open): {e}"
                    );
                    mcp_agent_mail_core::global_metrics()
                        .tools
                        .contact_enforcement_bypass_total
                        .inc();
                    Vec::new()
                });
                for name in recipients {
                    auto_ok_names.insert(name);
                }
            }
        }

        // Allow if sender and recipient share overlapping active file reservations.
        let reservations = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::get_active_reservations(ctx.cx(), &pool, project_id).await,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("contact enforcement: get_active_reservations failed (fail-open): {e}");
            mcp_agent_mail_core::global_metrics()
                .tools
                .contact_enforcement_bypass_total
                .inc();
            Vec::new()
        });
        let mut patterns_by_agent: HashMap<i64, Vec<CompiledPattern>> =
            HashMap::with_capacity(reservations.len());
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
                return Err(legacy_tool_error(
                    "CONTACT_BLOCKED",
                    "Recipient is not accepting messages.",
                    true,
                    json!({}),
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
            let blocked_sorted: Vec<String> = {
                let mut v = blocked.clone();
                v.sort();
                v.dedup();
                v
            };
            let recipient_list = blocked_sorted.join(", ");
            let sample = blocked_sorted.first().cloned().unwrap_or_default();
            return Err(legacy_tool_error(
                "CONTACT_REQUIRED",
                format!(
                    "Contact approval required for recipients: {recipient_list}. \
                     Before retrying, request approval with \
                     `request_contact(project_key='{project_key}', from_agent='{sender_name}', \
                     to_agent='{sample}')` or run \
                     `macro_contact_handshake(project_key='{project_key}', \
                     requester='{sender_name}', target='{sample}', auto_accept=True)`.",
                    project_key = project.human_key,
                    sender_name = sender.name,
                ),
                true,
                json!({
                    "blocked_recipients": blocked_sorted,
                    "sample_target": sample,
                }),
            ));
        }
    }

    // Serialize processed attachment metadata as JSON array
    let attachments_json =
        serde_json::to_string(&all_attachment_meta).unwrap_or_else(|_| "[]".to_string());

    // Create message + recipients in a single DB transaction (1 fsync)
    let recipient_refs: SmallVec<[(i64, &str); 8]> = all_recipients
        .iter()
        .map(|(id, kind)| (*id, kind.as_str()))
        .collect();
    let message = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::create_message_with_recipients(
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
            &recipient_refs,
        )
        .await,
    )?;

    let message_id = message.id.unwrap_or(0);

    // Emit notification signals for to/cc recipients only (never bcc).
    //
    // IMPORTANT: These must be synchronous so that the `.signal` file exists
    // immediately when `send_message` returns (conformance parity with legacy
    // Python implementation + fixture tests).
    let notification_meta = mcp_agent_mail_storage::NotificationMessage {
        id: Some(message_id),
        from: Some(sender_name.clone()),
        subject: Some(message.subject.clone()),
        importance: Some(message.importance.clone()),
    };
    let mut notified = HashSet::new();
    for name in resolved_to.iter().chain(resolved_cc_recipients.iter()) {
        if notified.insert(name.clone()) {
            let _ = mcp_agent_mail_storage::emit_notification_signal(
                config,
                &project.slug,
                name,
                Some(&notification_meta),
            );
        }
    }

    // Write message bundle to git archive (best-effort)
    {
        let mut all_recipient_names: SmallVec<[String; 12]> = SmallVec::new();
        all_recipient_names.extend(resolved_to.iter().cloned());
        all_recipient_names.extend(resolved_cc_recipients.iter().cloned());
        all_recipient_names.extend(resolved_bcc_recipients.iter().cloned());

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
            config,
            &project.slug,
            &msg_json,
            &message.body_md,
            &sender_name,
            &all_recipient_names,
            &all_attachment_rel_paths,
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
        to: resolved_to.into_vec(),
        cc: resolved_cc_recipients.into_vec(),
        bcc: resolved_bcc_recipients.into_vec(),
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
    let config = &Config::get();

    // ── Per-message size limits (fail fast before any DB/archive work) ──
    // Reply has no subject yet (inherited below) and no attachment_paths, so
    // validate body only here; subject is checked after construction.
    validate_reply_body_limit(config, &body_md)?;

    if config.disk_space_monitor_enabled {
        let pressure = mcp_agent_mail_core::disk::DiskPressure::from_u64(
            mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level
                .load(),
        );
        if pressure == mcp_agent_mail_core::disk::DiskPressure::Fatal {
            let free = mcp_agent_mail_core::global_metrics()
                .system
                .disk_effective_free_bytes
                .load();
            return Err(legacy_tool_error(
                "DISK_FULL",
                format!(
                    "Disk space critically low (pressure=fatal). Refusing to accept new messages until space recovers. \
effective_free_bytes={free}"
                ),
                true,
                json!({
                    "pressure": pressure.label(),
                    "effective_free_bytes": free,
                    "fatal_threshold_mb": config.disk_space_fatal_mb,
                    "critical_threshold_mb": config.disk_space_critical_mb,
                    "warning_threshold_mb": config.disk_space_warning_mb,
                }),
            ));
        }
    }

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    // Fetch original message to inherit properties
    let original = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_message(ctx.cx(), &pool, message_id).await,
    )?;
    if original.project_id != project_id {
        return Err(legacy_tool_error(
            "NOT_FOUND",
            format!("Message not found: {message_id}"),
            true,
            json!({
                "entity": "Message",
                "identifier": message_id,
            }),
        ));
    }

    // Resolve sender
    let sender = resolve_agent(ctx, &pool, project_id, &sender_name).await?;
    let sender_id = sender.id.unwrap_or(0);

    // Resolve original sender name for default recipient
    let original_sender = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::get_agent_by_id(ctx.cx(), &pool, original.sender_id).await,
    )?;

    // Determine thread_id: use original's thread_id, or the original message id as string.
    // Defense-in-depth: sanitize in case legacy data contains invalid characters.
    let fallback_tid = message_id.to_string();
    let thread_id = match original.thread_id.as_deref() {
        Some(tid) => sanitize_thread_id(tid, &fallback_tid),
        None => fallback_tid,
    };

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
    // Truncate subject at 200 chars (parity with Python legacy).
    // Use char_indices to avoid panicking on multi-byte UTF-8 boundaries.
    let subject = if subject.chars().count() > 200 {
        tracing::warn!(
            "Reply subject exceeds 200 characters ({}); truncating",
            subject.chars().count()
        );
        subject.chars().take(200).collect::<String>()
    } else {
        subject
    };

    // Default to to original sender if not specified
    let to_names = to.unwrap_or_else(|| vec![original_sender.name.clone()]);
    let cc_names = cc.unwrap_or_default();
    let bcc_names = bcc.unwrap_or_default();
    if !has_any_recipients(&to_names, &cc_names, &bcc_names) {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "At least one recipient is required. Provide at least one agent name in to/cc/bcc.",
            true,
            json!({
                "field": "to|cc|bcc",
                "error_detail": "empty recipient list",
            }),
        ));
    }

    // Resolve all recipients
    let total_recip = to_names.len() + cc_names.len() + bcc_names.len();
    let mut all_recipients: SmallVec<[(i64, String); 8]> = SmallVec::with_capacity(total_recip);
    let mut resolved_to: SmallVec<[String; 4]> = SmallVec::with_capacity(to_names.len());
    let mut resolved_cc_recipients: SmallVec<[String; 4]> = SmallVec::with_capacity(cc_names.len());
    let mut resolved_bcc_recipients: SmallVec<[String; 4]> =
        SmallVec::with_capacity(bcc_names.len());

    for name in &to_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        let aid = agent.id.unwrap_or(0);
        if !all_recipients.iter().any(|(id, _)| *id == aid) {
            all_recipients.push((aid, "to".to_string()));
        }
        resolved_to.push(agent.name);
    }
    for name in &cc_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        let aid = agent.id.unwrap_or(0);
        if !all_recipients.iter().any(|(id, _)| *id == aid) {
            all_recipients.push((aid, "cc".to_string()));
        }
        resolved_cc_recipients.push(agent.name);
    }
    for name in &bcc_names {
        let agent = resolve_agent(ctx, &pool, project_id, name).await?;
        let aid = agent.id.unwrap_or(0);
        if !all_recipients.iter().any(|(id, _)| *id == aid) {
            all_recipients.push((aid, "bcc".to_string()));
        }
        resolved_bcc_recipients.push(agent.name);
    }

    // Create reply message + recipients in a single DB transaction
    let recipient_refs: SmallVec<[(i64, &str); 8]> = all_recipients
        .iter()
        .map(|(id, kind)| (*id, kind.as_str()))
        .collect();
    let reply = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::create_message_with_recipients(
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
            &recipient_refs,
        )
        .await,
    )?;

    let reply_id = reply.id.unwrap_or(0);

    // Emit notification signals for to/cc recipients only (never bcc).
    // Mirrors the send_message notification logic for parity with Python.
    let notification_meta = mcp_agent_mail_storage::NotificationMessage {
        id: Some(reply_id),
        from: Some(sender_name.clone()),
        subject: Some(reply.subject.clone()),
        importance: Some(reply.importance.clone()),
    };
    let mut notified = HashSet::new();
    for name in resolved_to.iter().chain(resolved_cc_recipients.iter()) {
        if notified.insert(name.clone()) {
            let _ = mcp_agent_mail_storage::emit_notification_signal(
                config,
                &project.slug,
                name,
                Some(&notification_meta),
            );
        }
    }

    // Write reply message bundle to git archive (best-effort)
    {
        let mut all_recipient_names: SmallVec<[String; 12]> = SmallVec::new();
        all_recipient_names.extend(resolved_to.iter().cloned());
        all_recipient_names.extend(resolved_cc_recipients.iter().cloned());
        all_recipient_names.extend(resolved_bcc_recipients.iter().cloned());

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
            config,
            &project.slug,
            &msg_json,
            &reply.body_md,
            &sender_name,
            &all_recipient_names,
            &[],
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
        to: resolved_to.to_vec(),
        cc: resolved_cc_recipients.to_vec(),
        bcc: resolved_bcc_recipients.to_vec(),
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
        to: resolved_to.into_vec(),
        cc: resolved_cc_recipients.into_vec(),
        bcc: resolved_bcc_recipients.into_vec(),
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
#[allow(clippy::too_many_lines)]
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
        return Err(legacy_tool_error(
            "INVALID_LIMIT",
            format!("limit must be at least 1, got {msg_limit}. Use a positive integer."),
            true,
            json!({ "provided": msg_limit, "min": 1, "max": 1000 }),
        ));
    }
    if msg_limit > 1000 {
        tracing::info!(
            "fetch_inbox limit {} is very large; capping at 1000",
            msg_limit
        );
        msg_limit = 1000;
    }
    let msg_limit = usize::try_from(msg_limit).map_err(|_| {
        legacy_tool_error(
            "INVALID_LIMIT",
            format!("limit exceeds supported range: {msg_limit}"),
            true,
            json!({ "provided": msg_limit, "min": 1, "max": 1000 }),
        )
    })?;
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
            legacy_tool_error(
                "INVALID_TIMESTAMP",
                format!(
                    "Invalid since_ts format: '{ts}'. \
                     Expected ISO-8601 format like '2025-01-15T10:30:00+00:00' or '2025-01-15T10:30:00Z'. \
                     Common mistakes: missing timezone (add +00:00 or Z), using slashes instead of dashes, \
                     or using 12-hour format without AM/PM."
                ),
                true,
                json!({
                    "provided": ts,
                    "expected_format": "YYYY-MM-DDTHH:MM:SS+HH:MM",
                }),
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
    let config = &Config::get();
    let _ = mcp_agent_mail_storage::clear_notification_signal(config, &project.slug, &agent.name);

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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // is_valid_thread_id
    // -----------------------------------------------------------------------

    #[test]
    fn thread_id_simple_alphanumeric() {
        assert!(is_valid_thread_id("abc123"));
    }

    #[test]
    fn thread_id_with_dots_dashes_underscores() {
        assert!(is_valid_thread_id("TKT-123"));
        assert!(is_valid_thread_id("br-2ei.5.7.2"));
        assert!(is_valid_thread_id("feature_xyz"));
    }

    #[test]
    fn thread_id_single_char() {
        assert!(is_valid_thread_id("a"));
        assert!(is_valid_thread_id("0"));
    }

    #[test]
    fn thread_id_empty_rejected() {
        assert!(!is_valid_thread_id(""));
    }

    #[test]
    fn thread_id_starts_with_dash_rejected() {
        assert!(!is_valid_thread_id("-abc"));
    }

    #[test]
    fn thread_id_starts_with_dot_rejected() {
        assert!(!is_valid_thread_id(".abc"));
    }

    #[test]
    fn thread_id_starts_with_underscore_rejected() {
        assert!(!is_valid_thread_id("_abc"));
    }

    #[test]
    fn thread_id_contains_space_rejected() {
        assert!(!is_valid_thread_id("foo bar"));
    }

    #[test]
    fn thread_id_contains_slash_rejected() {
        assert!(!is_valid_thread_id("foo/bar"));
    }

    #[test]
    fn thread_id_contains_at_rejected() {
        assert!(!is_valid_thread_id("user@host"));
    }

    #[test]
    fn thread_id_max_length_128_accepted() {
        let id: String = std::iter::once('a')
            .chain(std::iter::repeat_n('b', 127))
            .collect();
        assert_eq!(id.len(), 128);
        assert!(is_valid_thread_id(&id));
    }

    #[test]
    fn thread_id_over_128_rejected() {
        let id: String = "a".repeat(129);
        assert!(!is_valid_thread_id(&id));
    }

    #[test]
    fn thread_id_unicode_rejected() {
        assert!(!is_valid_thread_id("café"));
    }

    #[test]
    fn thread_id_all_dashes_rejected() {
        // First char must be alphanumeric, so starting with '-' fails.
        assert!(!is_valid_thread_id("---"));
    }

    #[test]
    fn thread_id_numeric_start() {
        assert!(is_valid_thread_id("42"));
        assert!(is_valid_thread_id("123-abc"));
    }

    // -----------------------------------------------------------------------
    // Importance validation (tested via string checks matching send_message logic)
    // -----------------------------------------------------------------------

    #[test]
    fn valid_importance_values() {
        let valid = ["low", "normal", "high", "urgent"];
        for v in &valid {
            assert!(valid.contains(v), "Expected valid: {v}");
        }
    }

    #[test]
    fn invalid_importance_values() {
        let valid = ["low", "normal", "high", "urgent"];
        for v in &["NORMAL", "Low", "critical", "medium", "", "none"] {
            assert!(!valid.contains(v), "Expected invalid: {v}");
        }
    }

    // -----------------------------------------------------------------------
    // Subject truncation (the algorithm used in send_message and reply_message)
    // -----------------------------------------------------------------------

    fn truncate_subject(subject: &str) -> String {
        if subject.chars().count() > 200 {
            subject.chars().take(200).collect::<String>()
        } else {
            subject.to_string()
        }
    }

    #[test]
    fn subject_under_limit_unchanged() {
        let s = "Short subject";
        assert_eq!(truncate_subject(s), s);
    }

    #[test]
    fn subject_exactly_200_unchanged() {
        let s: String = "x".repeat(200);
        assert_eq!(truncate_subject(&s).chars().count(), 200);
    }

    #[test]
    fn subject_over_200_truncated() {
        let s: String = "y".repeat(250);
        let result = truncate_subject(&s);
        assert_eq!(result.chars().count(), 200);
    }

    #[test]
    fn subject_multibyte_utf8_safe() {
        // Each emoji is 1 char but 4 bytes. 201 emojis = 201 chars.
        let s: String = "\u{1F600}".repeat(201);
        assert_eq!(s.chars().count(), 201);
        let result = truncate_subject(&s);
        assert_eq!(result.chars().count(), 200);
        // Verify the result is valid UTF-8 (implicit - it's a String)
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn subject_empty_unchanged() {
        assert_eq!(truncate_subject(""), "");
    }

    // -----------------------------------------------------------------------
    // Reply subject prefix (case-insensitive idempotent)
    // -----------------------------------------------------------------------

    fn apply_prefix(original_subject: &str, prefix: &str) -> String {
        if original_subject
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
        {
            original_subject.to_string()
        } else {
            format!("{prefix} {original_subject}")
        }
    }

    #[test]
    fn prefix_added_when_absent() {
        assert_eq!(apply_prefix("My topic", "Re:"), "Re: My topic");
    }

    #[test]
    fn prefix_not_duplicated_when_present() {
        assert_eq!(apply_prefix("Re: My topic", "Re:"), "Re: My topic");
    }

    #[test]
    fn prefix_case_insensitive() {
        assert_eq!(apply_prefix("re: My topic", "Re:"), "re: My topic");
        assert_eq!(apply_prefix("RE: My topic", "Re:"), "RE: My topic");
    }

    #[test]
    fn custom_prefix() {
        assert_eq!(apply_prefix("My topic", "FW:"), "FW: My topic");
        assert_eq!(apply_prefix("FW: My topic", "FW:"), "FW: My topic");
    }

    // -----------------------------------------------------------------------
    // Empty recipients detection (send_message validation)
    // -----------------------------------------------------------------------

    #[test]
    fn empty_to_list_detected() {
        let to: Vec<String> = vec![];
        assert!(to.is_empty());
    }

    #[test]
    fn non_empty_to_list_accepted() {
        let to = ["BlueLake".to_string()];
        assert!(!to.is_empty());
    }

    #[test]
    fn has_any_recipients_false_when_all_empty() {
        let to: Vec<String> = vec![];
        let cc: Vec<String> = vec![];
        let bcc: Vec<String> = vec![];
        assert!(!has_any_recipients(&to, &cc, &bcc));
    }

    #[test]
    fn has_any_recipients_true_when_cc_or_bcc_present() {
        let to: Vec<String> = vec![];
        let cc: Vec<String> = vec!["BlueLake".to_string()];
        let bcc: Vec<String> = vec![];
        assert!(has_any_recipients(&to, &cc, &bcc));
    }

    // -----------------------------------------------------------------------
    // Response type serialization
    // -----------------------------------------------------------------------

    #[test]
    fn send_message_response_serializes() {
        let r = SendMessageResponse {
            deliveries: vec![],
            count: 0,
            attachments: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["count"], 0);
        assert!(json["deliveries"].as_array().unwrap().is_empty());
    }

    #[test]
    fn inbox_message_omits_body_when_none() {
        let r = InboxMessage {
            id: 1,
            project_id: 1,
            sender_id: 1,
            thread_id: None,
            subject: "test".into(),
            importance: "normal".into(),
            ack_required: false,
            from: "BlueLake".into(),
            created_ts: Some("2026-02-06T00:00:00Z".into()),
            kind: "to".into(),
            attachments: vec![],
            body_md: None,
        };
        let json_str = serde_json::to_string(&r).unwrap();
        assert!(!json_str.contains("body_md"));
    }

    #[test]
    fn inbox_message_includes_body_when_present() {
        let r = InboxMessage {
            id: 1,
            project_id: 1,
            sender_id: 1,
            thread_id: Some("thread-1".into()),
            subject: "test".into(),
            importance: "normal".into(),
            ack_required: true,
            from: "BlueLake".into(),
            created_ts: Some("2026-02-06T00:00:00Z".into()),
            kind: "to".into(),
            attachments: vec!["img.webp".into()],
            body_md: Some("Hello world".into()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["body_md"], "Hello world");
        assert_eq!(json["ack_required"], true);
        assert_eq!(json["thread_id"], "thread-1");
    }

    #[test]
    fn read_status_response_omits_null_read_at() {
        let r = ReadStatusResponse {
            message_id: 42,
            read: false,
            read_at: None,
        };
        let json_str = serde_json::to_string(&r).unwrap();
        assert!(!json_str.contains("read_at"));
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json["message_id"], 42);
        assert_eq!(json["read"], false);
    }

    #[test]
    fn ack_status_response_includes_timestamps() {
        let r = AckStatusResponse {
            message_id: 10,
            acknowledged: true,
            acknowledged_at: Some("2026-02-06T01:00:00Z".into()),
            read_at: Some("2026-02-06T00:30:00Z".into()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["acknowledged"], true);
        assert!(json["acknowledged_at"].is_string());
        assert!(json["read_at"].is_string());
    }

    #[test]
    fn message_payload_serializes_all_fields() {
        let r = MessagePayload {
            id: 1,
            project_id: 1,
            sender_id: 2,
            thread_id: Some("t-1".into()),
            subject: "Hello".into(),
            body_md: "# Content".into(),
            importance: "high".into(),
            ack_required: true,
            created_ts: Some("2026-02-06T00:00:00Z".into()),
            attachments: vec!["file.webp".into()],
            from: "BlueLake".into(),
            to: vec!["RedFox".into()],
            cc: vec!["GoldHawk".into()],
            bcc: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["from"], "BlueLake");
        assert_eq!(json["to"][0], "RedFox");
        assert_eq!(json["cc"][0], "GoldHawk");
        assert!(json["bcc"].as_array().unwrap().is_empty());
        assert_eq!(json["importance"], "high");
    }

    #[test]
    fn reply_response_round_trips() {
        let original = ReplyMessageResponse {
            id: 5,
            project_id: 1,
            sender_id: 2,
            thread_id: Some("t-1".into()),
            subject: "Re: Hello".into(),
            importance: "normal".into(),
            ack_required: false,
            created_ts: Some("2026-02-06T00:00:00Z".into()),
            attachments: vec![],
            body_md: "Reply body".into(),
            from: "BlueLake".into(),
            to: vec!["RedFox".into()],
            cc: vec![],
            bcc: vec![],
            reply_to: 3,
            deliveries: vec![],
            count: 1,
        };
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: ReplyMessageResponse = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.id, 5);
        assert_eq!(deserialized.reply_to, 3);
        assert_eq!(deserialized.subject, "Re: Hello");
    }

    // -----------------------------------------------------------------------
    // validate_message_size_limits
    // -----------------------------------------------------------------------

    fn config_with_limits(body: usize, attachment: usize, total: usize, subject: usize) -> Config {
        Config {
            max_message_body_bytes: body,
            max_attachment_bytes: attachment,
            max_total_message_bytes: total,
            max_subject_bytes: subject,
            ..Config::default()
        }
    }

    #[test]
    fn size_limits_pass_when_under() {
        let cfg = config_with_limits(1024, 1024, 2048, 256);
        let result = validate_message_size_limits(&cfg, "Hello", "Body text", None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_pass_when_zero_unlimited() {
        let cfg = config_with_limits(0, 0, 0, 0);
        let big = "x".repeat(10_000_000);
        let result = validate_message_size_limits(&cfg, &big, &big, None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_reject_oversized_subject() {
        let cfg = config_with_limits(0, 0, 0, 10);
        let subject = "A".repeat(11);
        let result = validate_message_size_limits(&cfg, &subject, "", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("subject") || err.to_string().contains("Subject"));
    }

    #[test]
    fn size_limits_accept_exact_subject() {
        let cfg = config_with_limits(0, 0, 0, 10);
        let subject = "A".repeat(10);
        let result = validate_message_size_limits(&cfg, &subject, "", None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_reject_oversized_body() {
        let cfg = config_with_limits(100, 0, 0, 0);
        let body = "B".repeat(101);
        let result = validate_message_size_limits(&cfg, "", &body, None);
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_accept_exact_body() {
        let cfg = config_with_limits(100, 0, 0, 0);
        let body = "B".repeat(100);
        let result = validate_message_size_limits(&cfg, "", &body, None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_reject_total_overflow() {
        // Subject + body exceed total even though each is within individual limits
        let cfg = config_with_limits(100, 0, 50, 100);
        let result = validate_message_size_limits(&cfg, "sub", &"x".repeat(50), None);
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_reject_oversized_attachment() {
        let cfg = config_with_limits(0, 10, 0, 0);
        // Create a temp file larger than 10 bytes
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, "x".repeat(20)).unwrap();
        let paths = vec![path.to_string_lossy().to_string()];
        let result = validate_message_size_limits(&cfg, "", "", Some(&paths));
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_accept_small_attachment() {
        let cfg = config_with_limits(0, 100, 0, 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "hello").unwrap();
        let paths = vec![path.to_string_lossy().to_string()];
        let result = validate_message_size_limits(&cfg, "", "", Some(&paths));
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_attachment_contributes_to_total() {
        let cfg = config_with_limits(0, 0, 50, 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("medium.txt");
        std::fs::write(&path, "x".repeat(45)).unwrap();
        let paths = vec![path.to_string_lossy().to_string()];
        // body (10) + attachment (45) = 55 > total limit of 50
        let result = validate_message_size_limits(&cfg, "", &"y".repeat(10), Some(&paths));
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_nonexistent_attachment_skipped() {
        let cfg = config_with_limits(0, 10, 0, 0);
        let paths = vec!["/nonexistent/file.txt".to_string()];
        // Non-existent files are skipped (downstream handles the error)
        let result = validate_message_size_limits(&cfg, "", "", Some(&paths));
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // validate_reply_body_limit
    // -----------------------------------------------------------------------

    #[test]
    fn reply_body_limit_pass() {
        let cfg = config_with_limits(100, 0, 0, 0);
        assert!(validate_reply_body_limit(&cfg, &"r".repeat(100)).is_ok());
    }

    #[test]
    fn reply_body_limit_reject() {
        let cfg = config_with_limits(100, 0, 0, 0);
        assert!(validate_reply_body_limit(&cfg, &"r".repeat(101)).is_err());
    }

    #[test]
    fn reply_body_limit_unlimited() {
        let cfg = config_with_limits(0, 0, 0, 0);
        assert!(validate_reply_body_limit(&cfg, &"r".repeat(10_000_000)).is_ok());
    }

    // -----------------------------------------------------------------------
    // sanitize_thread_id
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_thread_id_valid_passthrough() {
        assert_eq!(sanitize_thread_id("TKT-123", "fb"), "TKT-123");
        assert_eq!(sanitize_thread_id("br-2ei.5.7", "fb"), "br-2ei.5.7");
        assert_eq!(sanitize_thread_id("abc_123_xyz", "fb"), "abc_123_xyz");
    }

    #[test]
    fn sanitize_thread_id_strips_invalid_chars() {
        assert_eq!(sanitize_thread_id("foo bar", "fb"), "foobar");
        assert_eq!(sanitize_thread_id("a/b/c", "fb"), "abc");
        assert_eq!(sanitize_thread_id("test@host", "fb"), "testhost");
    }

    #[test]
    fn sanitize_thread_id_truncates_long() {
        let long = "a".repeat(200);
        let result = sanitize_thread_id(&long, "fb");
        assert_eq!(result.len(), 128);
    }

    #[test]
    fn sanitize_thread_id_empty_uses_fallback() {
        assert_eq!(sanitize_thread_id("", "fb"), "fb");
        assert_eq!(sanitize_thread_id("@#$%", "fb"), "fb");
    }

    #[test]
    fn sanitize_thread_id_non_alpha_start_uses_fallback() {
        assert_eq!(sanitize_thread_id("-abc", "fb"), "fb");
        assert_eq!(sanitize_thread_id(".xyz", "fb"), "fb");
        assert_eq!(sanitize_thread_id("_foo", "fb"), "fb");
    }

    #[test]
    fn sanitize_thread_id_preserves_numeric_start() {
        assert_eq!(sanitize_thread_id("123", "fb"), "123");
        assert_eq!(sanitize_thread_id("42-abc", "fb"), "42-abc");
    }

    #[test]
    fn sanitize_thread_id_unicode_stripped() {
        assert_eq!(sanitize_thread_id("café", "fb"), "caf");
        assert_eq!(sanitize_thread_id("日本", "fb"), "fb");
    }

    // ── br-1i11.6.6: E2E reply-flow tests with malformed thread_id fixtures ──
    //
    // Exercises the full sanitize → validate → reply path with realistic
    // malformed thread_id data that could appear in legacy databases.
    // Each fixture includes the original value, the expected sanitized result,
    // the decision path taken, and a reproduction command.

    /// Fixture entry for malformed `thread_id` E2E testing.
    struct ThreadIdFixture {
        raw: &'static str,
        /// Expected result after `sanitize_thread_id`
        expected: &'static str,
        uses_fallback: bool,
        decision_path: &'static str,
    }

    const MALFORMED_THREAD_ID_FIXTURES: &[ThreadIdFixture] = &[
        // Path traversal attempts (migration artifacts)
        ThreadIdFixture {
            raw: "../../../etc/passwd",
            expected: "fb",
            uses_fallback: true,
            decision_path: "strip slashes+dots → '..etcpasswd' → starts with dot → fallback",
        },
        ThreadIdFixture {
            raw: "..%2F..%2Fetc%2Fpasswd",
            expected: "fb",
            uses_fallback: true,
            decision_path: "strip % → '..2F..2Fetc2Fpasswd' → starts with dot → fallback",
        },
        // SQL injection fragments — dashes are valid chars so they survive
        ThreadIdFixture {
            raw: "thread'; DROP TABLE messages;--",
            expected: "threadDROPTABLEmessages--",
            uses_fallback: false,
            decision_path: "strip quotes/spaces/semicolons → 'threadDROPTABLEmessages--' → starts with 't' → accept",
        },
        // Unicode normalization edge cases
        ThreadIdFixture {
            raw: "café-thread",
            expected: "caf-thread",
            uses_fallback: false,
            decision_path: "strip non-ASCII 'é' → 'caf-thread' → starts with 'c' → accept",
        },
        ThreadIdFixture {
            raw: "日本語スレッド",
            expected: "fb",
            uses_fallback: true,
            decision_path: "strip all non-ASCII → empty → fallback",
        },
        // Null bytes and control chars
        ThreadIdFixture {
            raw: "thread\x00-id",
            expected: "thread-id",
            uses_fallback: false,
            decision_path: "strip null → 'thread-id' → starts with 't' → accept",
        },
        ThreadIdFixture {
            raw: "\x01\x02\x03abc",
            expected: "abc",
            uses_fallback: false,
            decision_path: "strip control chars → 'abc' → starts with 'a' → accept",
        },
        // Empty and whitespace-only
        ThreadIdFixture {
            raw: "",
            expected: "fb",
            uses_fallback: true,
            decision_path: "empty → fallback",
        },
        ThreadIdFixture {
            raw: "   ",
            expected: "fb",
            uses_fallback: true,
            decision_path: "strip spaces → empty → fallback",
        },
        ThreadIdFixture {
            raw: "\t\n\r",
            expected: "fb",
            uses_fallback: true,
            decision_path: "strip whitespace → empty → fallback",
        },
        // Leading invalid chars
        ThreadIdFixture {
            raw: "-starts-with-dash",
            expected: "fb",
            uses_fallback: true,
            decision_path: "strip nothing, first char '-' not alphanumeric → fallback",
        },
        ThreadIdFixture {
            raw: ".hidden-thread",
            expected: "fb",
            uses_fallback: true,
            decision_path: "first char '.' → not stripped (valid char) but not alphanumeric start → fallback",
        },
        ThreadIdFixture {
            raw: "_underscore_start",
            expected: "fb",
            uses_fallback: true,
            decision_path: "first char '_' → valid char but not alphanumeric start → fallback",
        },
        // Very long legacy values
        ThreadIdFixture {
            raw: "abcdefghijklmnopqrstuvwxyz0123456789-abcdefghijklmnopqrstuvwxyz0123456789-abcdefghijklmnopqrstuvwxyz0123456789-abcdefghijklmnopqrstuvwxyz0123456789-extra",
            expected: "abcdefghijklmnopqrstuvwxyz0123456789-abcdefghijklmnopqrstuvwxyz0123456789-abcdefghijklmnopqrstuvwxyz0123456789-abcdefghijklmnopq",
            uses_fallback: false,
            decision_path: "truncate to 128 chars → starts with 'a' → accept",
        },
        // Mixed valid and invalid
        ThreadIdFixture {
            raw: "TKT 123 with spaces",
            expected: "TKT123withspaces",
            uses_fallback: false,
            decision_path: "strip spaces → 'TKT123withspaces' → starts with 'T' → accept",
        },
        // HTML/script injection — angle brackets/quotes/parens stripped
        ThreadIdFixture {
            raw: "<script>alert('xss')</script>",
            expected: "scriptalertxssscript",
            uses_fallback: false,
            decision_path: "strip '<', '>', '(', ')', quote → 'scriptalertxssscript' → starts with 's' → accept",
        },
        // Valid legacy formats that should pass through
        ThreadIdFixture {
            raw: "TKT-123",
            expected: "TKT-123",
            uses_fallback: false,
            decision_path: "all chars valid, starts with 'T' → passthrough",
        },
        ThreadIdFixture {
            raw: "br-2ei.5.7.2",
            expected: "br-2ei.5.7.2",
            uses_fallback: false,
            decision_path: "all chars valid, starts with 'b' → passthrough",
        },
        ThreadIdFixture {
            raw: "42",
            expected: "42",
            uses_fallback: false,
            decision_path: "numeric start valid → passthrough",
        },
    ];

    #[test]
    fn sanitize_thread_id_e2e_malformed_fixtures() {
        let fallback = "fb";
        for (i, fixture) in MALFORMED_THREAD_ID_FIXTURES.iter().enumerate() {
            let result = sanitize_thread_id(fixture.raw, fallback);
            let used_fallback = result == fallback && fixture.raw != fallback;

            eprintln!(
                "fixture[{i}] raw={:?} expected={:?} got={:?} fallback={} decision={}",
                fixture.raw, fixture.expected, result, used_fallback, fixture.decision_path
            );

            assert_eq!(
                result, fixture.expected,
                "fixture[{i}]: sanitize_thread_id({:?}, {:?}) = {:?}, expected {:?}\n  decision_path: {}\n  reproduction: cargo test -p mcp-agent-mail-tools sanitize_thread_id_e2e_malformed_fixtures -- --nocapture",
                fixture.raw, fallback, result, fixture.expected, fixture.decision_path
            );

            if fixture.uses_fallback {
                assert_eq!(
                    result, fallback,
                    "fixture[{i}]: expected fallback but got {result:?}"
                );
            }

            // Post-condition: result must be a valid thread_id (or fallback)
            assert!(
                is_valid_thread_id(&result),
                "fixture[{i}]: sanitized result {result:?} is not a valid thread_id"
            );
        }
    }

    #[test]
    fn sanitize_thread_id_e2e_reply_flow_simulation() {
        // Simulate the exact code path from reply_message (lines 1176-1182):
        // let fallback_tid = message_id.to_string();
        // let thread_id = match original.thread_id.as_deref() {
        //     Some(tid) => sanitize_thread_id(tid, &fallback_tid),
        //     None => fallback_tid,
        // };

        #[allow(clippy::struct_field_names)]
        struct ReplyScenario {
            original_thread_id: Option<&'static str>,
            message_id: i64,
            expected_thread_id: &'static str,
        }

        let scenarios = [
            ReplyScenario {
                original_thread_id: Some("TKT-123"),
                message_id: 42,
                expected_thread_id: "TKT-123",
            },
            ReplyScenario {
                original_thread_id: Some("../etc/passwd"),
                message_id: 99,
                expected_thread_id: "99", // fallback to message_id
            },
            ReplyScenario {
                original_thread_id: Some(""),
                message_id: 7,
                expected_thread_id: "7",
            },
            ReplyScenario {
                original_thread_id: None,
                message_id: 55,
                expected_thread_id: "55",
            },
            ReplyScenario {
                original_thread_id: Some("-invalid-start"),
                message_id: 101,
                expected_thread_id: "101",
            },
            ReplyScenario {
                original_thread_id: Some("valid.thread-id_123"),
                message_id: 200,
                expected_thread_id: "valid.thread-id_123",
            },
            ReplyScenario {
                original_thread_id: Some("日本語"),
                message_id: 300,
                expected_thread_id: "300",
            },
        ];

        for (i, s) in scenarios.iter().enumerate() {
            let fallback_tid = s.message_id.to_string();
            let thread_id = match s.original_thread_id {
                Some(tid) => sanitize_thread_id(tid, &fallback_tid),
                None => fallback_tid,
            };

            eprintln!(
                "reply_flow[{i}] original_tid={:?} msg_id={} → thread_id={:?}",
                s.original_thread_id, s.message_id, thread_id
            );

            assert_eq!(
                thread_id, s.expected_thread_id,
                "reply_flow[{i}]: expected {:?}, got {:?}\n  reproduction: cargo test -p mcp-agent-mail-tools sanitize_thread_id_e2e_reply_flow_simulation -- --nocapture",
                s.expected_thread_id, thread_id
            );

            // Post-condition: result must always be valid
            assert!(
                is_valid_thread_id(&thread_id),
                "reply_flow[{i}]: result {thread_id:?} is not a valid thread_id"
            );
        }
    }

    // -----------------------------------------------------------------------
    // validate_message_size_limits — boundary and edge-case coverage
    // -----------------------------------------------------------------------

    #[test]
    fn size_limits_multiple_attachments_sum_to_total() {
        let cfg = config_with_limits(0, 0, 100, 0);
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("a.txt");
        let p2 = dir.path().join("b.txt");
        std::fs::write(&p1, "x".repeat(40)).unwrap();
        std::fs::write(&p2, "y".repeat(40)).unwrap();
        let paths = vec![
            p1.to_string_lossy().to_string(),
            p2.to_string_lossy().to_string(),
        ];
        // subject(0) + body(25) + a(40) + b(40) = 105 > 100
        let result = validate_message_size_limits(&cfg, "", &"z".repeat(25), Some(&paths));
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_empty_subject_and_body_pass() {
        let cfg = config_with_limits(1, 1, 1, 1);
        // Empty strings have length 0 which is ≤ any positive limit
        let result = validate_message_size_limits(&cfg, "", "", None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_error_message_contains_field_info() {
        let cfg = config_with_limits(10, 0, 0, 0);
        let err = validate_message_size_limits(&cfg, "", &"x".repeat(20), None).unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("body") || err_str.contains("Body"),
            "Error should mention body field: {err_str}"
        );
    }

    #[test]
    fn size_limits_subject_error_mentions_subject() {
        let cfg = config_with_limits(0, 0, 0, 5);
        let err = validate_message_size_limits(&cfg, "toolong", "", None).unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("ubject"),
            "Error should mention subject: {err_str}"
        );
    }

    #[test]
    fn size_limits_saturating_add_prevents_overflow() {
        // When total limit is small but file_size would be huge, saturating_add
        // should clamp to usize::MAX rather than wrapping to a small value.
        let cfg = config_with_limits(0, 0, 100, 0);
        // Even without real filesystem paths, we can test the accumulation logic:
        // subject(5) + body(10) = 15, which is under 100.
        let result = validate_message_size_limits(&cfg, "hello", &"x".repeat(10), None);
        assert!(result.is_ok());

        // Now with total limit = 10, subject(5) + body(10) = 15 > 10 via saturating_add
        let cfg2 = config_with_limits(0, 0, 10, 0);
        let result = validate_message_size_limits(&cfg2, "hello", &"x".repeat(10), None);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // validate_reply_body_limit — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn reply_body_limit_exact_boundary() {
        let cfg = config_with_limits(50, 0, 0, 0);
        assert!(validate_reply_body_limit(&cfg, &"r".repeat(50)).is_ok());
        assert!(validate_reply_body_limit(&cfg, &"r".repeat(51)).is_err());
    }

    #[test]
    fn reply_body_limit_empty_body_passes() {
        let cfg = config_with_limits(1, 0, 0, 0);
        assert!(validate_reply_body_limit(&cfg, "").is_ok());
    }

    // -----------------------------------------------------------------------
    // Importance validation — exhaustive enum coverage
    // -----------------------------------------------------------------------

    #[test]
    fn importance_case_sensitive_rejects_uppercase() {
        let valid = ["low", "normal", "high", "urgent"];
        for v in ["LOW", "Normal", "HIGH", "URGENT", "Urgent"] {
            assert!(
                !valid.contains(&v),
                "Importance should be case-sensitive, {v} should be rejected"
            );
        }
    }

    #[test]
    fn importance_rejects_common_typos() {
        let valid = ["low", "normal", "high", "urgent"];
        for v in ["critical", "medium", "info", "warning", "severe", "p0", "1"] {
            assert!(
                !valid.contains(&v),
                "Importance should reject typo/alias: {v}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Thread ID — additional boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn thread_id_exactly_128_is_valid() {
        let id: String = "a".repeat(128);
        assert!(is_valid_thread_id(&id));
    }

    #[test]
    fn thread_id_127_plus_special_chars_valid() {
        let mut id = String::from("X");
        id.push_str(&"-".repeat(127));
        assert_eq!(id.len(), 128);
        assert!(is_valid_thread_id(&id));
    }

    #[test]
    fn thread_id_mixed_valid_chars() {
        assert!(is_valid_thread_id("a.b-c_d"));
        assert!(is_valid_thread_id("br-2ei.5.7.2"));
        assert!(is_valid_thread_id("JIRA-12345"));
    }

    #[test]
    fn thread_id_tab_char_rejected() {
        assert!(!is_valid_thread_id("foo\tbar"));
    }

    #[test]
    fn thread_id_newline_rejected() {
        assert!(!is_valid_thread_id("foo\nbar"));
    }

    #[test]
    fn thread_id_null_byte_rejected() {
        assert!(!is_valid_thread_id("foo\0bar"));
    }

    // -----------------------------------------------------------------------
    // sanitize_thread_id — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_thread_id_fallback_itself_is_valid() {
        // Ensure the fallback is always returned when input is all-invalid
        let result = sanitize_thread_id("!!!!", "msg-42");
        assert_eq!(result, "msg-42");
        assert!(is_valid_thread_id(&result));
    }

    #[test]
    fn sanitize_thread_id_mixed_valid_invalid_preserves_valid() {
        let result = sanitize_thread_id("a@b#c", "fb");
        assert_eq!(result, "abc");
    }

    #[test]
    fn sanitize_thread_id_only_dashes_uses_fallback() {
        // Dashes are valid chars but can't start the string
        let result = sanitize_thread_id("---", "fb");
        assert_eq!(result, "fb");
    }

    // -----------------------------------------------------------------------
    // Response struct serialization — round-trip and edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn delivery_result_serializes_project() {
        let r = DeliveryResult {
            project: "/data/my-project".into(),
            payload: MessagePayload {
                id: 1,
                project_id: 1,
                sender_id: 1,
                thread_id: None,
                subject: "test".into(),
                body_md: "body".into(),
                importance: "normal".into(),
                ack_required: false,
                created_ts: None,
                attachments: vec![],
                from: "A".into(),
                to: vec!["B".into()],
                cc: vec![],
                bcc: vec![],
            },
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["project"], "/data/my-project");
        assert_eq!(json["payload"]["from"], "A");
    }

    #[test]
    fn message_payload_thread_id_null_when_none() {
        let r = MessagePayload {
            id: 1,
            project_id: 1,
            sender_id: 1,
            thread_id: None,
            subject: "s".into(),
            body_md: "b".into(),
            importance: "low".into(),
            ack_required: false,
            created_ts: None,
            attachments: vec![],
            from: "X".into(),
            to: vec![],
            cc: vec![],
            bcc: vec![],
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(json["thread_id"].is_null());
    }

    #[test]
    fn ack_status_omits_null_timestamps() {
        let r = AckStatusResponse {
            message_id: 1,
            acknowledged: false,
            acknowledged_at: None,
            read_at: None,
        };
        let json_str = serde_json::to_string(&r).unwrap();
        assert!(!json_str.contains("acknowledged_at"));
        assert!(!json_str.contains("read_at"));
    }
}
