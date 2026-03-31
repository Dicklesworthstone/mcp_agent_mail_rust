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
use std::path::Path;

use serde_json::{Value, json};

use crate::tool_util::{
    db_error_to_mcp_error, db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_agent,
    resolve_project,
};
use mcp_agent_mail_core::pattern_overlap::CompiledPattern;

pub(crate) fn try_dispatch_archive_write(op: mcp_agent_mail_storage::WriteOp, context: &str) {
    match mcp_agent_mail_storage::wbq_enqueue(op.clone()) {
        mcp_agent_mail_storage::WbqEnqueueResult::Enqueued
        | mcp_agent_mail_storage::WbqEnqueueResult::SkippedDiskCritical => {
            // Disk pressure guard: archive writes may be disabled; DB remains authoritative.
        }
        mcp_agent_mail_storage::WbqEnqueueResult::QueueUnavailable => {
            tracing::warn!("{context}; WBQ unavailable, falling back to direct archive write");
            if let Err(error) = mcp_agent_mail_storage::write_op_sync(&op) {
                tracing::warn!(error = %error, "{context}; direct archive write failed");
            }
        }
    }
}

/// Write a message bundle to the git archive (best-effort).
/// Failures are logged but never fail the tool call.
///
/// Uses the write-behind queue when available. If the queue is unavailable,
/// falls back to the direct storage path before giving up.
pub fn try_write_message_archive(
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
    try_dispatch_archive_write(op, &format!("message archive write project={project_slug}"));
}

pub(crate) fn enqueue_message_semantic_index(
    project_id: i64,
    message_id: i64,
    subject: &str,
    body_md: &str,
) {
    let _ = mcp_agent_mail_db::search_service::enqueue_semantic_document(
        mcp_agent_mail_db::search_planner::DocKind::Message,
        message_id,
        Some(project_id),
        subject,
        body_md,
    );
}

/// Index a message into the Tantivy lexical search index (fire-and-forget).
///
/// Runs synchronously but is best-effort: failures are logged, never propagated.
pub(crate) fn enqueue_message_lexical_index(msg: &mcp_agent_mail_db::search_v3::IndexableMessage) {
    match mcp_agent_mail_db::search_v3::index_message(msg) {
        Ok(true) => {
            tracing::debug!(message_id = msg.id, "indexed message in Tantivy");
        }
        Ok(false) => {} // bridge not initialized, silent skip
        Err(e) => {
            tracing::warn!(
                message_id = msg.id,
                error = %e,
                "failed to index message in Tantivy (non-fatal)"
            );
        }
    }
}

pub(crate) fn enqueue_agent_semantic_index(agent: &mcp_agent_mail_db::AgentRow) {
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

fn is_agent_unique_constraint_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    let Some((prefix, columns)) = normalized.split_once(':') else {
        return false;
    };
    if !prefix.contains("unique constraint failed") {
        return false;
    }

    let columns = columns
        .split(',')
        .map(str::trim)
        .map(|column| column.rsplit('.').next().unwrap_or(column))
        .collect::<Vec<_>>();
    columns.contains(&"project_id") && columns.contains(&"name")
}

fn contact_blocked_error() -> McpError {
    const MESSAGE: &str = "Recipient is not accepting messages.";
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        MESSAGE,
        json!({
            "error": {
                "type": "CONTACT_BLOCKED",
                "message": MESSAGE,
                "recoverable": true
            }
        }),
    )
}

fn contact_required_error(
    project_key: &str,
    sender_name: &str,
    blocked_recipients: &[String],
    attempted: &[String],
    ttl_seconds: i64,
) -> McpError {
    let mut blocked_sorted = blocked_recipients.to_vec();
    blocked_sorted.sort();
    blocked_sorted.dedup();
    let recipient_list = blocked_sorted.join(", ");
    let sample_target = blocked_sorted.first().cloned().unwrap_or_default();

    let mut err_msg_parts = vec![
        format!("Contact approval required for recipients: {recipient_list}."),
        format!(
            "Before retrying, request approval with \
             `request_contact(project_key='{project_key}', from_agent='{sender_name}', \
             to_agent='{sample_target}')` or run \
             `macro_contact_handshake(project_key='{project_key}', requester='{sender_name}', \
             target='{sample_target}', auto_accept=True)`."
        ),
        "Alternatively, send your message inside a recent thread that already includes them by reusing its thread_id.".to_string(),
    ];
    if !attempted.is_empty() {
        err_msg_parts.push(format!(
            "Automatic handshake attempts already ran for: {}; wait for approval or retry the suggested calls explicitly.",
            attempted.join(", ")
        ));
    }
    let err_msg = err_msg_parts.join(" ");

    let mut examples = vec![json!({
        "tool": "macro_contact_handshake",
        "arguments": {
            "project_key": project_key,
            "requester": sender_name,
            "target": sample_target,
            "auto_accept": true,
            "ttl_seconds": ttl_seconds,
        }
    })];
    for nm in blocked_sorted.iter().take(3) {
        examples.push(json!({
            "tool": "request_contact",
            "arguments": {
                "project_key": project_key,
                "from_agent": sender_name,
                "to_agent": nm,
                "ttl_seconds": ttl_seconds,
            }
        }));
    }

    legacy_tool_error(
        "CONTACT_REQUIRED",
        err_msg,
        true,
        json!({
            "recipients_blocked": blocked_sorted,
            "remedies": [
                "Call request_contact(project_key, from_agent, to_agent) to request approval",
                "Call macro_contact_handshake(project_key, requester, target, auto_accept=true) to automate"
            ],
            "auto_contact_attempted": attempted,
            "suggested_tool_calls": examples,
        }),
    )
}

fn recipient_not_found_error(
    project_human_key: &str,
    project_slug: &str,
    sender: &mcp_agent_mail_db::AgentRow,
    missing_local: &[String],
    suggestions_map: Option<&HashMap<String, Vec<Value>>>,
) -> McpError {
    let mut missing_sorted = missing_local.to_vec();
    missing_sorted.sort_unstable();
    missing_sorted.dedup();

    let hint = format!(
        "Use resource://agents/{project_slug} to list registered agents or register new identities."
    );
    let message = format!(
        "Unable to send message — local recipients {} are not registered in project '{project_human_key}'; {hint}",
        missing_sorted.join(", "),
    );

    let suggested_tool_calls: Vec<Value> = missing_sorted
        .iter()
        .take(5)
        .map(|name| {
            json!({
                "tool": "register_agent",
                "arguments": {
                    "project_key": project_human_key,
                    "name": name,
                    "program": sender.program,
                    "model": sender.model,
                    "task_description": sender.task_description,
                },
            })
        })
        .collect();

    let mut data = json!({
        "unknown_local": missing_sorted,
        "hint": hint,
    });
    if let Some(obj) = data.as_object_mut() {
        if !suggested_tool_calls.is_empty() {
            obj.insert(
                "suggested_tool_calls".to_string(),
                Value::Array(suggested_tool_calls),
            );
        }
        if let Some(map) = suggestions_map
            && !map.is_empty()
        {
            // Flatten map to JSON object
            let mut sug_json = serde_json::Map::new();
            for (k, v) in map {
                sug_json.insert(k.clone(), Value::Array(v.clone()));
            }
            obj.insert("suggestions".to_string(), Value::Object(sug_json));
        }
    }

    legacy_tool_error("RECIPIENT_NOT_FOUND", message, true, data)
}

fn extract_recipient_not_found_names(err: &McpError) -> Option<Vec<String>> {
    let error_payload = err
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|root| root.get("error"))
        .and_then(Value::as_object)?;
    if error_payload.get("type").and_then(Value::as_str) != Some("RECIPIENT_NOT_FOUND") {
        return None;
    }
    let names = error_payload
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("unknown_local"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(names)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContactPolicyDecision {
    Allow,
    RequireApproval,
    BlockAll,
}

fn contact_policy_decision(
    sender_name: &str,
    recipient_name: &str,
    policy_raw: &str,
    recent_ok: bool,
    approved: bool,
) -> ContactPolicyDecision {
    // 1) Allow self messages first.
    if recipient_name == sender_name {
        return ContactPolicyDecision::Allow;
    }

    let mut policy = policy_raw.to_lowercase();
    if !["open", "auto", "contacts_only", "block_all"].contains(&policy.as_str()) {
        policy = "auto".to_string();
    }

    // 2) block_all is an immediate hard stop.
    if policy == "block_all" {
        return ContactPolicyDecision::BlockAll;
    }

    if policy == "open" {
        return ContactPolicyDecision::Allow;
    }

    // 3) auto + recent contact.
    if policy == "auto" && recent_ok {
        return ContactPolicyDecision::Allow;
    }

    // 4) approved AgentLink.
    if approved {
        return ContactPolicyDecision::Allow;
    }

    // 5) otherwise blocked.
    ContactPolicyDecision::RequireApproval
}

fn reservations_prove_shared_scope_for_contact(
    sender_patterns: &[CompiledPattern],
    recipient_patterns: &[CompiledPattern],
) -> bool {
    sender_patterns.iter().any(|sender_pattern| {
        recipient_patterns.iter().any(|recipient_pattern| {
            if sender_pattern.normalized() == recipient_pattern.normalized() {
                return sender_pattern.is_matchable() && recipient_pattern.is_matchable();
            }

            // `CompiledPattern::overlaps` is intentionally conservative for
            // reservation conflict detection; two unrelated globs can return
            // true when the matcher cannot prove disjointness. Contact
            // enforcement must fail closed, so only exact/glob or exact/exact
            // overlaps are trusted here.
            if sender_pattern.is_glob() && recipient_pattern.is_glob() {
                return false;
            }

            sender_pattern.overlaps(recipient_pattern)
        })
    })
}

async fn resolve_or_register_agent(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::DbPool,
    project_id: i64,
    agent_name: &str,
    _sender: &mcp_agent_mail_db::AgentRow,
    config: &Config,
) -> McpResult<mcp_agent_mail_db::AgentRow> {
    let agent_name = agent_name.trim();
    // Normalize name if it follows the adj+noun pattern, otherwise keep as-is.
    let agent_name_norm = mcp_agent_mail_core::models::normalize_agent_name(agent_name)
        .unwrap_or_else(|| agent_name.to_string());

    let agent =
        match mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, &agent_name_norm)
            .await
        {
            Outcome::Ok(agent) => Ok(agent),
            Outcome::Err(DbError::NotFound { .. }) if config.messaging_auto_register_recipients => {
                match mcp_agent_mail_db::queries::register_agent(
                    ctx.cx(),
                    pool,
                    project_id,
                    &agent_name_norm,
                    "unknown",
                    "unknown",
                    None,
                    None,
                    None,
                )
                .await
                {
                    Outcome::Ok(_) => {}
                    Outcome::Err(DbError::Sqlite(message))
                        if is_agent_unique_constraint_error(&message) =>
                    {
                        tracing::debug!(
                            project_id,
                            agent = %agent_name,
                            "auto-register race detected; loading existing agent row"
                        );
                    }
                    Outcome::Err(e) => return Err(db_error_to_mcp_error(e)),
                    Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
                    Outcome::Panicked(p) => {
                        return Err(McpError::internal_error(format!(
                            "Internal panic: {}",
                            p.message()
                        )));
                    }
                }
                db_outcome_to_mcp_result(
                    mcp_agent_mail_db::queries::get_agent(
                        ctx.cx(),
                        pool,
                        project_id,
                        &agent_name_norm,
                    )
                    .await,
                )
            }
            Outcome::Err(e) => Err(db_error_to_mcp_error(e)),
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(p) => Err(McpError::internal_error(format!(
                "Internal panic: {}",
                p.message()
            ))),
        }?;
    enqueue_agent_semantic_index(&agent);
    Ok(agent)
}

/// Validate `thread_id` format: must start with alphanumeric and contain only
/// letters, numbers, '.', '_', or '-'. Max 128 chars.
fn is_valid_thread_id(tid: &str) -> bool {
    if tid.is_empty() || tid.len() > 128 {
        return false;
    }
    let Some(first) = tid.chars().next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    tid.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

fn is_bare_numeric_thread_id(tid: &str) -> bool {
    !tid.is_empty() && tid.bytes().all(|b| b.is_ascii_digit())
}

fn invalid_thread_id_error(tid: &str, reason: &str) -> McpError {
    legacy_tool_error(
        "INVALID_THREAD_ID",
        format!(
            "Invalid thread_id: '{tid}'. {reason} \
             Examples: 'TKT-123', 'bd-42', 'feature-xyz'."
        ),
        true,
        json!({
            "provided": tid,
            "examples": ["TKT-123", "bd-42", "feature-xyz"],
            "reason": reason,
        }),
    )
}

async fn validate_explicit_thread_id_for_send(
    ctx: &McpContext,
    pool: &mcp_agent_mail_db::DbPool,
    project_id: i64,
    thread_id: &str,
) -> McpResult<()> {
    if !is_valid_thread_id(thread_id) {
        return Err(invalid_thread_id_error(
            thread_id,
            "Thread IDs must start with an alphanumeric character and contain only letters, numbers, '.', '_', or '-' (max 128).",
        ));
    }

    if !is_bare_numeric_thread_id(thread_id) {
        return Ok(());
    }

    let existing_messages = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_thread_messages(
            ctx.cx(),
            pool,
            project_id,
            thread_id,
            Some(1),
        )
        .await,
    )?;
    let has_exact_thread = existing_messages
        .iter()
        .any(|row| row.thread_id.as_deref() == Some(thread_id));
    if !has_exact_thread {
        return Err(invalid_thread_id_error(
            thread_id,
            "Bare numeric IDs are reserved for reply-seeded threads. If you are starting a new thread, use an alphanumeric identifier (e.g. 'TKT-123', 'bd-42', 'feature-xyz').",
        ));
    }

    Ok(())
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
    if sanitized.is_empty() || !sanitized.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        return fallback.to_string();
    }
    sanitized
}

/// Validate per-message size limits before any DB/archive operations.
///
/// Enforces `max_subject_bytes`, `max_message_body_bytes`, `max_attachment_bytes`,
/// and `max_total_message_bytes` from config. A limit of 0 means unlimited.
#[allow(clippy::too_many_lines)]
fn validate_message_size_limits(
    config: &Config,
    subject: &str,
    body_md: &str,
    attachment_paths: Option<&[String]>,
    attachment_base_dir: Option<&Path>,
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
    if let Some(paths) = attachment_paths
        && !paths.is_empty()
    {
        let canonical_attachment_base =
            attachment_base_dir.map(|base_dir| base_dir.canonicalize().ok());
        for path in paths {
            let metadata = match canonical_attachment_base.as_ref() {
                Some(Some(base_dir)) => {
                    // When a project base dir is provided, only size-check resolved
                    // paths accepted by attachment path policy. Invalid paths are
                    // deferred to downstream attachment validation.
                    match mcp_agent_mail_storage::resolve_attachment_source_path_from_canonical_base(
                        base_dir, config, path,
                    ) {
                        Ok(resolved) => std::fs::metadata(resolved),
                        Err(_) => continue,
                    }
                }
                Some(None) => continue,
                None => std::fs::metadata(path),
            };
            if let Ok(meta) = metadata {
                if !meta.is_file() {
                    return Err(legacy_tool_error(
                        "INVALID_ARGUMENT",
                        format!("Attachment path is not a file: {path}"),
                        true,
                        json!({
                            "field": "attachment_paths",
                            "path": path,
                        }),
                    ));
                }
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

fn attachment_size_bytes(meta: &Value) -> Option<u64> {
    meta.get("bytes")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| meta.get("size").and_then(serde_json::Value::as_u64))
        .or_else(|| {
            meta.get("size")
                .and_then(serde_json::Value::as_str)
                .and_then(|raw| raw.parse::<u64>().ok())
        })
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

fn normalized_topic_argument(topic: Option<&str>) -> Option<&str> {
    topic.map(str::trim).filter(|value| !value.is_empty())
}

fn reject_unsupported_topic_argument(topic: Option<&str>, tool_name: &str) -> McpResult<()> {
    let Some(topic_value) = normalized_topic_argument(topic) else {
        return Ok(());
    };
    Err(legacy_tool_error(
        "INVALID_ARGUMENT",
        format!("{tool_name} does not support the 'topic' argument yet. Omit 'topic' and retry."),
        true,
        json!({
            "argument": "topic",
            "value": topic_value,
        }),
    ))
}

const fn has_any_recipients(to: &[String], cc: &[String], bcc: &[String]) -> bool {
    !(to.is_empty() && cc.is_empty() && bcc.is_empty())
}

fn python_json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "int"
            } else {
                "float"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn python_value_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "\\'")),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

#[cfg(test)]
fn send_message_has_explicit_to_recipients(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .any(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())),
        Some(Value::String(s)) => !s.trim().is_empty(),
        _ => false,
    }
}

fn normalize_send_message_to_argument(
    arguments: &mut serde_json::Map<String, Value>,
) -> McpResult<()> {
    let Some(to_value) = arguments.get("to").cloned() else {
        return Ok(());
    };
    match to_value {
        Value::String(s) => {
            arguments.insert("to".to_string(), json!([s]));
            Ok(())
        }
        Value::Array(items) => {
            if let Some(invalid_item) = items.iter().find(|item| !item.is_string()) {
                return Err(legacy_tool_error(
                    "INVALID_ARGUMENT",
                    format!(
                        "Each recipient in 'to' must be a string (agent name). Got: {}",
                        python_json_type_name(invalid_item)
                    ),
                    true,
                    json!({
                        "argument": "to",
                        "invalid_item": python_value_repr(invalid_item),
                    }),
                ));
            }
            Ok(())
        }
        other => Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "'to' must be a list of agent names (e.g., ['BlueLake']) or a single agent name string. Received: {}",
                python_json_type_name(&other)
            ),
            true,
            json!({
                "argument": "to",
                "received_type": python_json_type_name(&other),
            }),
        )),
    }
}

fn normalize_send_message_cc_bcc_argument(
    arguments: &mut serde_json::Map<String, Value>,
    field: &str,
) -> McpResult<()> {
    let Some(value) = arguments.get(field).cloned() else {
        return Ok(());
    };

    match value {
        Value::Null => {
            arguments.remove(field);
            Ok(())
        }
        Value::String(s) => {
            arguments.insert(field.to_string(), json!([s]));
            Ok(())
        }
        Value::Array(items) => {
            if let Some(invalid_item) = items.iter().find(|item| !item.is_string()) {
                return Err(legacy_tool_error(
                    "INVALID_ARGUMENT",
                    format!(
                        "Each recipient in '{field}' must be a string (agent name). Got: {}",
                        python_json_type_name(invalid_item)
                    ),
                    true,
                    json!({
                        "argument": field,
                        "invalid_item": python_value_repr(invalid_item),
                    }),
                ));
            }
            Ok(())
        }
        _ => Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!("{field} must be a list of strings or a single string."),
            true,
            json!({ "argument": field }),
        )),
    }
}

fn normalize_send_message_aliases(arguments: &mut Value) {
    let Some(args) = arguments.as_object_mut() else {
        return;
    };

    if args.contains_key("project_key") {
        let _ = args.remove("project");
        let _ = args.remove("project_slug");
        let _ = args.remove("human_key");
    } else if let Some(val) = args
        .remove("project")
        .or_else(|| args.remove("project_slug"))
        .or_else(|| args.remove("human_key"))
    {
        args.insert("project_key".to_string(), val);
    }

    if args.contains_key("sender_name") {
        let _ = args.remove("from");
        let _ = args.remove("from_agent");
        let _ = args.remove("requester");
    } else if let Some(val) = args
        .remove("from")
        .or_else(|| args.remove("from_agent"))
        .or_else(|| args.remove("requester"))
    {
        args.insert("sender_name".to_string(), val);
    }

    if args.contains_key("message_id") {
        let _ = args.remove("id");
    } else if let Some(val) = args.remove("id") {
        args.insert("message_id".to_string(), val);
    }
}

/// Normalize raw `send_message` / `reply_message` arguments for parity with the
/// Python reference:
/// - accepts common project/sender/message aliases used in messaging flows
/// - accepts single-string forms for to/cc/bcc (converts to one-element arrays)
/// - validates recipient container/item types with parity messages
/// - rejects `broadcast=true` because broadcast messaging is intentionally disabled
pub fn normalize_send_message_arguments(arguments: &mut Value) -> McpResult<()> {
    normalize_send_message_aliases(arguments);

    let Some(args) = arguments.as_object_mut() else {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "Tool arguments must be a JSON object".to_string(),
            true,
            json!({}),
        ));
    };

    if args
        .get("broadcast")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "broadcast=true is intentionally unsupported to prevent agent spam. Address agents explicitly and omit the broadcast flag.",
            true,
            json!({ "argument": "broadcast" }),
        ));
    }

    normalize_send_message_to_argument(args)?;
    normalize_send_message_cc_bcc_argument(args, "cc")?;
    normalize_send_message_cc_bcc_argument(args, "bcc")?;
    Ok(())
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
    project_human_key: &str,
    project_slug: &str,
    recipient_map: &mut HashMap<String, mcp_agent_mail_db::AgentRow>,
    all_recipients: &mut SmallVec<[(i64, String); 8]>,
    resolved_list: &mut SmallVec<[String; 4]>,
) -> McpResult<()> {
    let name = name.trim();
    let name_key = name.to_lowercase();
    let agent = if let Some(existing) = recipient_map.get(&name_key) {
        existing.clone()
    } else {
        let agent = match resolve_or_register_agent(ctx, pool, project_id, name, sender, config)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                // Re-wrap NOT_FOUND as RECIPIENT_NOT_FOUND with Python-parity message.
                if let Some(data) = &e.data
                    && let Some(err_type) = data
                        .as_object()
                        .and_then(|o| o.get("error"))
                        .and_then(|e| e.get("type"))
                        .and_then(|t| t.as_str())
                    && err_type == "NOT_FOUND"
                {
                    let hint = format!(
                        "Use resource://agents/{project_slug} to list registered agents or register new identities."
                    );
                    let message = format!(
                        "Unable to send message — local recipients {name} are not registered in project '{project_human_key}'; {hint}"
                    );
                    return Err(legacy_tool_error(
                        "RECIPIENT_NOT_FOUND",
                        message,
                        true,
                        json!({
                            "unknown_local": [name],
                            "hint": hint,
                        }),
                    ));
                }
                return Err(e);
            }
        };
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
        resolved_list.push(agent.name);
    }
    Ok(())
}

#[allow(dead_code, clippy::too_many_arguments, clippy::too_many_lines)]
fn process_message_attachments(
    config: &Config,
    project_slug: &str,
    project_human_key: &str,
    base_dir: &Path,
    subject: &str,
    body_md: &str,
    attachment_paths: Option<&[String]>,
    do_convert: bool,
    embed_policy: mcp_agent_mail_storage::EmbedPolicy,
) -> McpResult<(String, Vec<serde_json::Value>, Vec<String>)> {
    let attachment_count = attachment_paths.map_or(0, <[String]>::len);
    let mut final_body = body_md.to_string();
    let mut all_attachment_meta: Vec<serde_json::Value> = Vec::with_capacity(attachment_count + 4);
    let mut all_attachment_rel_paths: Vec<String> = Vec::with_capacity(attachment_count + 4);
    let has_explicit_attachments = attachment_paths.is_some_and(|paths| !paths.is_empty());
    let has_local_markdown_images = do_convert
        && mcp_agent_mail_storage::markdown_has_processable_local_images(config, base_dir, body_md);

    if do_convert && (has_explicit_attachments || has_local_markdown_images) {
        match mcp_agent_mail_storage::ensure_archive(config, project_slug) {
            Ok(archive) => {
                let (updated_body, md_meta, rel_paths) =
                    mcp_agent_mail_storage::process_markdown_images(
                        &archive,
                        config,
                        base_dir,
                        body_md,
                        embed_policy,
                    )
                    .map_err(|e| {
                        let (code, message) = match &e {
                            mcp_agent_mail_storage::StorageError::InvalidPath(_) => (
                                "INVALID_ARGUMENT",
                                format!("Invalid Markdown image reference in body: {e}"),
                            ),
                            _ => (
                                "ARCHIVE_ERROR",
                                format!("Failed to process Markdown image references: {e}"),
                            ),
                        };
                        legacy_tool_error(
                            code,
                            message,
                            true,
                            json!({
                                "field": "body_md",
                                "project_slug": project_slug,
                                "project_root": project_human_key,
                            }),
                        )
                    })?;
                final_body = updated_body;
                all_attachment_rel_paths.extend(rel_paths);
                for meta in &md_meta {
                    if let Ok(value) = serde_json::to_value(meta) {
                        all_attachment_meta.push(value);
                    }
                }

                if let Some(paths) = attachment_paths
                    && !paths.is_empty()
                {
                    let (att_meta, rel_paths) = mcp_agent_mail_storage::process_attachments(
                        &archive,
                        config,
                        base_dir,
                        paths,
                        embed_policy,
                    )
                    .map_err(|e| {
                        let (code, message) = match &e {
                            mcp_agent_mail_storage::StorageError::InvalidPath(_) => {
                                ("INVALID_ARGUMENT", format!("Invalid attachment_paths: {e}"))
                            }
                            _ => (
                                "ARCHIVE_ERROR",
                                format!("Failed to process attachment_paths: {e}"),
                            ),
                        };
                        legacy_tool_error(
                            code,
                            message,
                            true,
                            json!({
                                "field": "attachment_paths",
                                "provided": paths,
                            }),
                        )
                    })?;
                    all_attachment_rel_paths.extend(rel_paths);
                    for meta in &att_meta {
                        if let Ok(value) = serde_json::to_value(meta) {
                            all_attachment_meta.push(value);
                        }
                    }
                }
            }
            Err(e) => {
                return Err(legacy_tool_error(
                    "ARCHIVE_ERROR",
                    format!(
                        "Failed to initialize git archive for project '{project_slug}'. This prevents storing attachments or rewriting local Markdown image references: {e}"
                    ),
                    true,
                    json!({
                        "project_slug": project_slug,
                        "project_root": project_human_key,
                        "field": if has_explicit_attachments && !has_local_markdown_images {
                            "attachment_paths"
                        } else {
                            "body_md"
                        },
                    }),
                ));
            }
        }
    } else if let Some(paths) = attachment_paths
        && !paths.is_empty()
    {
        match mcp_agent_mail_storage::ensure_archive(config, project_slug) {
            Ok(archive) => {
                let canonical_base = base_dir.canonicalize().ok();
                for path in paths {
                    let resolved = canonical_base.as_ref().map_or_else(
                        || {
                            mcp_agent_mail_storage::resolve_attachment_source_path(
                                base_dir,
                                config,
                                path,
                            )
                        },
                        |base_dir| {
                            mcp_agent_mail_storage::resolve_attachment_source_path_from_canonical_base(
                                base_dir,
                                config,
                                path,
                            )
                        },
                    )
                    .map_err(|e| {
                        legacy_tool_error(
                            "INVALID_ARGUMENT",
                            format!("Invalid attachment path: {e}"),
                            true,
                            json!({
                                "field": "attachment_paths",
                                "provided": path,
                            }),
                        )
                    })?;

                    let stored = mcp_agent_mail_storage::store_raw_attachment(
                        &archive,
                        &resolved,
                        config.max_attachment_bytes,
                    )
                    .map_err(|e| {
                        let (code, message) = match &e {
                            mcp_agent_mail_storage::StorageError::InvalidPath(_) => {
                                ("INVALID_ARGUMENT", format!("Invalid attachment_paths: {e}"))
                            }
                            _ => (
                                "ARCHIVE_ERROR",
                                format!("Failed to store raw attachment: {e}"),
                            ),
                        };
                        legacy_tool_error(
                            code,
                            message,
                            true,
                            json!({
                                "field": "attachment_paths",
                                "path": path,
                            }),
                        )
                    })?;

                    all_attachment_rel_paths.extend(stored.rel_paths);
                    if let Ok(value) = serde_json::to_value(&stored.meta) {
                        all_attachment_meta.push(value);
                    }
                }
            }
            Err(e) => {
                return Err(legacy_tool_error(
                    "ARCHIVE_ERROR",
                    format!(
                        "Failed to initialize git archive for project '{project_slug}'. This prevents storing attachments: {e}"
                    ),
                    true,
                    json!({
                        "project_slug": project_slug,
                        "project_root": project_human_key,
                    }),
                ));
            }
        }
    }

    if do_convert {
        if config.max_message_body_bytes > 0 && final_body.len() > config.max_message_body_bytes {
            return Err(legacy_tool_error(
                "INVALID_ARGUMENT",
                format!(
                    "Message body exceeds size limit after inlining images: {} bytes > {} byte limit. \
                     Use 'file' attachments policy or reduce image sizes.",
                    final_body.len(),
                    config.max_message_body_bytes,
                ),
                true,
                json!({
                    "field": "body_md",
                    "size_bytes": final_body.len(),
                    "limit_bytes": config.max_message_body_bytes,
                }),
            ));
        }

        if config.max_total_message_bytes > 0 {
            let mut total_size = subject.len().saturating_add(final_body.len());
            for meta in &all_attachment_meta {
                let att_type = meta.get("type").and_then(serde_json::Value::as_str);

                if att_type == Some("inline") && meta.get("data_base64").is_none() {
                    continue;
                }

                if (att_type == Some("file") || att_type == Some("inline"))
                    && let Some(bytes) = attachment_size_bytes(meta)
                {
                    if let Ok(bytes_usize) = usize::try_from(bytes) {
                        let effective_bytes = if att_type == Some("inline") {
                            bytes_usize.saturating_mul(4).saturating_div(3)
                        } else {
                            bytes_usize
                        };
                        total_size = total_size.saturating_add(effective_bytes);
                    } else {
                        total_size = usize::MAX;
                        break;
                    }
                }
            }

            if total_size > config.max_total_message_bytes {
                return Err(legacy_tool_error(
                    "INVALID_ARGUMENT",
                    format!(
                        "Total message size exceeds limit after processing: {} bytes > {} byte limit. \
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
        }
    }

    Ok((final_body, all_attachment_meta, all_attachment_rel_paths))
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
    /// Whether the sender's identity was cryptographically verified via `sender_token`.
    /// - `true`: `sender_token` was provided and matches the agent's `registration_token`.
    /// - `false`: `sender_token` was not provided; message was sent without verification.
    ///
    /// If `sender_token` was provided but mismatched, the call is rejected with an error.
    pub verified_sender: bool,
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
    pub attachments: Vec<serde_json::Value>,
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
    #[serde(default, skip_serializing)]
    pub to: Vec<String>,
    #[serde(default, skip_serializing)]
    pub cc: Vec<String>,
    #[serde(default, skip_serializing)]
    pub bcc: Vec<String>,
    pub created_ts: Option<String>,
    pub kind: String,
    pub attachments: Vec<serde_json::Value>,
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
    pub attachments: Vec<serde_json::Value>,
    pub body_md: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: i64,
    pub deliveries: Vec<DeliveryResult>,
    pub count: usize,
    /// Whether the sender's identity was cryptographically verified via `sender_token`.
    pub verified_sender: bool,
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
/// - `thread_id`: Associate with existing thread (optional; bare numerics must already exist)
/// - `topic`: Reserved for future topic tags; non-blank values are currently rejected
/// - `auto_contact_if_blocked`: Auto-request contact if blocked (optional)
/// - `sender_token`: Registration token for sender identity verification (optional)
#[allow(
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::too_many_lines
)]
#[tool(
    description = "Send a Markdown message to one or more recipients and persist canonical and mailbox copies to Git.\n\nDiscovery\n---------\nTo discover available agent names for recipients, use: resource://agents/{project_key}\nAgent names are NOT the same as program names or user names.\n\nWhat this does\n--------------\n- Stores message (and recipients) in the database; updates sender's activity\n- Writes a canonical `.md` under `messages/YYYY/MM/`\n- Writes sender outbox and per-recipient inbox copies\n- Optionally converts referenced images to WebP and embeds small images inline\n- Supports explicit attachments via `attachment_paths` in addition to inline references\n\nParameters\n----------\nproject_key : str\n    Project identifier (same used with `ensure_project`/`register_agent`).\nsender_name : str\n    Must match an agent registered in the project.\nto : list[str]\n    Primary recipients (agent names). At least one of to/cc/bcc must be non-empty.\nsubject : str\n    Short subject line that will be visible in inbox/outbox and search results.\nbody_md : str\n    GitHub-Flavored Markdown body. Image references can be file paths or data URIs.\ncc, bcc : Optional[list[str]]\n    Additional recipients by name.\nattachment_paths : Optional[list[str]]\n    Extra file paths to include as attachments; will be converted to WebP and stored.\nconvert_images : Optional[bool]\n    Overrides server default for image conversion/inlining. If None, server settings apply.\n    Note: sender attachments_policy \"inline\"/\"file\" always forces conversion/inlining.\nimportance : str\n    One of {\"low\",\"normal\",\"high\",\"urgent\"} (free form tolerated; used by filters).\nack_required : bool\n    If true, recipients should call `acknowledge_message` after reading.\nthread_id : Optional[str]\n    If provided, message will be associated with an existing thread.\nbroadcast : bool\n    Reserved for schema compatibility only. `broadcast=true` is intentionally\n    rejected to prevent agent spam; address agents explicitly instead.\ntopic : Optional[str]\n    Reserved for future topic tags. Non-blank values are currently rejected until\n    topic persistence and filtering are implemented.\nsender_token : Optional[str]\n    Registration token returned by `register_agent`. If provided and valid,\n    the response includes `verified_sender: true`. If provided but mismatched,\n    the call is rejected. If omitted, the message sends but with `verified_sender: false`.\n\nReturns\n-------\ndict\n    {\n      \"deliveries\": [ { \"project\": str, \"payload\": { ... message payload ... } } ],\n      \"count\": int,\n      \"verified_sender\": bool\n    }\n\nEdge cases\n----------\n- If no recipients are given, the call fails.\n- Unknown recipient names fail fast; register them first.\n- Non-absolute attachment paths are resolved relative to the project archive root.\n- `broadcast=true` is intentionally rejected.\n\nDo / Don't\n----------\nDo:\n- Keep subjects concise and specific (aim for \u{2264} 80 characters).\n- Use `thread_id` (or `reply_message`) to keep related discussion in a single thread.\n- Address only relevant recipients; use CC/BCC sparingly and intentionally.\n- Prefer Markdown links; attach images only when they materially aid understanding. The server\n  auto-converts images to WebP and may inline small images depending on policy.\n\nDon't:\n- Send large, repeated binaries\u{2014}reuse prior attachments via `attachment_paths` when possible.\n- Change topics mid-thread\u{2014}start a new thread for a new subject.\n- Broadcast to \"all\" agents unnecessarily\u{2014}target just the agents who need to act.\n\nExamples\n--------\n1) Simple message:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"5\",\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"sender_name\":\"GreenCastle\",\"to\":[\"BlueLake\"],\n  \"subject\":\"Plan for /api/users\",\"body_md\":\"See below.\"\n}}}\n```\n\n2) Inline image (auto-convert to WebP and inline if small):\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"6a\",\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"sender_name\":\"GreenCastle\",\"to\":[\"BlueLake\"],\n  \"subject\":\"Diagram\",\"body_md\":\"![diagram](docs/flow.png)\",\"convert_images\":true\n}}}\n```\n\n3) Explicit attachments:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"6b\",\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"sender_name\":\"GreenCastle\",\"to\":[\"BlueLake\"],\n  \"subject\":\"Screenshots\",\"body_md\":\"Please review.\",\"attachment_paths\":[\"shots/a.png\",\"shots/b.png\"]\n}}}\n```"
)]
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
    topic: Option<String>,
    broadcast: Option<bool>,
    auto_contact_if_blocked: Option<bool>,
    sender_token: Option<String>,
) -> McpResult<String> {
    // Normalize names
    let sender_name =
        mcp_agent_mail_core::models::normalize_agent_name(&sender_name).unwrap_or(sender_name);
    let to: Vec<String> = to
        .into_iter()
        .map(|n| mcp_agent_mail_core::models::normalize_agent_name(&n).unwrap_or(n))
        .collect();
    let cc: Option<Vec<String>> = cc.map(|v| {
        v.into_iter()
            .map(|n| mcp_agent_mail_core::models::normalize_agent_name(&n).unwrap_or(n))
            .collect()
    });
    let bcc: Option<Vec<String>> = bcc.map(|v| {
        v.into_iter()
            .map(|n| mcp_agent_mail_core::models::normalize_agent_name(&n).unwrap_or(n))
            .collect()
    });

    // Truncate subject at 200 chars (parity with Python legacy).
    // Use char_indices to avoid panicking on multi-byte UTF-8 boundaries.
    let subject = if let Some((idx, _)) = subject.char_indices().nth(200) {
        tracing::warn!("Subject exceeds 200 characters; truncating");
        subject[..idx].to_string()
    } else {
        subject
    };

    // Validate importance
    let importance_val =
        importance.map_or_else(|| "normal".to_string(), |s| s.to_ascii_lowercase());
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
    reject_unsupported_topic_argument(topic.as_deref(), "send_message")?;

    let config = &Config::get();

    // ── Per-message size limits (subject/body) before any DB/archive work ──
    validate_message_size_limits(config, &subject, &body_md, None, None)?;

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
    let base_dir = Path::new(&project.human_key);

    if let Some(ref tid) = thread_id {
        validate_explicit_thread_id_for_send(ctx, &pool, project_id, tid).await?;
    }

    // Validate attachment + total sizes with project-relative path resolution.
    validate_message_size_limits(
        config,
        &subject,
        &body_md,
        attachment_paths.as_deref(),
        Some(base_dir),
    )?;

    // Resolve sender
    let sender = resolve_agent(
        ctx,
        &pool,
        project_id,
        &sender_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
    let sender_id = sender.id.unwrap_or(0);

    // ── Sender identity verification (issue #42) ──────────────────────
    //
    // If `sender_token` is provided, validate it against the agent's stored
    // `registration_token` using constant-time comparison.
    // - Match: proceed, set `verified_sender = true` in response.
    // - Mismatch: reject immediately with an error (impersonation attempt).
    // - Not provided: proceed but set `verified_sender = false`.
    let verified_sender = match sender_token.as_deref() {
        Some(token) => {
            if let Some(ref stored_token) = sender.registration_token {
                if mcp_agent_mail_core::setup::constant_time_str_eq(token, stored_token) {
                    true
                } else {
                    return Err(legacy_tool_error(
                        "SENDER_TOKEN_MISMATCH",
                        format!(
                            "sender_token does not match the registered token for agent '{sender_name}'. \
                             Only the agent's owner (the session that called register_agent) can send \
                             messages as this agent. Use the registration_token returned by register_agent."
                        ),
                        false,
                        json!({
                            "sender_name": sender_name,
                            "hint": "Use the registration_token returned by register_agent as sender_token",
                        }),
                    ));
                }
            } else {
                // Agent has no stored token — token was provided but agent never had one.
                // Treat as unverified rather than rejecting (graceful upgrade path).
                false
            }
        }
        None => false,
    };

    let explicitly_targeted = !to.is_empty()
        || cc.as_ref().is_some_and(|v| !v.is_empty())
        || bcc.as_ref().is_some_and(|v| !v.is_empty());

    let broadcast = broadcast.unwrap_or(false);
    if broadcast && explicitly_targeted {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "broadcast=true and explicit 'to' recipients are mutually exclusive. Set broadcast=true with an empty 'to' list, or provide explicit recipients without broadcast.",
            true,
            serde_json::json!({ "argument": "broadcast" }),
        ));
    }
    if broadcast {
        return Err(legacy_tool_error(
            "BROADCAST_DISABLED",
            "Broadcast messaging is intentionally not supported to prevent agent spam. Address agents specifically.",
            true,
            serde_json::json!({ "argument": "broadcast" }),
        ));
    }

    // Self-send detection: warn if sender is sending to themselves (Python parity)
    {
        let sender_lower = sender_name.trim().to_ascii_lowercase();
        let all_named: Vec<&str> = to
            .iter()
            .chain(cc.iter().flatten())
            .chain(bcc.iter().flatten())
            .map(String::as_str)
            .collect();
        if all_named
            .iter()
            .any(|r| r.trim().to_ascii_lowercase() == sender_lower)
        {
            tracing::warn!(
                "[note] You ({sender_name}) are sending a message to yourself. \
                 This is allowed but usually not intended. To communicate with other agents, \
                 use their agent names (e.g., 'BlueLake'). To discover agents, \
                 use resource://agents/{project_key}."
            );
        }
    }

    // Validate recipients
    let cc_list = cc.unwrap_or_default();
    let bcc_list = bcc.unwrap_or_default();

    if !has_any_recipients(&to, &cc_list, &bcc_list) {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "At least one recipient is required. Provide agent names in to, cc, or bcc.",
            true,
            json!({
                "field": "to|cc|bcc",
                "error_detail": "empty recipient list",
            }),
        ));
    }

    // Resolve all recipients (to, cc, bcc) with optional auto-registration
    let total_recip = to.len() + cc_list.len() + bcc_list.len();
    let mut all_recipients: SmallVec<[(i64, String); 8]> = SmallVec::with_capacity(total_recip);
    let mut resolved_to: SmallVec<[String; 4]> = SmallVec::with_capacity(to.len());
    let mut resolved_cc_recipients: SmallVec<[String; 4]> = SmallVec::with_capacity(cc_list.len());
    let mut resolved_bcc_recipients: SmallVec<[String; 4]> =
        SmallVec::with_capacity(bcc_list.len());
    let mut recipient_map: HashMap<String, mcp_agent_mail_db::AgentRow> =
        HashMap::with_capacity(total_recip);
    let mut missing_local: Vec<String> = Vec::new();

    for name in &to {
        if let Err(err) = push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "to",
            &sender,
            config,
            &project.human_key,
            &project.slug,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_to,
        )
        .await
        {
            if let Some(mut names) = extract_recipient_not_found_names(&err) {
                if names.is_empty() {
                    missing_local.push(name.clone());
                } else {
                    missing_local.append(&mut names);
                }
                continue;
            }
            return Err(err);
        }
    }
    for name in &cc_list {
        if let Err(err) = push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "cc",
            &sender,
            config,
            &project.human_key,
            &project.slug,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_cc_recipients,
        )
        .await
        {
            if let Some(mut names) = extract_recipient_not_found_names(&err) {
                if names.is_empty() {
                    missing_local.push(name.clone());
                } else {
                    missing_local.append(&mut names);
                }
                continue;
            }
            return Err(err);
        }
    }
    for name in &bcc_list {
        if let Err(err) = push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "bcc",
            &sender,
            config,
            &project.human_key,
            &project.slug,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_bcc_recipients,
        )
        .await
        {
            if let Some(mut names) = extract_recipient_not_found_names(&err) {
                if names.is_empty() {
                    missing_local.push(name.clone());
                } else {
                    missing_local.append(&mut names);
                }
                continue;
            }
            return Err(err);
        }
    }

    if !missing_local.is_empty() {
        let mut suggestions_map = HashMap::new();
        for name in &missing_local {
            // Attempt resolve to get fuzzy suggestions from the error payload
            if let Err(e) = resolve_agent(
                ctx,
                &pool,
                project_id,
                name,
                &project.slug,
                &project.human_key,
            )
            .await
                && let Some(data) = &e.data
                && let Some(sug) = data
                    .get("error")
                    .and_then(|e| e.get("data"))
                    .and_then(|d| d.get("suggestions"))
                    .and_then(|s| s.as_array())
            {
                suggestions_map.insert(name.clone(), sug.clone());
            }
        }

        return Err(recipient_not_found_error(
            &project.human_key,
            &project.slug,
            &sender,
            &missing_local,
            Some(&suggestions_map),
        ));
    }

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
            patterns_by_agent.entry(res.agent_id).or_default().push(
                (*mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(&res.path_pattern))
                    .clone(),
            );
        }
        if let Some(sender_patterns) = patterns_by_agent.get(&sender_id) {
            for agent in recipient_map.values() {
                if let Some(rec_id) = agent.id
                    && let Some(rec_patterns) = patterns_by_agent.get(&rec_id)
                    && reservations_prove_shared_scope_for_contact(sender_patterns, rec_patterns)
                {
                    auto_ok_names.insert(agent.name.clone());
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

        let mut blocked: Vec<(String, String)> = Vec::new();

        // Check policy for each resolved recipient
        let mut check_policy = |name: &String, kind: &str| {
            if let Some(agent) = recipient_map.get(&name.to_lowercase()) {
                let rec_id = agent.id.unwrap_or(0);
                let recent_ok = auto_ok_names.contains(&agent.name) || recent_set.contains(&rec_id);
                let approved = approved_set.contains(&rec_id);
                match contact_policy_decision(
                    &sender.name,
                    &agent.name,
                    &agent.contact_policy,
                    recent_ok,
                    approved,
                ) {
                    ContactPolicyDecision::Allow => {}
                    ContactPolicyDecision::BlockAll => {
                        // immediate failure for BlockAll
                        return Some(Err(contact_blocked_error()));
                    }
                    ContactPolicyDecision::RequireApproval => {
                        blocked.push((agent.name.clone(), kind.to_string()));
                    }
                }
            }
            None
        };

        for name in &resolved_to {
            if let Some(err) = check_policy(name, "to") {
                return err;
            }
        }
        for name in &resolved_cc_recipients {
            if let Some(err) = check_policy(name, "cc") {
                return err;
            }
        }
        for name in &resolved_bcc_recipients {
            if let Some(err) = check_policy(name, "bcc") {
                return err;
            }
        }

        let mut attempted: Vec<String> = Vec::new();
        if !blocked.is_empty() {
            let effective_auto_contact =
                auto_contact_if_blocked.unwrap_or(config.messaging_auto_handshake_on_block);
            if effective_auto_contact {
                for (name, _) in &blocked {
                    if Box::pin(crate::macros::macro_contact_handshake(
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
                    .await
                    .is_ok()
                    {
                        attempted.push(name.clone());
                    }
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

                // Remove agents who are STILL blocked from the delivery lists.
                // Agents who are now approved remain in the lists (they were added by push_recipient).
                blocked.retain(|(name, _kind)| {
                    if let Some(agent) = recipient_map.get(&name.to_lowercase()) {
                        let rec_id = agent.id.unwrap_or(0);
                        let mut policy = agent.contact_policy.to_lowercase();
                        if !["open", "auto", "contacts_only", "block_all"]
                            .contains(&policy.as_str())
                        {
                            policy = "auto".to_string();
                        }
                        let approved = approved_set.contains(&rec_id);
                        let is_blocked = match policy.as_str() {
                            "open" => false,
                            "auto" | "contacts_only" => !approved,
                            _ => true, // block_all
                        };

                        if !is_blocked {
                            return false; // No longer blocked
                        }
                    }
                    true
                });

                // For any remaining blocked ones, remove them from the active lists so the
                // final DB insert and archive write don't include them.
                if !blocked.is_empty() {
                    let still_blocked_names: HashSet<String> =
                        blocked.iter().map(|(n, _)| n.clone()).collect();
                    resolved_to.retain(|n| !still_blocked_names.contains(n));
                    resolved_cc_recipients.retain(|n| !still_blocked_names.contains(n));
                    resolved_bcc_recipients.retain(|n| !still_blocked_names.contains(n));

                    let still_blocked_ids: HashSet<i64> = still_blocked_names
                        .iter()
                        .filter_map(|n| recipient_map.get(&n.to_lowercase()).and_then(|a| a.id))
                        .collect();
                    all_recipients.retain(|(id, _)| !still_blocked_ids.contains(id));
                }
            }
        }

        if !blocked.is_empty() {
            let blocked_names: Vec<String> = blocked.into_iter().map(|(n, _)| n).collect();
            return Err(contact_required_error(
                &project.human_key,
                &sender.name,
                &blocked_names,
                &attempted,
                ttl_seconds,
            ));
        }
    }

    let (final_body, all_attachment_meta, all_attachment_rel_paths) = process_message_attachments(
        config,
        &project.slug,
        &project.human_key,
        base_dir,
        &subject,
        &body_md,
        attachment_paths.as_deref(),
        do_convert,
        embed_policy,
    )?;

    // Serialize processed attachment metadata as JSON array
    let attachments_json = serde_json::to_string(&all_attachment_meta).unwrap_or_else(|e| {
        tracing::warn!(
            "attachment metadata serialization failed, falling back to empty array: {e}"
        );
        "[]".to_string()
    });

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
    enqueue_message_semantic_index(project_id, message_id, &message.subject, &message.body_md);
    enqueue_message_lexical_index(&mcp_agent_mail_db::search_v3::IndexableMessage {
        id: message_id,
        project_id,
        project_slug: project.slug.clone(),
        sender_name: sender.name.clone(),
        subject: message.subject.clone(),
        body_md: message.body_md.clone(),
        thread_id: thread_id.clone(),
        importance: message.importance.clone(),
        created_ts: message.created_ts,
    });

    // Emit notification signals for to/cc recipients only (never bcc).
    //
    // IMPORTANT: These must be synchronous so that the `.signal` file exists
    // immediately when `send_message` returns (conformance parity with legacy
    // Python implementation + fixture tests).
    let notification_meta = mcp_agent_mail_storage::NotificationMessage {
        id: Some(message_id),
        from: Some(sender.name.clone()),
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

        // Deduplicate recipient names for archive write (avoid duplicate inbox writes)
        all_recipient_names.sort_unstable();
        all_recipient_names.dedup();

        let msg_json = serde_json::json!({
            "id": message_id,
            "from": &sender.name,
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
            &sender.name,
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
        attachments: all_attachment_meta,
        from: sender.name.clone(),
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
        verified_sender,
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
/// - `sender_token`: Registration token for sender identity verification (optional)
#[allow(
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::too_many_lines
)]
#[tool(
    description = "Reply to an existing message, preserving or establishing a thread.\n\nBehavior\n--------\n- Inherits original `importance` and `ack_required` flags unless overridden\n- `thread_id` is taken from the original message if present; otherwise, the original id is used\n- Subject is prefixed with `subject_prefix` if not already present\n- Defaults `to` to the original sender if not explicitly provided\n\nParameters\n----------\nproject_key : str\n    Project identifier.\nmessage_id : int\n    The id of the message you are replying to.\nsender_name : str\n    Your agent name (must be registered in the project).\nbody_md : str\n    Reply body in Markdown.\nto, cc, bcc : Optional[list[str]]\n    Recipients by agent name. If omitted, `to` defaults to original sender.\nsubject_prefix : str\n    Prefix to apply (default \"Re:\"). Case-insensitive idempotent.\nimportance : Optional[str]\n    Override importance level {\"low\",\"normal\",\"high\",\"urgent\"}. Inherits from original if omitted.\nack_required : Optional[bool]\n    Override acknowledgement requirement. Inherits from original if omitted.\nsender_token : Optional[str]\n    Registration token for identity verification.\n\nDo / Don't\n----------\nDo:\n- Keep the subject focused; avoid topic drift within a thread.\n- Reply to the original sender unless new stakeholders are strictly required.\n- Preserve importance/ack flags from the original unless there is a clear reason to change.\n- Use CC for FYI only; BCC sparingly and with intention.\n\nDon't:\n- Change `thread_id` when continuing the same discussion.\n- Escalate to many recipients; prefer targeted replies and start a new thread for new topics.\n- Attach large binaries in replies unless essential; reference prior attachments where possible.\n\nReturns\n-------\ndict\n    Message payload including `thread_id` and `reply_to`.\n\nExamples\n--------\nMinimal reply to original sender:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"6\",\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"message_id\":1234,\"sender_name\":\"BlueLake\",\n  \"body_md\":\"Questions about the migration plan...\"\n}}}\n```\n\nReply with explicit recipients and CC:\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"6c\",\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"message_id\":1234,\"sender_name\":\"BlueLake\",\n  \"body_md\":\"Looping ops.\",\"to\":[\"GreenCastle\"],\"cc\":[\"RedCat\"],\"subject_prefix\":\"RE:\"\n}}}\n```"
)]
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
    importance: Option<String>,
    ack_required: Option<bool>,
    attachment_paths: Option<Vec<String>>,
    convert_images: Option<bool>,
    sender_token: Option<String>,
) -> McpResult<String> {
    // Normalize names
    let sender_name =
        mcp_agent_mail_core::models::normalize_agent_name(&sender_name).unwrap_or(sender_name);
    let to: Option<Vec<String>> = to.map(|v| {
        v.into_iter()
            .map(|n| mcp_agent_mail_core::models::normalize_agent_name(&n).unwrap_or(n))
            .collect()
    });
    let cc: Option<Vec<String>> = cc.map(|v| {
        v.into_iter()
            .map(|n| mcp_agent_mail_core::models::normalize_agent_name(&n).unwrap_or(n))
            .collect()
    });
    let bcc: Option<Vec<String>> = bcc.map(|v| {
        v.into_iter()
            .map(|n| mcp_agent_mail_core::models::normalize_agent_name(&n).unwrap_or(n))
            .collect()
    });

    let prefix = subject_prefix.unwrap_or_else(|| "Re:".to_string());
    let config = &Config::get();

    // Validate importance override if provided (resolved to final value after
    // the original message is fetched below).
    if let Some(ref imp) = importance {
        let lower = imp.to_ascii_lowercase();
        if !["low", "normal", "high", "urgent"].contains(&lower.as_str()) {
            return Err(legacy_tool_error(
                "INVALID_ARGUMENT",
                format!(
                    "Invalid argument value: importance='{imp}'. \
                     Must be: low, normal, high, or urgent."
                ),
                true,
                json!({
                    "field": "importance",
                    "error_detail": imp,
                }),
            ));
        }
    }

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

    // Resolve importance: use override if provided, otherwise inherit from original.
    let importance_val = if let Some(ref imp) = importance {
        imp.to_ascii_lowercase()
    } else {
        original.importance.clone()
    };

    // Resolve sender
    let sender = resolve_agent(
        ctx,
        &pool,
        project_id,
        &sender_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
    let sender_id = sender.id.unwrap_or(0);

    // ── Sender identity verification (issue #42) ──────────────────────
    let verified_sender = match sender_token.as_deref() {
        Some(token) => {
            if let Some(ref stored_token) = sender.registration_token {
                if mcp_agent_mail_core::setup::constant_time_str_eq(token, stored_token) {
                    true
                } else {
                    return Err(legacy_tool_error(
                        "SENDER_TOKEN_MISMATCH",
                        format!(
                            "sender_token does not match the registered token for agent '{sender_name}'. \
                             Only the agent's owner (the session that called register_agent) can send \
                             messages as this agent. Use the registration_token returned by register_agent."
                        ),
                        false,
                        serde_json::json!({
                            "sender_name": sender_name,
                            "hint": "Use the registration_token returned by register_agent as sender_token",
                        }),
                    ));
                }
            } else {
                false
            }
        }
        None => false,
    };

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
    // Check for prefix followed by a space to avoid false positives like "Regarding"
    let prefix_with_space = format!("{prefix} ");
    let subject = if original
        .subject
        .to_ascii_lowercase()
        .starts_with(&prefix_with_space.to_ascii_lowercase())
        || original.subject.eq_ignore_ascii_case(&prefix)
    {
        original.subject.clone()
    } else {
        format!("{prefix} {}", original.subject)
    };
    // Truncate subject at 200 chars (parity with Python legacy).
    // Use char_indices to avoid panicking on multi-byte UTF-8 boundaries.
    let subject = if let Some((idx, _)) = subject.char_indices().nth(200) {
        tracing::warn!("Subject exceeds 200 characters; truncating");
        subject[..idx].to_string()
    } else {
        subject
    };

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

    let (final_body, all_attachment_meta, all_attachment_rel_paths) = process_message_attachments(
        config,
        &project.slug,
        &project.human_key,
        Path::new(&project.human_key),
        &subject,
        &body_md,
        attachment_paths.as_deref(),
        do_convert,
        embed_policy,
    )?;

    // Serialize processed attachment metadata as JSON array
    let attachments_json = serde_json::to_string(&all_attachment_meta).unwrap_or_else(|e| {
        tracing::error!(
            "attachment metadata serialization failed, falling back to empty array: {e}"
        );
        "[]".to_string()
    });

    // ── Per-message size limits (subject/total) ──
    if config.max_subject_bytes > 0 && subject.len() > config.max_subject_bytes {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "Reply subject exceeds size limit: {} bytes > {} byte limit. Shorten the subject.",
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

    if config.max_total_message_bytes > 0 {
        let total_size = subject.len().saturating_add(final_body.len());
        if total_size > config.max_total_message_bytes {
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
    }

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

    // Resolve all recipients with auto-registration and deduplication
    let total_recip = to_names.len() + cc_names.len() + bcc_names.len();
    let mut all_recipients: SmallVec<[(i64, String); 8]> = SmallVec::with_capacity(total_recip);
    let mut resolved_to: SmallVec<[String; 4]> = SmallVec::with_capacity(to_names.len());
    let mut resolved_cc_recipients: SmallVec<[String; 4]> = SmallVec::with_capacity(cc_names.len());
    let mut resolved_bcc_recipients: SmallVec<[String; 4]> =
        SmallVec::with_capacity(bcc_names.len());
    let mut recipient_map: HashMap<String, mcp_agent_mail_db::AgentRow> =
        HashMap::with_capacity(total_recip);
    let mut missing_local: Vec<String> = Vec::new();

    for name in &to_names {
        if let Err(err) = push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "to",
            &sender,
            config,
            &project.human_key,
            &project.slug,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_to,
        )
        .await
        {
            if let Some(mut names) = extract_recipient_not_found_names(&err) {
                if names.is_empty() {
                    missing_local.push(name.clone());
                } else {
                    missing_local.append(&mut names);
                }
                continue;
            }
            return Err(err);
        }
    }
    for name in &cc_names {
        if let Err(err) = push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "cc",
            &sender,
            config,
            &project.human_key,
            &project.slug,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_cc_recipients,
        )
        .await
        {
            if let Some(mut names) = extract_recipient_not_found_names(&err) {
                if names.is_empty() {
                    missing_local.push(name.clone());
                } else {
                    missing_local.append(&mut names);
                }
                continue;
            }
            return Err(err);
        }
    }
    for name in &bcc_names {
        if let Err(err) = push_recipient(
            ctx,
            &pool,
            project_id,
            name,
            "bcc",
            &sender,
            config,
            &project.human_key,
            &project.slug,
            &mut recipient_map,
            &mut all_recipients,
            &mut resolved_bcc_recipients,
        )
        .await
        {
            if let Some(mut names) = extract_recipient_not_found_names(&err) {
                if names.is_empty() {
                    missing_local.push(name.clone());
                } else {
                    missing_local.append(&mut names);
                }
                continue;
            }
            return Err(err);
        }
    }

    if !missing_local.is_empty() {
        let mut suggestions_map = HashMap::new();
        for name in &missing_local {
            // Attempt resolve to get fuzzy suggestions from the error payload
            if let Err(e) = resolve_agent(
                ctx,
                &pool,
                project_id,
                name,
                &project.slug,
                &project.human_key,
            )
            .await
                && let Some(data) = &e.data
                && let Some(sug) = data
                    .get("error")
                    .and_then(|e| e.get("data"))
                    .and_then(|d| d.get("suggestions"))
                    .and_then(|s| s.as_array())
            {
                suggestions_map.insert(name.clone(), sug.clone());
            }
        }

        return Err(recipient_not_found_error(
            &project.human_key,
            &project.slug,
            &sender,
            &missing_local,
            Some(&suggestions_map),
        ));
    }

    if config.contact_enforcement_enabled {
        let mut auto_ok_names: HashSet<String> = HashSet::new();

        if !thread_id.is_empty() {
            let thread_rows = db_outcome_to_mcp_result(
                mcp_agent_mail_db::queries::list_thread_messages(
                    ctx.cx(),
                    &pool,
                    project_id,
                    &thread_id,
                    Some(500),
                )
                .await,
            )
            .unwrap_or_else(|e| {
                tracing::warn!("contact enforcement: list_thread_messages failed (fail-open): {e}");
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
            patterns_by_agent.entry(res.agent_id).or_default().push(
                (*mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(&res.path_pattern))
                    .clone(),
            );
        }
        if let Some(sender_patterns) = patterns_by_agent.get(&sender_id) {
            for agent in recipient_map.values() {
                if let Some(rec_id) = agent.id
                    && let Some(rec_patterns) = patterns_by_agent.get(&rec_id)
                    && reservations_prove_shared_scope_for_contact(sender_patterns, rec_patterns)
                {
                    auto_ok_names.insert(agent.name.clone());
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
        for name in resolved_to
            .iter()
            .chain(resolved_cc_recipients.iter())
            .chain(resolved_bcc_recipients.iter())
        {
            let Some(agent) = recipient_map.get(&name.to_lowercase()) else {
                continue;
            };
            let rec_id = agent.id.unwrap_or(0);
            let recent_ok = auto_ok_names.contains(&agent.name) || recent_set.contains(&rec_id);
            let approved = approved_set.contains(&rec_id);
            match contact_policy_decision(
                &sender.name,
                &agent.name,
                &agent.contact_policy,
                recent_ok,
                approved,
            ) {
                ContactPolicyDecision::Allow => {}
                ContactPolicyDecision::BlockAll => return Err(contact_blocked_error()),
                ContactPolicyDecision::RequireApproval => blocked.push(agent.name.clone()),
            }
        }

        let mut attempted: Vec<String> = Vec::new();
        if !blocked.is_empty() && config.messaging_auto_handshake_on_block {
            for name in &blocked {
                if Box::pin(crate::macros::macro_contact_handshake(
                    ctx,
                    project.human_key.clone(),
                    Some(sender.name.clone()),
                    Some(name.clone()),
                    None,
                    None,
                    None,
                    Some("auto-handshake by reply_message".to_string()),
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
                .await
                .is_ok()
                {
                    attempted.push(name.clone());
                }
            }

            // Re-check contact approval after handshake attempts (mirrors send_message)
            if !attempted.is_empty() {
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
                        if (policy == "auto" || policy == "contacts_only") && approved {
                            return false;
                        }
                    }
                    true
                });
            }
        }

        if !blocked.is_empty() {
            return Err(contact_required_error(
                &project.human_key,
                &sender.name,
                &blocked,
                &attempted,
                ttl_seconds,
            ));
        }
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
            &final_body,
            Some(&thread_id),
            &importance_val,
            ack_required.unwrap_or(original.ack_required != 0),
            &attachments_json,
            &recipient_refs,
        )
        .await,
    )?;

    let reply_id = reply.id.unwrap_or(0);
    enqueue_message_semantic_index(project_id, reply_id, &reply.subject, &reply.body_md);
    enqueue_message_lexical_index(&mcp_agent_mail_db::search_v3::IndexableMessage {
        id: reply_id,
        project_id,
        project_slug: project.slug.clone(),
        sender_name: sender.name.clone(),
        subject: reply.subject.clone(),
        body_md: reply.body_md.clone(),
        thread_id: Some(thread_id.clone()),
        importance: reply.importance.clone(),
        created_ts: reply.created_ts,
    });

    // Emit notification signals for to/cc recipients only (never bcc).
    // Mirrors the send_message notification logic for parity with Python.
    let notification_meta = mcp_agent_mail_storage::NotificationMessage {
        id: Some(reply_id),
        from: Some(sender.name.clone()),
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

        // Deduplicate recipient names for archive write (avoid duplicate inbox writes)
        all_recipient_names.sort_unstable();
        all_recipient_names.dedup();

        let msg_json = serde_json::json!({
            "id": reply_id,
            "from": &sender.name,
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
            "attachments": &all_attachment_meta,
            "reply_to": message_id,
        });
        try_write_message_archive(
            config,
            &project.slug,
            &msg_json,
            &reply.body_md,
            &sender.name,
            &all_recipient_names,
            &all_attachment_rel_paths,
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
        attachments: all_attachment_meta.clone(),
        from: sender.name.clone(),
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
        attachments: all_attachment_meta,
        body_md: reply.body_md,
        from: sender.name.clone(),
        to: resolved_to.into_vec(),
        cc: resolved_cc_recipients.into_vec(),
        bcc: resolved_bcc_recipients.into_vec(),
        reply_to: message_id,
        deliveries: vec![DeliveryResult {
            project: project.human_key.clone(),
            payload,
        }],
        count: 1,
        verified_sender,
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
/// - `topic`: Reserved for future topic filtering; non-blank values are currently rejected
#[allow(
    clippy::items_after_statements,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
#[tool(
    description = "Retrieve recent messages for an agent without mutating read/ack state.\n\nFilters\n-------\n- `urgent_only`: only messages with importance in {high, urgent}\n- `since_ts`: ISO-8601 timestamp string; messages strictly newer than this are returned\n- `limit`: max number of messages (default 20)\n- `include_bodies`: include full Markdown bodies in the payloads\n- `topic`: reserved for future topic filtering; non-blank values are currently rejected\n\nUsage patterns\n--------------\n- Poll after each editing step in an agent loop to pick up coordination messages.\n- Use `since_ts` with the timestamp from your last poll for efficient incremental fetches.\n- Combine with `acknowledge_message` if `ack_required` is true.\n\nReturns\n-------\nlist[dict]\n    Each message includes: { id, subject, from, created_ts, importance, ack_required, kind, [body_md] }\n\nExample\n-------\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"7\",\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"agent_name\":\"BlueLake\",\"since_ts\":\"2025-10-23T00:00:00+00:00\"\n}}}\n```"
)]
pub async fn fetch_inbox(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    urgent_only: Option<bool>,
    since_ts: Option<String>,
    limit: Option<i32>,
    include_bodies: Option<bool>,
    topic: Option<String>,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);
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
    reject_unsupported_topic_argument(topic.as_deref(), "fetch_inbox")?;

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
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

    #[derive(serde::Deserialize, Default)]
    struct FastRecipients {
        #[serde(default)]
        to: Vec<String>,
        #[serde(default)]
        cc: Vec<String>,
        #[serde(default)]
        bcc: Vec<String>,
    }

    let messages: Vec<InboxMessage> = inbox_rows
        .into_iter()
        .map(|row| {
            let attachments: Vec<serde_json::Value> =
                serde_json::from_str(&row.message.attachments).unwrap_or_default();
            let recipients: FastRecipients =
                serde_json::from_str(&row.message.recipients_json).unwrap_or_default();

            let to = recipients.to;
            let cc = recipients.cc;
            let bcc = if row.message.sender_id == agent_id {
                recipients.bcc
            } else {
                Vec::new()
            };

            InboxMessage {
                id: row.message.id.unwrap_or(0),
                project_id: row.message.project_id,
                sender_id: row.message.sender_id,
                thread_id: row.message.thread_id,
                subject: row.message.subject,
                importance: row.message.importance,
                ack_required: row.message.ack_required != 0,
                from: row.sender_name,
                to,
                cc,
                bcc,
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

    // Auto-mark fetched messages as read (and auto-ack if ack_required=1).
    // Agents consistently fail to call mark_message_read explicitly, so
    // fetching IS reading.  Best-effort: errors are logged but don't fail
    // the fetch.
    //
    // Uses batch UPDATE (single transaction) instead of N individual
    // mark_message_read calls — ~80% latency reduction for typical 20-message
    // inbox fetches.
    {
        let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
        if let Err(e) = mcp_agent_mail_db::sync::mark_messages_read_batch_sync(
            pool.sqlite_path(),
            agent_id,
            &ids,
        ) {
            tracing::warn!(
                agent_id = agent_id,
                count = ids.len(),
                error = %e,
                "batch auto-mark-read on fetch_inbox failed"
            );
        } else {
            mcp_agent_mail_db::read_cache()
                .invalidate_inbox_stats_scoped(pool.sqlite_path(), agent_id);
        }
    }

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
#[tool(
    description = "Mark a specific message as read for the given agent.\n\nNotes\n-----\n- Read receipts are per-recipient; this only affects the specified agent.\n- This does not send an acknowledgement; use `acknowledge_message` for that.\n- Safe to call multiple times; later calls return the original timestamp.\n\nIdempotency\n-----------\n- If `mark_message_read` has already been called earlier for the same (agent, message),\n  the original timestamp is returned and no error is raised.\n\nReturns\n-------\ndict\n    { message_id, read: bool, read_at: iso8601 | null }\n\nExample\n-------\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"8\",\"method\":\"tools/call\",\"params\":{\"name\":\"mark_message_read\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"agent_name\":\"BlueLake\",\"message_id\":1234\n}}}\n```"
)]
pub async fn mark_message_read(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    message_id: i64,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
    let agent_id = agent.id.unwrap_or(0);

    // Authorization note: agent_id is globally unique (auto-increment across
    // all projects), so the DB query `WHERE agent_id = ? AND message_id = ?`
    // implicitly scopes to the correct project. An agent in project A cannot
    // match a message_recipients row for project B because the agent_ids differ.
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
#[tool(
    description = "Acknowledge a message addressed to an agent (and mark as read).\n\nBehavior\n--------\n- Sets both read_ts and ack_ts for the (agent, message) pairing\n- Safe to call multiple times; subsequent calls will return the prior timestamps\n\nIdempotency\n-----------\n- If acknowledgement already exists, the previous timestamps are preserved and returned.\n\nWhen to use\n-----------\n- Respond to messages with `ack_required=true` to signal explicit receipt.\n- Agents can treat an acknowledgement as a lightweight, non-textual reply.\n\nReturns\n-------\ndict\n    { message_id, acknowledged: bool, acknowledged_at: iso8601 | null, read_at: iso8601 | null }\n\nExample\n-------\n```json\n{\"jsonrpc\":\"2.0\",\"id\":\"9\",\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\n  \"project_key\":\"/abs/path/backend\",\"agent_name\":\"BlueLake\",\"message_id\":1234\n}}}\n```"
)]
pub async fn acknowledge_message(
    ctx: &McpContext,
    project_key: String,
    agent_name: String,
    message_id: i64,
) -> McpResult<String> {
    let agent_name =
        mcp_agent_mail_core::models::normalize_agent_name(&agent_name).unwrap_or(agent_name);

    let pool = get_db_pool()?;
    let project = resolve_project(ctx, &pool, &project_key).await?;
    let project_id = project.id.unwrap_or(0);

    let agent = resolve_agent(
        ctx,
        &pool,
        project_id,
        &agent_name,
        &project.slug,
        &project.human_key,
    )
    .await?;
    let agent_id = agent.id.unwrap_or(0);

    // Authorization note: agent_id is globally unique (auto-increment), so
    // the DB query implicitly scopes to the correct project. See mark_message_read.
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
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::{Cx, Outcome};
    use fastmcp::prelude::McpContext;
    use mcp_agent_mail_db::{AgentRow, DbPool, DbPoolConfig, ProjectRow, queries};

    static MESSAGING_THREAD_ID_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn run_thread_validation_test<F, Fut>(db_name: &str, f: F)
    where
        F: FnOnce(Cx, DbPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = MESSAGING_THREAD_ID_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("messaging test tempdir");
        let db_path = dir.path().join(db_name);
        let cfg = DbPoolConfig {
            database_url: format!("sqlite://{}", db_path.display()),
            ..DbPoolConfig::default()
        };
        let pool = DbPool::new(&cfg).expect("messaging test pool");
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx, pool));
    }

    async fn ensure_project_row(cx: &Cx, pool: &DbPool, human_key: &str) -> ProjectRow {
        match queries::ensure_project(cx, pool, human_key).await {
            Outcome::Ok(project) => project,
            other => panic!("ensure_project({human_key}) failed: {other:?}"),
        }
    }

    async fn register_agent_row(cx: &Cx, pool: &DbPool, project_id: i64, name: &str) -> AgentRow {
        match queries::register_agent(
            cx,
            pool,
            project_id,
            name,
            "codex-cli",
            "gpt-5",
            Some("messaging thread-id test"),
            None,
            None,
        )
        .await
        {
            Outcome::Ok(agent) => agent,
            other => panic!("register_agent({name}, None) failed: {other:?}"),
        }
    }

    #[test]
    fn agent_unique_constraint_error_detection_matches_expected_sqlite_text() {
        assert!(is_agent_unique_constraint_error(
            "UNIQUE constraint failed: agents.project_id, agents.name"
        ));
        assert!(is_agent_unique_constraint_error(
            "unique constraint failed: AGENTS.PROJECT_ID, AGENTS.NAME"
        ));
        assert!(is_agent_unique_constraint_error(
            "UNIQUE constraint failed: project_id, name"
        ));
        assert!(!is_agent_unique_constraint_error(
            "UNIQUE constraint failed: project_id, project_name"
        ));
        assert!(!is_agent_unique_constraint_error(
            "UNIQUE constraint failed: projects.slug"
        ));
    }

    #[test]
    fn contact_blocked_error_message_parity() {
        let err = contact_blocked_error();
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(err.message, "Recipient is not accepting messages.");
    }

    #[test]
    fn contact_blocked_error_payload_has_no_data_field() {
        let err = contact_blocked_error();
        let payload = err
            .data
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .and_then(|root| root.get("error"))
            .and_then(serde_json::Value::as_object)
            .expect("CONTACT_BLOCKED payload should include error object");

        assert_eq!(
            payload.get("type").and_then(serde_json::Value::as_str),
            Some("CONTACT_BLOCKED")
        );
        assert_eq!(
            payload.get("message").and_then(serde_json::Value::as_str),
            Some("Recipient is not accepting messages.")
        );
        assert_eq!(
            payload
                .get("recoverable")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert!(
            !payload.contains_key("data"),
            "CONTACT_BLOCKED parity requires no data payload"
        );
    }

    #[test]
    fn contact_required_error_message_parity() {
        let blocked = vec!["BlueLake".to_string(), "RedCat".to_string()];
        let attempted = vec!["BlueLake".to_string()];
        let err = contact_required_error("/tmp/project", "GreenCastle", &blocked, &attempted, 3600);
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "Contact approval required for recipients: BlueLake, RedCat. Before retrying, request approval with `request_contact(project_key='/tmp/project', from_agent='GreenCastle', to_agent='BlueLake')` or run `macro_contact_handshake(project_key='/tmp/project', requester='GreenCastle', target='BlueLake', auto_accept=True)`. Alternatively, send your message inside a recent thread that already includes them by reusing its thread_id. Automatic handshake attempts already ran for: BlueLake; wait for approval or retry the suggested calls explicitly."
        );
    }

    #[test]
    fn contact_required_error_payload_parity() {
        let blocked = vec!["Zulu".to_string(), "Alpha".to_string(), "Zulu".to_string()];
        let attempted = vec!["Zulu".to_string()];
        let err = contact_required_error("/tmp/project", "GreenCastle", &blocked, &attempted, 1800);

        let data = err
            .data
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .and_then(|root| root.get("error"))
            .and_then(serde_json::Value::as_object)
            .and_then(|payload| payload.get("data"))
            .and_then(serde_json::Value::as_object)
            .expect("CONTACT_REQUIRED payload should include error.data object");

        assert_eq!(
            data.get("recipients_blocked"),
            Some(&serde_json::json!(["Alpha", "Zulu"]))
        );
        assert_eq!(
            data.get("remedies"),
            Some(&serde_json::json!([
                "Call request_contact(project_key, from_agent, to_agent) to request approval",
                "Call macro_contact_handshake(project_key, requester, target, auto_accept=true) to automate"
            ]))
        );
        assert_eq!(
            data.get("auto_contact_attempted"),
            Some(&serde_json::json!(["Zulu"]))
        );

        let calls = data
            .get("suggested_tool_calls")
            .and_then(serde_json::Value::as_array)
            .expect("CONTACT_REQUIRED payload should include suggested_tool_calls array");
        assert_eq!(
            calls[0],
            serde_json::json!({
                "tool": "macro_contact_handshake",
                "arguments": {
                    "project_key": "/tmp/project",
                    "requester": "GreenCastle",
                    "target": "Alpha",
                    "auto_accept": true,
                    "ttl_seconds": 1800,
                }
            })
        );
        assert_eq!(
            calls[1],
            serde_json::json!({
                "tool": "request_contact",
                "arguments": {
                    "project_key": "/tmp/project",
                    "from_agent": "GreenCastle",
                    "to_agent": "Alpha",
                    "ttl_seconds": 1800,
                }
            })
        );
        assert_eq!(
            calls[2],
            serde_json::json!({
                "tool": "request_contact",
                "arguments": {
                    "project_key": "/tmp/project",
                    "from_agent": "GreenCastle",
                    "to_agent": "Zulu",
                    "ttl_seconds": 1800,
                }
            })
        );
        assert_eq!(calls.len(), 3);
    }

    #[test]
    fn extract_recipient_not_found_names_parses_payload() {
        let err = legacy_tool_error(
            "RECIPIENT_NOT_FOUND",
            "test",
            true,
            json!({
                "unknown_local": ["Zulu", "Alpha"],
            }),
        );
        let names = extract_recipient_not_found_names(&err).expect("should parse names");
        assert_eq!(names, vec!["Zulu", "Alpha"]);
    }

    #[test]
    fn extract_recipient_not_found_names_ignores_other_error_types() {
        let err = legacy_tool_error(
            "NOT_FOUND",
            "test",
            true,
            json!({
                "unknown_local": ["Zulu"],
            }),
        );
        assert!(extract_recipient_not_found_names(&err).is_none());
    }

    #[test]
    fn recipient_not_found_error_sorts_names_and_includes_suggestions() {
        let sender = mcp_agent_mail_db::AgentRow {
            name: "BlueLake".to_string(),
            program: "codex-cli".to_string(),
            model: "gpt-5".to_string(),
            task_description: "test task".to_string(),
            ..mcp_agent_mail_db::AgentRow::default()
        };
        let err = recipient_not_found_error(
            "/tmp/proj",
            "proj-slug",
            &sender,
            &["Zulu".to_string(), "Alpha".to_string(), "Zulu".to_string()],
            None,
        );

        assert_eq!(
            err.message,
            "Unable to send message — local recipients Alpha, Zulu are not registered in project '/tmp/proj'; Use resource://agents/proj-slug to list registered agents or register new identities."
        );

        let data = err
            .data
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .and_then(|root| root.get("error"))
            .and_then(serde_json::Value::as_object)
            .and_then(|payload| payload.get("data"))
            .and_then(serde_json::Value::as_object)
            .expect("RECIPIENT_NOT_FOUND payload should include error.data object");
        assert_eq!(
            data.get("unknown_local"),
            Some(&serde_json::json!(["Alpha", "Zulu"]))
        );
        assert_eq!(
            data.get("hint"),
            Some(&serde_json::json!(
                "Use resource://agents/proj-slug to list registered agents or register new identities."
            ))
        );
        let calls = data
            .get("suggested_tool_calls")
            .and_then(serde_json::Value::as_array)
            .expect("suggested_tool_calls should be present");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            serde_json::json!({
                "tool": "register_agent",
                "arguments": {
                    "project_key": "/tmp/proj",
                    "name": "Alpha",
                    "program": "codex-cli",
                    "model": "gpt-5",
                    "task_description": "test task",
                },
            })
        );
    }

    #[test]
    fn contact_policy_decision_self_allowed_even_if_block_all() {
        assert_eq!(
            contact_policy_decision("AgentA", "AgentA", "block_all", false, false),
            ContactPolicyDecision::Allow
        );
    }

    #[test]
    fn contact_policy_decision_block_all_overrides_recent_and_approved() {
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "block_all", true, true),
            ContactPolicyDecision::BlockAll
        );
    }

    #[test]
    fn contact_policy_decision_auto_recent_then_link() {
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "auto", true, false),
            ContactPolicyDecision::Allow
        );
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "auto", false, true),
            ContactPolicyDecision::Allow
        );
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "auto", false, false),
            ContactPolicyDecision::RequireApproval
        );
    }

    #[test]
    fn contact_policy_decision_contacts_only_requires_link() {
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "contacts_only", true, false),
            ContactPolicyDecision::RequireApproval
        );
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "contacts_only", false, true),
            ContactPolicyDecision::Allow
        );
    }

    #[test]
    fn contact_policy_decision_invalid_policy_defaults_to_auto() {
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "unexpected_policy", true, false),
            ContactPolicyDecision::Allow
        );
        assert_eq!(
            contact_policy_decision("AgentA", "AgentB", "unexpected_policy", false, false),
            ContactPolicyDecision::RequireApproval
        );
    }

    #[test]
    fn reservations_prove_shared_scope_for_contact_rejects_ambiguous_glob_pairs() {
        let sender_patterns = [CompiledPattern::new("src/*.rs")];
        let recipient_patterns = [CompiledPattern::new("src/*.txt")];
        assert!(!reservations_prove_shared_scope_for_contact(
            &sender_patterns,
            &recipient_patterns
        ));
    }

    #[test]
    fn reservations_prove_shared_scope_for_contact_allows_equal_globs() {
        let sender_patterns = [CompiledPattern::new("src/**")];
        let recipient_patterns = [CompiledPattern::new("src/**")];
        assert!(reservations_prove_shared_scope_for_contact(
            &sender_patterns,
            &recipient_patterns
        ));
    }

    #[test]
    fn reservations_prove_shared_scope_for_contact_rejects_equal_invalid_globs() {
        let sender_patterns = [CompiledPattern::new("[abc")];
        let recipient_patterns = [CompiledPattern::new(" [abc ")];
        assert!(!reservations_prove_shared_scope_for_contact(
            &sender_patterns,
            &recipient_patterns
        ));
    }

    #[test]
    fn reservations_prove_shared_scope_for_contact_allows_exact_directory_and_glob() {
        let sender_patterns = [CompiledPattern::new("src")];
        let recipient_patterns = [CompiledPattern::new("src/**/*.rs")];
        assert!(reservations_prove_shared_scope_for_contact(
            &sender_patterns,
            &recipient_patterns
        ));
    }

    #[test]
    fn normalize_send_message_arguments_converts_single_string_recipient_forms() {
        let mut args = json!({
            "to": "BlueLake",
            "cc": "RedCat",
            "bcc": "GoldHawk",
        });
        normalize_send_message_arguments(&mut args).expect("should normalize single-string forms");
        assert_eq!(args["to"], json!(["BlueLake"]));
        assert_eq!(args["cc"], json!(["RedCat"]));
        assert_eq!(args["bcc"], json!(["GoldHawk"]));
    }

    #[test]
    fn normalize_send_message_arguments_does_not_rewrite_non_message_agent_fields() {
        let mut args = json!({
            "project": "/tmp/project",
            "from_agent": "BlueLake",
            "id": 42,
            "target": "RedPeak",
            "to_agent": "RedPeak",
        });
        normalize_send_message_arguments(&mut args).expect("message aliases should normalize");
        assert_eq!(args["project_key"], json!("/tmp/project"));
        assert_eq!(args["sender_name"], json!("BlueLake"));
        assert_eq!(args["message_id"], json!(42));
        assert_eq!(args["target"], json!("RedPeak"));
        assert_eq!(args["to_agent"], json!("RedPeak"));
        assert!(args.get("agent_name").is_none());
    }

    #[test]
    fn normalize_send_message_arguments_rejects_broadcast_with_explicit_to() {
        let mut args = json!({
            "broadcast": true,
            "to": ["BlueLake"],
        });
        let err = normalize_send_message_arguments(&mut args)
            .expect_err("broadcast with explicit to should fail");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "broadcast=true is intentionally unsupported to prevent agent spam. Address agents explicitly and omit the broadcast flag."
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "broadcast");
    }

    #[test]
    fn normalize_send_message_arguments_rejects_non_list_to() {
        let mut args = json!({ "to": 123 });
        let err =
            normalize_send_message_arguments(&mut args).expect_err("numeric to should be rejected");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "'to' must be a list of agent names (e.g., ['BlueLake']) or a single agent name string. Received: int"
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "to");
        assert_eq!(data["error"]["data"]["received_type"], "int");
    }

    #[test]
    fn normalize_send_message_arguments_rejects_non_string_to_items() {
        let mut args = json!({ "to": [42] });
        let err = normalize_send_message_arguments(&mut args)
            .expect_err("non-string to item should be rejected");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "Each recipient in 'to' must be a string (agent name). Got: int"
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "to");
        assert_eq!(data["error"]["data"]["invalid_item"], "42");
    }

    #[test]
    fn normalize_send_message_arguments_rejects_non_list_cc() {
        let mut args = json!({
            "to": ["BlueLake"],
            "cc": 123,
        });
        let err = normalize_send_message_arguments(&mut args).expect_err("non-list cc should fail");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "cc must be a list of strings or a single string."
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "cc");
    }

    #[test]
    fn normalize_send_message_arguments_rejects_non_list_bcc() {
        let mut args = json!({
            "to": ["BlueLake"],
            "bcc": 123,
        });
        let err =
            normalize_send_message_arguments(&mut args).expect_err("non-list bcc should fail");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "bcc must be a list of strings or a single string."
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "bcc");
    }

    #[test]
    fn normalize_send_message_arguments_rejects_non_string_cc_items() {
        let mut args = json!({
            "to": ["BlueLake"],
            "cc": ["RedCat", 7],
        });
        let err = normalize_send_message_arguments(&mut args)
            .expect_err("non-string cc item should fail");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "Each recipient in 'cc' must be a string (agent name). Got: int"
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "cc");
    }

    #[test]
    fn normalize_send_message_arguments_rejects_non_string_bcc_items() {
        let mut args = json!({
            "to": ["BlueLake"],
            "bcc": ["GoldHawk", false],
        });
        let err = normalize_send_message_arguments(&mut args)
            .expect_err("non-string bcc item should fail");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "Each recipient in 'bcc' must be a string (agent name). Got: bool"
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "bcc");
    }

    #[test]
    fn normalized_topic_argument_treats_blank_as_absent() {
        assert_eq!(normalized_topic_argument(Some("   ")), None);
        assert_eq!(normalized_topic_argument(None), None);
    }

    #[test]
    fn normalized_topic_argument_trims_non_blank_topic() {
        assert_eq!(
            normalized_topic_argument(Some("  build-updates  ")),
            Some("build-updates")
        );
    }

    #[test]
    fn reject_unsupported_topic_argument_allows_blank_topic() {
        reject_unsupported_topic_argument(Some("   "), "send_message")
            .expect("blank topic should behave like an omitted topic");
    }

    #[test]
    fn reject_unsupported_topic_argument_rejects_non_blank_topic() {
        let err = reject_unsupported_topic_argument(Some("build-updates"), "fetch_inbox")
            .expect_err("non-blank topic should be rejected until implemented");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "fetch_inbox does not support the 'topic' argument yet. Omit 'topic' and retry."
        );
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert_eq!(data["error"]["data"]["argument"], "topic");
        assert_eq!(data["error"]["data"]["value"], "build-updates");
    }

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

    #[test]
    fn bare_numeric_thread_id_detected() {
        assert!(is_bare_numeric_thread_id("42"));
        assert!(!is_bare_numeric_thread_id("123-abc"));
    }

    #[test]
    fn invalid_thread_id_error_mentions_examples() {
        let err = invalid_thread_id_error("bad id", "reason text");
        assert!(err.message.contains("reason text"));
        assert!(err.message.contains("TKT-123"));
    }

    #[test]
    fn validate_explicit_thread_id_for_send_allows_existing_numeric_thread() {
        run_thread_validation_test(
            "messaging_numeric_thread_exists.db",
            |cx, pool| async move {
                let project =
                    ensure_project_row(&cx, &pool, "/tmp/am-msg-thread-id-existing").await;
                let project_id = project.id.expect("project id");
                let sender = register_agent_row(&cx, &pool, project_id, "BlueLake").await;
                let recipient = register_agent_row(&cx, &pool, project_id, "RedPeak").await;
                let root_message = match queries::create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    sender.id.expect("sender id"),
                    "seed",
                    "body",
                    None,
                    "normal",
                    false,
                    "[]",
                    &[(recipient.id.expect("recipient id"), "to")],
                )
                .await
                {
                    Outcome::Ok(message) => message,
                    other => panic!("create_message_with_recipients failed: {other:?}"),
                };
                let numeric_thread_id = root_message.id.expect("message id").to_string();
                match queries::create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    sender.id.expect("sender id"),
                    "reply",
                    "body",
                    Some(&numeric_thread_id),
                    "normal",
                    false,
                    "[]",
                    &[(recipient.id.expect("recipient id"), "to")],
                )
                .await
                {
                    Outcome::Ok(_) => {}
                    other => panic!("create_message_with_recipients reply failed: {other:?}"),
                }

                let ctx = McpContext::new(cx.clone(), 1);
                validate_explicit_thread_id_for_send(&ctx, &pool, project_id, &numeric_thread_id)
                    .await
                    .expect("existing numeric thread id should be allowed");
            },
        );
    }

    #[test]
    fn validate_explicit_thread_id_for_send_rejects_unknown_numeric_thread() {
        run_thread_validation_test(
            "messaging_numeric_thread_missing.db",
            |cx, pool| async move {
                let project = ensure_project_row(&cx, &pool, "/tmp/am-msg-thread-id-missing").await;
                let project_id = project.id.expect("project id");
                let ctx = McpContext::new(cx.clone(), 1);
                let err = validate_explicit_thread_id_for_send(&ctx, &pool, project_id, "424242")
                    .await
                    .expect_err("unknown numeric thread id should be rejected");
                assert!(err.message.contains("reserved for reply-seeded thread"));
            },
        );
    }

    #[test]
    fn validate_explicit_thread_id_for_send_rejects_root_message_without_reply_seed() {
        run_thread_validation_test("messaging_numeric_root_only.db", |cx, pool| async move {
            let project = ensure_project_row(&cx, &pool, "/tmp/am-msg-thread-id-root-only").await;
            let project_id = project.id.expect("project id");
            let sender = register_agent_row(&cx, &pool, project_id, "BlueLake").await;
            let recipient = register_agent_row(&cx, &pool, project_id, "RedPeak").await;
            let root_message = match queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "seed",
                "body",
                None,
                "normal",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            {
                Outcome::Ok(message) => message,
                other => panic!("create_message_with_recipients failed: {other:?}"),
            };

            let ctx = McpContext::new(cx.clone(), 1);
            let err = validate_explicit_thread_id_for_send(
                &ctx,
                &pool,
                project_id,
                &root_message.id.expect("message id").to_string(),
            )
            .await
            .expect_err("root message id without replies should be rejected");
            assert!(err.message.contains("reserved for reply-seeded thread"));
        });
    }

    #[test]
    fn validate_explicit_thread_id_for_send_rejects_leading_zero_numeric_variant() {
        run_thread_validation_test("messaging_numeric_leading_zero.db", |cx, pool| async move {
            let project =
                ensure_project_row(&cx, &pool, "/tmp/am-msg-thread-id-leading-zero").await;
            let project_id = project.id.expect("project id");
            let sender = register_agent_row(&cx, &pool, project_id, "BlueLake").await;
            let recipient = register_agent_row(&cx, &pool, project_id, "RedPeak").await;
            let root_message = match queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "seed",
                "body",
                None,
                "normal",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            {
                Outcome::Ok(message) => message,
                other => panic!("create_message_with_recipients failed: {other:?}"),
            };
            let numeric_thread_id = root_message.id.expect("message id").to_string();
            match queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "reply",
                "body",
                Some(&numeric_thread_id),
                "normal",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            {
                Outcome::Ok(_) => {}
                other => panic!("create_message_with_recipients reply failed: {other:?}"),
            }

            let ctx = McpContext::new(cx.clone(), 1);
            let leading_zero_thread_id = format!("0{numeric_thread_id}");
            let err = validate_explicit_thread_id_for_send(
                &ctx,
                &pool,
                project_id,
                &leading_zero_thread_id,
            )
            .await
            .expect_err("leading-zero numeric variant should be rejected");
            assert!(err.message.contains("reserved for reply-seeded thread"));
        });
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
        if let Some((idx, _)) = subject.char_indices().nth(200) {
            subject[..idx].to_string()
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
        if original_subject.len() >= prefix.len()
            && original_subject.is_char_boundary(prefix.len())
            && original_subject[..prefix.len()].eq_ignore_ascii_case(&prefix)
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
            verified_sender: false,
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
            to: vec![],
            cc: vec![],
            bcc: vec![],
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
            to: vec![],
            cc: vec![],
            bcc: vec![],
            created_ts: Some("2026-02-06T00:00:00Z".into()),
            kind: "to".into(),
            attachments: vec![json!({"path": "img.webp", "type": "file"})],
            body_md: Some("Hello world".into()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(json["body_md"], "Hello world");
        assert_eq!(json["ack_required"], true);
        assert_eq!(json["thread_id"], "thread-1");
        assert_eq!(json["attachments"][0]["path"], "img.webp");
    }

    #[test]
    fn inbox_message_deserializes_missing_recipient_lists_as_empty() {
        let json = json!({
            "id": 1,
            "project_id": 1,
            "sender_id": 1,
            "thread_id": "thread-1",
            "subject": "test",
            "importance": "normal",
            "ack_required": false,
            "from": "BlueLake",
            "created_ts": "2026-02-06T00:00:00Z",
            "kind": "to",
            "attachments": [],
        });

        let parsed: InboxMessage = serde_json::from_value(json).expect("deserialize inbox message");
        assert!(parsed.to.is_empty());
        assert!(parsed.cc.is_empty());
        assert!(parsed.bcc.is_empty());
    }

    #[test]
    fn inbox_message_omits_recipient_lists_when_serialized() {
        let r = InboxMessage {
            id: 1,
            project_id: 1,
            sender_id: 1,
            thread_id: Some("thread-1".into()),
            subject: "test".into(),
            importance: "normal".into(),
            ack_required: false,
            from: "BlueLake".into(),
            to: vec!["RedFox".into()],
            cc: vec!["GoldHawk".into()],
            bcc: vec!["SilverPeak".into()],
            created_ts: Some("2026-02-06T00:00:00Z".into()),
            kind: "to".into(),
            attachments: vec![],
            body_md: None,
        };

        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(json.get("to").is_none());
        assert!(json.get("cc").is_none());
        assert!(json.get("bcc").is_none());
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
            attachments: vec![json!({"path": "file.webp"})],
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
        assert_eq!(json["attachments"][0]["path"], "file.webp");
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
            verified_sender: false,
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
    fn attachment_size_bytes_accepts_size_aliases() {
        let numeric = serde_json::json!({"size": 128});
        let stringly = serde_json::json!({"size": "256"});
        let bytes = serde_json::json!({"bytes": 512});

        assert_eq!(attachment_size_bytes(&numeric), Some(128));
        assert_eq!(attachment_size_bytes(&stringly), Some(256));
        assert_eq!(attachment_size_bytes(&bytes), Some(512));
    }

    #[test]
    fn size_limits_pass_when_under() {
        let cfg = config_with_limits(1024, 1024, 2048, 256);
        let result = validate_message_size_limits(&cfg, "Hello", "Body text", None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_pass_when_zero_unlimited() {
        let cfg = config_with_limits(0, 0, 0, 0);
        let big = "x".repeat(10_000_000);
        let result = validate_message_size_limits(&cfg, &big, &big, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_reject_oversized_subject() {
        let cfg = config_with_limits(0, 0, 0, 10);
        let subject = "A".repeat(11);
        let result = validate_message_size_limits(&cfg, &subject, "", None, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("subject") || err.to_string().contains("Subject"));
    }

    #[test]
    fn size_limits_accept_exact_subject() {
        let cfg = config_with_limits(0, 0, 0, 10);
        let subject = "A".repeat(10);
        let result = validate_message_size_limits(&cfg, &subject, "", None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_reject_oversized_body() {
        let cfg = config_with_limits(100, 0, 0, 0);
        let body = "B".repeat(101);
        let result = validate_message_size_limits(&cfg, "", &body, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_accept_exact_body() {
        let cfg = config_with_limits(100, 0, 0, 0);
        let body = "B".repeat(100);
        let result = validate_message_size_limits(&cfg, "", &body, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_reject_total_overflow() {
        // Subject + body exceed total even though each is within individual limits
        let cfg = config_with_limits(100, 0, 50, 100);
        let result = validate_message_size_limits(&cfg, "sub", &"x".repeat(50), None, None);
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
        let result = validate_message_size_limits(&cfg, "", "", Some(&paths), None);
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_accept_small_attachment() {
        let cfg = config_with_limits(0, 100, 0, 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "hello").unwrap();
        let paths = vec![path.to_string_lossy().to_string()];
        let result = validate_message_size_limits(&cfg, "", "", Some(&paths), None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_resolve_relative_attachment_from_project_base_dir() {
        let cfg = config_with_limits(0, 10, 0, 0);
        let project_dir = tempfile::tempdir().unwrap();
        let attachments_dir = project_dir.path().join("attachments");
        std::fs::create_dir_all(&attachments_dir).unwrap();
        std::fs::write(attachments_dir.join("big.txt"), "x".repeat(20)).unwrap();

        let paths = vec!["attachments/big.txt".to_string()];
        let result =
            validate_message_size_limits(&cfg, "", "", Some(&paths), Some(project_dir.path()));
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_skip_disallowed_absolute_paths_when_base_dir_provided() {
        let mut cfg = config_with_limits(0, 10, 0, 0);
        cfg.allow_absolute_attachment_paths = false;

        let project_dir = tempfile::tempdir().unwrap();
        let external_dir = tempfile::tempdir().unwrap();
        let external_file = external_dir.path().join("big.txt");
        std::fs::write(&external_file, "x".repeat(20)).unwrap();

        let paths = vec![external_file.to_string_lossy().to_string()];
        // Disallowed absolute paths should be validated by downstream
        // attachment resolution, not by this size pre-check.
        let result =
            validate_message_size_limits(&cfg, "", "", Some(&paths), Some(project_dir.path()));
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_skip_attachment_checks_when_project_base_dir_is_missing() {
        let cfg = config_with_limits(0, 10, 0, 0);
        let missing_project_dir = tempfile::tempdir().unwrap();
        let missing_base = missing_project_dir.path().join("missing");

        let external_dir = tempfile::tempdir().unwrap();
        let external_file = external_dir.path().join("big.txt");
        std::fs::write(&external_file, "x".repeat(20)).unwrap();

        let paths = vec![external_file.to_string_lossy().to_string()];
        let result = validate_message_size_limits(&cfg, "", "", Some(&paths), Some(&missing_base));
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
        let result = validate_message_size_limits(&cfg, "", &"y".repeat(10), Some(&paths), None);
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_nonexistent_attachment_skipped() {
        let cfg = config_with_limits(0, 10, 0, 0);
        let paths = vec!["/nonexistent/file.txt".to_string()];
        // Non-existent files are skipped (downstream handles the error)
        let result = validate_message_size_limits(&cfg, "", "", Some(&paths), None);
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
        let result = validate_message_size_limits(&cfg, "", &"z".repeat(25), Some(&paths), None);
        assert!(result.is_err());
    }

    #[test]
    fn size_limits_empty_subject_and_body_pass() {
        let cfg = config_with_limits(1, 1, 1, 1);
        // Empty strings have length 0 which is ≤ any positive limit
        let result = validate_message_size_limits(&cfg, "", "", None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn size_limits_error_message_contains_field_info() {
        let cfg = config_with_limits(10, 0, 0, 0);
        let err = validate_message_size_limits(&cfg, "", &"x".repeat(20), None, None).unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("body") || err_str.contains("Body"),
            "Error should mention body field: {err_str}"
        );
    }

    #[test]
    fn size_limits_subject_error_mentions_subject() {
        let cfg = config_with_limits(0, 0, 0, 5);
        let err = validate_message_size_limits(&cfg, "toolong", "", None, None).unwrap_err();
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
        let result = validate_message_size_limits(&cfg, "hello", &"x".repeat(10), None, None);
        assert!(result.is_ok());

        // Now with total limit = 10, subject(5) + body(10) = 15 > 10 via saturating_add
        let cfg2 = config_with_limits(0, 0, 10, 0);
        let result = validate_message_size_limits(&cfg2, "hello", &"x".repeat(10), None, None);
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

    #[test]
    fn process_message_attachments_rejects_invalid_markdown_image() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            storage_root: tmp.path().join("storage"),
            ..Config::default()
        };

        let broken = project_dir.path().join("broken.png");
        std::fs::write(&broken, b"not an image").unwrap();

        let err = process_message_attachments(
            &cfg,
            "bad-markdown-image",
            project_dir.path().to_str().unwrap(),
            project_dir.path(),
            "Subject",
            "Broken image: ![broken](broken.png)",
            None,
            true,
            mcp_agent_mail_storage::EmbedPolicy::File,
        )
        .expect_err("invalid markdown image should fail");

        let err_str = err.to_string();
        assert!(
            err_str.contains("Markdown image reference"),
            "expected markdown-image validation error, got {err_str}"
        );
    }

    #[test]
    fn process_message_attachments_rejects_archive_failure_for_local_markdown_image() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let bad_storage_root = tmp.path().join("storage-root-file");
        std::fs::write(&bad_storage_root, b"not a directory").unwrap();
        cfg.storage_root = bad_storage_root;

        let image = project_dir.path().join("ok.png");
        let png = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x08,
            0x1d, 0x63, 0xf8, 0xff, 0xff, 0xff, 0x7f, 0x00, 0x09, 0xfb, 0x03, 0xfd, 0x2a, 0x86,
            0xe3, 0x8a, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&image, png).unwrap();

        let err = process_message_attachments(
            &cfg,
            "archive-init-failure",
            project_dir.path().to_str().unwrap(),
            project_dir.path(),
            "Subject",
            "Needs rewrite: ![ok](ok.png)",
            None,
            true,
            mcp_agent_mail_storage::EmbedPolicy::File,
        )
        .expect_err("archive init failure for local markdown images should fail");

        let err_str = err.to_string();
        assert!(
            err_str.contains("rewriting local Markdown image references"),
            "expected archive-init markdown error, got {err_str}"
        );
    }

    #[test]
    fn process_message_attachments_skips_archive_init_for_remote_markdown_images() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let bad_storage_root = tmp.path().join("storage-root-file");
        std::fs::write(&bad_storage_root, b"not a directory").unwrap();
        cfg.storage_root = bad_storage_root;

        let body = "Remote image: ![ok](https://example.com/ok.png)";
        let (final_body, attachment_meta, rel_paths) = process_message_attachments(
            &cfg,
            "remote-markdown-image",
            project_dir.path().to_str().unwrap(),
            project_dir.path(),
            "Subject",
            body,
            None,
            true,
            mcp_agent_mail_storage::EmbedPolicy::File,
        )
        .expect("remote markdown images should not require archive init");

        assert_eq!(final_body, body);
        assert!(attachment_meta.is_empty());
        assert!(rel_paths.is_empty());
    }

    #[test]
    fn process_message_attachments_ignores_empty_attachment_list() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let bad_storage_root = tmp.path().join("storage-root-file");
        std::fs::write(&bad_storage_root, b"not a directory").unwrap();
        cfg.storage_root = bad_storage_root;

        let body = "No attachments";
        let empty_paths: Vec<String> = Vec::new();
        let (final_body, attachment_meta, rel_paths) = process_message_attachments(
            &cfg,
            "empty-attachment-list",
            project_dir.path().to_str().unwrap(),
            project_dir.path(),
            "Subject",
            body,
            Some(&empty_paths),
            false,
            mcp_agent_mail_storage::EmbedPolicy::File,
        )
        .expect("empty attachment list should be treated as no attachments");

        assert_eq!(final_body, body);
        assert!(attachment_meta.is_empty());
        assert!(rel_paths.is_empty());
    }

    #[test]
    fn process_message_attachments_rejects_empty_raw_attachment_as_invalid_argument() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            storage_root: tmp.path().join("storage"),
            ..Config::default()
        };

        let empty = project_dir.path().join("empty.bin");
        std::fs::write(&empty, []).unwrap();
        let attachment_paths = vec!["empty.bin".to_string()];

        let err = process_message_attachments(
            &cfg,
            "empty-raw-attachment",
            project_dir.path().to_str().unwrap(),
            project_dir.path(),
            "Subject",
            "Body",
            Some(&attachment_paths),
            false,
            mcp_agent_mail_storage::EmbedPolicy::File,
        )
        .expect_err("empty raw attachment should fail");

        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        let data = err.data.expect("error payload");
        assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
        assert!(
            err.message.contains("Attachment file is empty"),
            "expected empty-file validation message, got {}",
            err.message
        );
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

    // -----------------------------------------------------------------------
    // Edge case: Unicode subject truncation with "Re:" prefix
    // -----------------------------------------------------------------------

    #[test]
    fn prefix_plus_unicode_subject_truncated_at_char_boundary() {
        // 198 CJK chars (each 3 bytes UTF-8, 1 char). After "Re: " prefix (4 chars)
        // the result is 202 chars, which should be truncated to 200.
        let base: String = "\u{4e16}".repeat(198); // 世 repeated 198 times
        assert_eq!(base.chars().count(), 198);

        let prefixed = apply_prefix(&base, "Re:");
        assert_eq!(prefixed, format!("Re: {base}"));
        assert_eq!(prefixed.chars().count(), 202);

        let truncated = truncate_subject(&prefixed);
        assert_eq!(truncated.chars().count(), 200);
        assert!(truncated.starts_with("Re: "));
        // Verify valid UTF-8 (implicit — it's a String — but also check boundary)
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn prefix_plus_mixed_multibyte_truncated_safely() {
        // Mix of emoji (4 bytes), CJK (3 bytes), and ASCII (1 byte).
        // Build exactly 199 chars, then prefix pushes it to 203.
        let segment = "\u{1F600}\u{4e16}a"; // 😀世a = 3 chars, 8 bytes
        let repeats = 67; // 67 * 3 = 201 chars, take first 199
        let base: String = segment.repeat(repeats).chars().take(199).collect();
        assert_eq!(base.chars().count(), 199);

        let prefixed = apply_prefix(&base, "Re:");
        assert_eq!(prefixed.chars().count(), 203);

        let truncated = truncate_subject(&prefixed);
        assert_eq!(truncated.chars().count(), 200);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn prefix_on_exactly_200_char_unicode_subject_truncates() {
        // Subject is exactly 200 emoji chars. "Re: " prefix adds 4 -> 204 -> must truncate.
        let base: String = "\u{1F389}".repeat(200); // 🎉 × 200
        assert_eq!(base.chars().count(), 200);

        let prefixed = apply_prefix(&base, "Re:");
        assert_eq!(prefixed.chars().count(), 204);

        let truncated = truncate_subject(&prefixed);
        assert_eq!(truncated.chars().count(), 200);
        assert!(truncated.starts_with("Re: "));
    }

    #[test]
    fn prefix_on_short_unicode_subject_no_truncation() {
        let base = "\u{1F600}\u{4e16}\u{1F389}"; // 😀世🎉 = 3 chars
        let prefixed = apply_prefix(base, "Re:");
        assert_eq!(prefixed, format!("Re: {base}"));
        assert_eq!(prefixed.chars().count(), 7); // "Re: " (4) + 3 = 7

        let truncated = truncate_subject(&prefixed);
        assert_eq!(truncated, prefixed, "short subject should not be truncated");
    }

    // ── python_json_type_name tests ─────────────────────────────────

    #[test]
    fn python_type_name_null() {
        assert_eq!(python_json_type_name(&Value::Null), "NoneType");
    }

    #[test]
    fn python_type_name_bool() {
        assert_eq!(python_json_type_name(&json!(true)), "bool");
        assert_eq!(python_json_type_name(&json!(false)), "bool");
    }

    #[test]
    fn python_type_name_int() {
        assert_eq!(python_json_type_name(&json!(42)), "int");
        assert_eq!(python_json_type_name(&json!(-1)), "int");
        assert_eq!(python_json_type_name(&json!(0)), "int");
    }

    #[test]
    fn python_type_name_float() {
        assert_eq!(python_json_type_name(&json!(3.15_f64)), "float");
        assert_eq!(python_json_type_name(&json!(0.0)), "float");
    }

    #[test]
    fn python_type_name_string() {
        assert_eq!(python_json_type_name(&json!("hello")), "str");
        assert_eq!(python_json_type_name(&json!("")), "str");
    }

    #[test]
    fn python_type_name_list_and_dict() {
        assert_eq!(python_json_type_name(&json!([1, 2])), "list");
        assert_eq!(python_json_type_name(&json!([])), "list");
        assert_eq!(python_json_type_name(&json!({"k": "v"})), "dict");
        assert_eq!(python_json_type_name(&json!({})), "dict");
    }

    // ── python_value_repr tests ─────────────────────────────────────

    #[test]
    fn python_repr_null() {
        assert_eq!(python_value_repr(&Value::Null), "None");
    }

    #[test]
    fn python_repr_booleans() {
        assert_eq!(python_value_repr(&json!(true)), "True");
        assert_eq!(python_value_repr(&json!(false)), "False");
    }

    #[test]
    fn python_repr_numbers() {
        assert_eq!(python_value_repr(&json!(42)), "42");
        assert_eq!(python_value_repr(&json!(3.15_f64)), "3.15");
    }

    #[test]
    fn python_repr_string_with_single_quotes() {
        assert_eq!(python_value_repr(&json!("hello")), "'hello'");
        assert_eq!(
            python_value_repr(&json!("it's")),
            "'it\\'s'",
            "single quotes should be escaped"
        );
    }

    #[test]
    fn python_repr_empty_string() {
        assert_eq!(python_value_repr(&json!("")), "''");
    }

    #[test]
    fn python_repr_array_and_object_use_json() {
        let arr = json!([1, "two"]);
        let repr = python_value_repr(&arr);
        assert!(repr.contains('['), "array repr should use JSON format");

        let obj = json!({"key": "val"});
        let repr = python_value_repr(&obj);
        assert!(repr.contains('{'), "object repr should use JSON format");
    }

    // ── send_message_has_explicit_to_recipients tests ───────────────

    #[test]
    fn explicit_to_none() {
        assert!(!send_message_has_explicit_to_recipients(None));
    }

    #[test]
    fn explicit_to_empty_string() {
        let val = json!("");
        assert!(!send_message_has_explicit_to_recipients(Some(&val)));
    }

    #[test]
    fn explicit_to_whitespace_only_string() {
        let val = json!("   ");
        assert!(!send_message_has_explicit_to_recipients(Some(&val)));
    }

    #[test]
    fn explicit_to_valid_string() {
        let val = json!("BlueLake");
        assert!(send_message_has_explicit_to_recipients(Some(&val)));
    }

    #[test]
    fn explicit_to_empty_array() {
        let val = json!([]);
        assert!(!send_message_has_explicit_to_recipients(Some(&val)));
    }

    #[test]
    fn explicit_to_array_with_empty_strings() {
        let val = json!(["", "  "]);
        assert!(!send_message_has_explicit_to_recipients(Some(&val)));
    }

    #[test]
    fn explicit_to_array_with_valid_name() {
        let val = json!(["BlueLake", ""]);
        assert!(send_message_has_explicit_to_recipients(Some(&val)));
    }

    #[test]
    fn explicit_to_non_string_value() {
        let val = json!(42);
        assert!(!send_message_has_explicit_to_recipients(Some(&val)));
    }

    #[test]
    fn explicit_to_null_value() {
        assert!(!send_message_has_explicit_to_recipients(Some(&Value::Null)));
    }

    // ── normalize_send_message_to_argument tests ────────────────────

    #[test]
    fn normalize_to_string_wraps_in_array() {
        let mut args = serde_json::Map::new();
        args.insert("to".to_string(), json!("BlueLake"));
        normalize_send_message_to_argument(&mut args).unwrap();
        assert_eq!(args.get("to"), Some(&json!(["BlueLake"])));
    }

    #[test]
    fn normalize_to_valid_array_is_noop() {
        let mut args = serde_json::Map::new();
        args.insert("to".to_string(), json!(["BlueLake", "RedPeak"]));
        normalize_send_message_to_argument(&mut args).unwrap();
        assert_eq!(args.get("to"), Some(&json!(["BlueLake", "RedPeak"])));
    }

    #[test]
    fn normalize_to_absent_is_noop() {
        let mut args = serde_json::Map::new();
        args.insert("subject".to_string(), json!("Test"));
        normalize_send_message_to_argument(&mut args).unwrap();
        assert!(!args.contains_key("to"));
    }

    #[test]
    fn normalize_to_array_with_non_string_errors() {
        let mut args = serde_json::Map::new();
        args.insert("to".to_string(), json!(["BlueLake", 42]));
        let result = normalize_send_message_to_argument(&mut args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("must be a string"), "{}", err.message);
    }

    #[test]
    fn normalize_to_number_errors_with_type_name() {
        let mut args = serde_json::Map::new();
        args.insert("to".to_string(), json!(42));
        let result = normalize_send_message_to_argument(&mut args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("list of agent names"),
            "{}",
            err.message
        );
    }

    // ── normalize_send_message_cc_bcc_argument tests ────────────────

    #[test]
    fn normalize_cc_null_removes_field() {
        let mut args = serde_json::Map::new();
        args.insert("cc".to_string(), Value::Null);
        normalize_send_message_cc_bcc_argument(&mut args, "cc").unwrap();
        assert!(!args.contains_key("cc"));
    }

    #[test]
    fn normalize_bcc_null_removes_field() {
        let mut args = serde_json::Map::new();
        args.insert("bcc".to_string(), Value::Null);
        normalize_send_message_cc_bcc_argument(&mut args, "bcc").unwrap();
        assert!(!args.contains_key("bcc"));
    }

    #[test]
    fn normalize_cc_string_wraps_in_array() {
        let mut args = serde_json::Map::new();
        args.insert("cc".to_string(), json!("Agent"));
        normalize_send_message_cc_bcc_argument(&mut args, "cc").unwrap();
        assert_eq!(args.get("cc"), Some(&json!(["Agent"])));
    }

    #[test]
    fn normalize_bcc_valid_array_is_noop() {
        let mut args = serde_json::Map::new();
        args.insert("bcc".to_string(), json!(["A", "B"]));
        normalize_send_message_cc_bcc_argument(&mut args, "bcc").unwrap();
        assert_eq!(args.get("bcc"), Some(&json!(["A", "B"])));
    }

    #[test]
    fn normalize_cc_array_with_non_string_errors() {
        let mut args = serde_json::Map::new();
        args.insert("cc".to_string(), json!(["Agent", true]));
        let result = normalize_send_message_cc_bcc_argument(&mut args, "cc");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("must be a string"), "{}", err.message);
    }

    #[test]
    fn normalize_bcc_absent_is_noop() {
        let mut args = serde_json::Map::new();
        normalize_send_message_cc_bcc_argument(&mut args, "bcc").unwrap();
        assert!(!args.contains_key("bcc"));
    }

    #[test]
    fn normalize_cc_object_errors() {
        let mut args = serde_json::Map::new();
        args.insert("cc".to_string(), json!({"name": "Agent"}));
        let result = normalize_send_message_cc_bcc_argument(&mut args, "cc");
        assert!(result.is_err());
    }

    // ── enqueue_message_lexical_index non-fatal behavior ────────────

    #[test]
    fn enqueue_lexical_index_does_not_panic() {
        // When the global Tantivy bridge is not initialized,
        // enqueue_message_lexical_index should silently no-op.
        enqueue_message_lexical_index(&mcp_agent_mail_db::search_v3::IndexableMessage {
            id: 1,
            project_id: 1,
            project_slug: "test-project".into(),
            sender_name: "TestAgent".into(),
            subject: "Test Subject".into(),
            body_md: "Test body".into(),
            thread_id: Some("thread-1".into()),
            importance: "normal".into(),
            created_ts: 1_000_000,
        });
        // If we reach here, the function didn't panic.
    }

    #[test]
    fn enqueue_lexical_index_none_thread_id_does_not_panic() {
        enqueue_message_lexical_index(&mcp_agent_mail_db::search_v3::IndexableMessage {
            id: 2,
            project_id: 1,
            project_slug: "proj".into(),
            sender_name: "Agent".into(),
            subject: "Subject".into(),
            body_md: "Body".into(),
            thread_id: None,
            importance: "high".into(),
            created_ts: 0,
        });
    }

    #[test]
    fn enqueue_lexical_index_empty_fields_does_not_panic() {
        enqueue_message_lexical_index(&mcp_agent_mail_db::search_v3::IndexableMessage {
            id: 0,
            project_id: 0,
            project_slug: String::new(),
            sender_name: String::new(),
            subject: String::new(),
            body_md: String::new(),
            thread_id: None,
            importance: String::new(),
            created_ts: 0,
        });
    }
}
