//! Product cluster tools (cross-project operations)
//!
//! Ported from legacy Python:
//! - Feature-gated behind `WORKTREES_ENABLED=1`
//! - Products are global (not per-project)
//! - Product keys may match `product_uid` or `name`
//! - Cross-project search/inbox/thread summary operate across linked projects

use asupersync::Cx;
use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbError, DbPool, ProductRow, micros_to_iso};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::llm;
use crate::messaging::InboxMessage;
use crate::search::{ExampleMessage, SingleThreadResponse};
use crate::tool_util::{
    db_error_to_mcp_error, db_outcome_to_mcp_result, get_db_pool, get_read_db_pool,
    legacy_tool_error, parse_attachment_metadata_json, parse_recipients_lists, resolve_project,
};

static PRODUCT_UID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn worktrees_required() -> McpError {
    legacy_tool_error(
        "FEATURE_DISABLED",
        "Product Bus is disabled. Enable WORKTREES_ENABLED to use this tool.",
        true,
        serde_json::json!({ "feature": "worktrees", "env_var": "WORKTREES_ENABLED" }),
    )
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_hex_uid(candidate: &str) -> bool {
    let s = candidate.trim();
    if s.len() < 8 || s.len() > 64 {
        return false;
    }
    s.chars().all(|c| c.is_ascii_hexdigit())
}

fn generate_product_uid(now_micros: i64) -> String {
    use std::fmt::Write;
    let seq = PRODUCT_UID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    let mut out = String::with_capacity(32);
    // Format directly into the output buffer
    #[allow(clippy::cast_sign_loss)]
    let time_component = now_micros as u64;
    let _ = write!(out, "{time_component:016x}{pid:08x}{seq:08x}");

    if out.len() > 32 {
        // If it somehow exceeds 32 chars, we keep the rightmost 32 chars
        // to ensure we keep the sequence number and least significant bits of time/pid.
        let start = out.len() - 32;
        out.drain(0..start);
    }
    out
}

fn parse_fetch_inbox_product_limit(limit: Option<i32>) -> McpResult<usize> {
    let mut msg_limit = limit.unwrap_or(20);
    if msg_limit < 1 {
        return Err(legacy_tool_error(
            "INVALID_LIMIT",
            format!("limit must be at least 1, got {msg_limit}. Use a positive integer."),
            true,
            serde_json::json!({ "provided": msg_limit, "min": 1, "max": 1000 }),
        ));
    }
    if msg_limit > 1000 {
        tracing::info!(
            "fetch_inbox_product limit {} is very large; capping at 1000",
            msg_limit
        );
        msg_limit = 1000;
    }
    usize::try_from(msg_limit).map_err(|_| {
        legacy_tool_error(
            "INVALID_LIMIT",
            format!("limit exceeds supported range: {msg_limit}"),
            true,
            serde_json::json!({ "provided": msg_limit, "min": 1, "max": 1000 }),
        )
    })
}

fn parse_product_since_ts(since_ts: Option<&str>) -> Option<i64> {
    since_ts.and_then(|raw| {
        let parsed = mcp_agent_mail_db::iso_to_micros(raw);
        if parsed.is_none() {
            tracing::debug!(
                since_ts = raw,
                "ignoring invalid fetch_inbox_product since_ts to preserve Python parity"
            );
        }
        parsed
    })
}

fn validate_product_inbox_agent_name(agent_name: &str) -> McpResult<()> {
    if agent_name.trim().is_empty() {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "Agent name cannot be empty. Provide a valid agent name.",
            true,
            serde_json::json!({"parameter":"agent_name","provided":agent_name}),
        ));
    }

    const AGENT_PLACEHOLDER_PATTERNS: &[&str] = &[
        "YOUR_AGENT",
        "YOUR_AGENT_NAME",
        "AGENT_NAME",
        "PLACEHOLDER",
        "<AGENT>",
        "{AGENT}",
        "$AGENT",
    ];
    let name_upper = agent_name.trim().to_ascii_uppercase();
    for pattern in AGENT_PLACEHOLDER_PATTERNS {
        if name_upper.contains(pattern) || name_upper == *pattern {
            return Err(legacy_tool_error(
                "CONFIGURATION_ERROR",
                format!(
                    "Detected placeholder value '{agent_name}' instead of a real agent name. \
                     Replace placeholder values with your actual agent name."
                ),
                true,
                serde_json::json!({
                    "parameter": "agent_name",
                    "provided": agent_name,
                    "detected_placeholder": pattern,
                    "fix_hint": "Update AGENT_MAIL_AGENT or agent_name in your configuration",
                }),
            ));
        }
    }

    Ok(())
}

fn parse_product_thread_limit(per_thread_limit: Option<i32>) -> McpResult<usize> {
    let msg_limit_raw = per_thread_limit.unwrap_or(50);
    if msg_limit_raw < 1 {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            "Invalid argument value: per_thread_limit must be at least 1. Check that all parameters have valid values.",
            true,
            serde_json::json!({"field":"per_thread_limit","error_detail":msg_limit_raw}),
        ));
    }
    usize::try_from(msg_limit_raw).map_err(|_| {
        legacy_tool_error(
            "INVALID_ARGUMENT",
            "Invalid argument value: per_thread_limit exceeds supported range. Check that all parameters have valid values.",
            true,
            serde_json::json!({"field":"per_thread_limit","error_detail":msg_limit_raw}),
        )
    })
}

fn parse_product_search_limit(limit: Option<i32>) -> usize {
    let max_results_raw = match limit {
        Some(value) if value > 0 => value.clamp(1, 1000),
        _ => 20,
    };
    max_results_raw.unsigned_abs() as usize
}

async fn get_product_by_key(cx: &Cx, pool: &DbPool, key: &str) -> McpResult<Option<ProductRow>> {
    match mcp_agent_mail_db::queries::get_product_by_key(cx, pool, key).await {
        Outcome::Ok(product) => Ok(Some(product)),
        Outcome::Err(DbError::NotFound { .. }) => Ok(None),
        Outcome::Err(err) => Err(db_error_to_mcp_error(err)),
        Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
        Outcome::Panicked(panic) => Err(McpError::internal_error(format!(
            "Internal panic: {}",
            panic.message()
        ))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductResponse {
    pub id: i64,
    pub product_uid: String,
    pub name: String,
    pub created_at: String,
}

/// Ensure a Product exists. If not, create one.
///
/// # Conformance
/// Python-parity.
#[tool(
    description = "Ensure a Product exists. If not, create one.\n\n- product_key may be a product_uid or a name\n- If both are absent, error"
)]
pub async fn ensure_product(
    ctx: &McpContext,
    product_key: Option<String>,
    name: Option<String>,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let key_raw = product_key
        .as_deref()
        .or(name.as_deref())
        .unwrap_or("")
        .trim();
    if key_raw.is_empty() {
        return Err(legacy_tool_error(
            "MISSING_FIELD",
            "Provide product_key or name.",
            true,
            serde_json::json!({ "field": "product_key" }),
        ));
    }

    let pool = get_db_pool()?;
    if let Some(existing) = get_product_by_key(ctx.cx(), &pool, key_raw).await? {
        let response = ProductResponse {
            id: existing.id.unwrap_or(0),
            product_uid: existing.product_uid,
            name: existing.name,
            created_at: micros_to_iso(existing.created_at),
        };
        return serde_json::to_string(&response)
            .map_err(|e| McpError::internal_error(format!("JSON error: {e}")));
    }

    let now = mcp_agent_mail_db::now_micros();
    let uid = match product_key.as_deref() {
        Some(pk) if is_hex_uid(pk) => pk.trim().to_ascii_lowercase(),
        _ => generate_product_uid(now),
    };
    let display_name_raw = name.as_deref().unwrap_or(key_raw);
    let mut display_name = collapse_whitespace(display_name_raw)
        .chars()
        .take(255)
        .collect::<String>();
    if display_name.is_empty() {
        display_name = uid.clone();
    }

    let row = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::ensure_product(
            ctx.cx(),
            &pool,
            Some(uid.as_str()),
            Some(display_name.as_str()),
        )
        .await,
    )?;

    let response = ProductResponse {
        id: row.id.unwrap_or(0),
        product_uid: row.product_uid,
        name: row.name,
        created_at: micros_to_iso(row.created_at),
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductSummary {
    pub id: i64,
    pub product_uid: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductsLinkResponse {
    pub product: ProductSummary,
    pub project: ProjectSummary,
    pub linked: bool,
}

/// Link a project into a product (idempotent).
///
/// # Conformance
/// Python-parity.
#[tool(description = "Link a project into a product (idempotent).")]
pub async fn products_link(
    ctx: &McpContext,
    product_key: String,
    project_key: String,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let pool = get_db_pool()?;

    let product = get_product_by_key(ctx.cx(), &pool, product_key.trim())
        .await?
        .ok_or_else(|| {
            legacy_tool_error(
                "NOT_FOUND",
                format!("Product not found: {product_key}"),
                true,
                serde_json::json!({ "entity": "Product", "identifier": product_key }),
            )
        })?;

    let project = resolve_project(ctx, &pool, &project_key).await?;
    let product_id = product.id.unwrap_or(0);
    let project_id = project.id.unwrap_or(0);

    let _ = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::link_product_to_projects(
            ctx.cx(),
            &pool,
            product_id,
            &[project_id],
        )
        .await,
    )?;

    let response = ProductsLinkResponse {
        product: ProductSummary {
            id: product_id,
            product_uid: product.product_uid,
            name: product.name,
        },
        project: ProjectSummary {
            id: project_id,
            slug: project.slug,
            human_key: project.human_key,
        },
        linked: true,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductSearchItem {
    pub id: i64,
    pub subject: String,
    pub importance: String,
    pub ack_required: i32,
    pub created_ts: Option<String>,
    pub thread_id: Option<String>,
    pub from: String,
    pub project_id: i64,
    /// Message body (Markdown). Populated only when the caller passes
    /// `include_body_md=true`; otherwise omitted from the JSON envelope so
    /// FTS5 result lists stay cheap by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductSearchResponse {
    pub result: Vec<ProductSearchItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistance: Option<mcp_agent_mail_db::QueryAssistance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<crate::search::SearchDiagnostics>,
}

/// Search across all projects linked to a product.
///
/// # Conformance
/// Python-parity.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[tool(
    description = "Search across all projects linked to a product using the unified Search V3 service.\n\nParameters\n----------\nproduct_key : str\n    Product identifier.\nquery : str\n    Search query string.\nlimit : int\n    Max results to return (default 20, max 1000).\ncursor : str\n    Stable pagination cursor for large result sets.\nproject : str\n    Optional project filter inside the product scope.\n    Aliases: `project_key_filter`, `project_slug`, `proj`.\nsender : str\n    Filter by sender agent name (exact match). Aliases: `from_agent`, `sender_name`.\nimportance : str\n    Filter by importance level(s). Comma-separated: \"low\", \"normal\", \"high\", \"urgent\".\nthread_id : str\n    Filter by thread ID (exact match).\ndate_start : str\n    Inclusive lower bound for created timestamp.\ndate_end : str\n    Inclusive upper bound for created timestamp.\n    Aliases for start: `date_from`, `after`, `since`.\n    Aliases for end: `date_to`, `before`, `until`.\ninclude_body_md : bool\n    If true, include the full `body_md` field on each result (default false). Use this\n    when the caller intends to read message contents directly from search output rather\n    than via `fetch_inbox_product` or `resource://thread/...`.\n\nReturns\n-------\ndict\n    { result: [{ id, subject, importance, ack_required, created_ts, thread_id, from, project_id, body_md? }], assistance?, next_cursor?, diagnostics? }\n\n`body_md` is only present when `include_body_md=true`."
)]
pub async fn search_messages_product(
    ctx: &McpContext,
    product_key: String,
    query: String,
    limit: Option<i32>,
    cursor: Option<String>,
    project: Option<String>,
    project_key_filter: Option<String>,
    project_slug: Option<String>,
    proj: Option<String>,
    sender: Option<String>,
    from_agent: Option<String>,
    sender_name: Option<String>,
    importance: Option<String>,
    thread_id: Option<String>,
    ranking: Option<String>,
    date_start: Option<String>,
    date_end: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    after: Option<String>,
    before: Option<String>,
    since: Option<String>,
    until: Option<String>,
    include_body_md: Option<bool>,
) -> McpResult<String> {
    let include_body_md = include_body_md.unwrap_or(false);
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let pool = get_read_db_pool()?;
    let product = get_product_by_key(ctx.cx(), &pool, product_key.trim())
        .await?
        .ok_or_else(|| {
            legacy_tool_error(
                "NOT_FOUND",
                format!("Product not found: {product_key}"),
                true,
                serde_json::json!({ "entity": "Product", "identifier": product_key }),
            )
        })?;
    let product_id = product.id.unwrap_or(0);

    let trimmed = query.trim();
    if trimmed.is_empty() {
        let response = ProductSearchResponse {
            result: Vec::new(),
            assistance: None,
            next_cursor: None,
            diagnostics: None,
        };
        return serde_json::to_string(&response)
            .map_err(|e| McpError::internal_error(format!("JSON error: {e}")));
    }

    let max_results = parse_product_search_limit(limit);

    // Parse optional ranking mode
    let ranking_mode = match &ranking {
        Some(r) => crate::search::parse_search_mode(r)?,
        None => mcp_agent_mail_db::search_planner::RankingMode::default(),
    };

    // Parse optional filters (reuse helpers from search module)
    let importance_filter = match &importance {
        Some(imp) => crate::search::parse_importance_list(imp)?,
        None => Vec::new(),
    };
    let sender_filter = crate::search::resolve_text_filter_alias(
        "sender",
        sender,
        &[("from_agent", from_agent), ("sender_name", sender_name)],
    )?
    .map(|n| mcp_agent_mail_core::models::normalize_agent_name(&n).unwrap_or(n));
    let project_filter = crate::search::resolve_text_filter_alias(
        "project",
        project,
        &[
            ("project_key_filter", project_key_filter),
            ("project_slug", project_slug),
            ("proj", proj),
        ],
    )?;
    let time_range = crate::search::parse_time_range_with_aliases(
        date_start,
        date_end,
        &[("date_from", date_from), ("after", after), ("since", since)],
        &[("date_to", date_to), ("before", before), ("until", until)],
    )?;
    let scoped_project_id = if let Some(project_selector) = project_filter {
        let project = resolve_project(ctx, &pool, &project_selector).await?;
        project.id
    } else {
        None
    };

    // Build planner query with product scope and facets
    let search_query = mcp_agent_mail_db::search_planner::SearchQuery {
        text: trimmed.to_string(),
        doc_kind: mcp_agent_mail_db::search_planner::DocKind::Message,
        project_id: scoped_project_id,
        product_id: Some(product_id),
        importance: importance_filter,
        direction: None,
        agent_name: sender_filter,
        thread_id,
        ack_required: None,
        time_range,
        ranking: ranking_mode,
        limit: Some(max_results),
        cursor,
        // Collect explain internally to derive deterministic degraded diagnostics.
        explain: true,
        ..Default::default()
    };

    // Product search always routes through the unified Search V3 service.
    let planner_response = db_outcome_to_mcp_result(
        mcp_agent_mail_db::search_service::execute_search_simple(ctx.cx(), &pool, &search_query)
            .await,
    )?;

    let result: Vec<ProductSearchItem> = planner_response
        .results
        .into_iter()
        .map(|r| ProductSearchItem {
            id: r.id,
            subject: r.title,
            importance: r.importance.unwrap_or_default(),
            ack_required: i32::from(r.ack_required.unwrap_or(false)),
            created_ts: r.created_ts.map(micros_to_iso),
            thread_id: r.thread_id,
            from: r.from_agent.unwrap_or_default(),
            project_id: r.project_id.unwrap_or(0),
            body_md: if include_body_md { Some(r.body) } else { None },
        })
        .collect();

    let diagnostics = crate::search::derive_search_diagnostics(planner_response.explain.as_ref());
    let response = ProductSearchResponse {
        result,
        assistance: planner_response.assistance,
        next_cursor: planner_response.next_cursor,
        diagnostics,
    };
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Retrieve recent messages for an agent across all projects linked to a product (non-mutating).
///
/// # Conformance
/// Python-parity.
#[allow(clippy::items_after_statements, clippy::too_many_lines)]
#[tool(
    description = "Retrieve recent messages for an agent across all projects linked to a product (non-mutating)."
)]
pub async fn fetch_inbox_product(
    ctx: &McpContext,
    product_key: String,
    agent_name: String,
    limit: Option<i32>,
    urgent_only: Option<bool>,
    include_bodies: Option<bool>,
    since_ts: Option<String>,
) -> McpResult<String> {
    let agent_name = mcp_agent_mail_core::models::normalize_agent_name(&agent_name)
        .unwrap_or_else(|| agent_name.trim().to_string());

    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let pool = get_read_db_pool()?;
    let product = get_product_by_key(ctx.cx(), &pool, product_key.trim())
        .await?
        .ok_or_else(|| {
            legacy_tool_error(
                "NOT_FOUND",
                format!("Product not found: {product_key}"),
                true,
                serde_json::json!({ "entity": "Product", "identifier": product_key }),
            )
        })?;
    let product_id = product.id.unwrap_or(0);

    let max_messages = parse_fetch_inbox_product_limit(limit)?;
    let urgent = urgent_only.unwrap_or(false);
    let with_bodies = include_bodies.unwrap_or(false);
    let since_micros = parse_product_since_ts(since_ts.as_deref());
    validate_product_inbox_agent_name(&agent_name)?;

    let rows = db_outcome_to_mcp_result(if with_bodies {
        mcp_agent_mail_db::queries::fetch_inbox_for_product_agent(
            ctx.cx(),
            &pool,
            product_id,
            &agent_name,
            urgent,
            since_micros,
            max_messages,
        )
        .await
    } else {
        mcp_agent_mail_db::queries::fetch_inbox_for_product_agent_metadata(
            ctx.cx(),
            &pool,
            product_id,
            &agent_name,
            urgent,
            since_micros,
            max_messages,
        )
        .await
    })?;

    let out: Vec<InboxMessage> = rows
        .into_iter()
        .map(|row| {
            let msg = row.message;
            let created_ts = msg.created_ts;
            let id = msg.id.unwrap_or(0);
            let recipients = parse_recipients_lists(&msg.recipients_json);
            InboxMessage {
                id,
                project_id: msg.project_id,
                sender_id: msg.sender_id,
                thread_id: msg.thread_id,
                subject: msg.subject,
                importance: msg.importance,
                ack_required: msg.ack_required != 0,
                from: row.sender_name,
                to: recipients.to,
                cc: recipients.cc,
                bcc: Vec::new(),
                created_ts: Some(micros_to_iso(created_ts)),
                kind: row.kind,
                attachments: parse_attachment_metadata_json(&msg.attachments),
                body_md: if with_bodies { Some(msg.body_md) } else { None },
            }
        })
        .collect();

    serde_json::to_string(&out).map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Summarize a thread (by id or thread key) across all projects linked to a product.
///
/// # Conformance
/// Python-parity.
#[tool(
    description = "Summarize a thread (by id or thread key) across all projects linked to a product."
)]
#[allow(clippy::too_many_arguments)]
pub async fn summarize_thread_product(
    ctx: &McpContext,
    product_key: String,
    thread_id: String,
    include_examples: Option<bool>,
    llm_mode: Option<bool>,
    llm_model: Option<String>,
    per_thread_limit: Option<i32>,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let pool = get_read_db_pool()?;
    let product = get_product_by_key(ctx.cx(), &pool, product_key.trim())
        .await?
        .ok_or_else(|| {
            legacy_tool_error(
                "NOT_FOUND",
                format!("Product not found: {product_key}"),
                true,
                serde_json::json!({ "entity": "Product", "identifier": product_key }),
            )
        })?;
    let product_id = product.id.unwrap_or(0);

    let projects = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_product_projects(ctx.cx(), &pool, product_id).await,
    )?;

    let msg_limit = parse_product_thread_limit(per_thread_limit)?;
    let mut rows: Vec<mcp_agent_mail_db::queries::ThreadMessageRow> =
        Vec::with_capacity(msg_limit.saturating_mul(projects.len()));
    for p in projects {
        let project_id = p.id.unwrap_or(0);
        let msgs = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::list_thread_messages(
                ctx.cx(),
                &pool,
                project_id,
                &thread_id,
                Some(msg_limit),
            )
            .await,
        )?;
        rows.extend(msgs);
    }

    rows.sort_by(|a, b| {
        a.created_ts
            .cmp(&b.created_ts)
            .then_with(|| a.id.cmp(&b.id))
    });
    let start_idx = rows.len().saturating_sub(msg_limit);
    if start_idx > 0 {
        rows.drain(..start_idx);
    }
    let use_llm = llm_mode.unwrap_or(true);
    let mut summary = crate::search::summarize_messages(&rows);

    // Optional LLM refinement (legacy parity: same merge semantics as summarize_thread).
    if use_llm && config.llm_enabled {
        let start_idx = rows.len().saturating_sub(llm::MAX_MESSAGES_FOR_LLM);
        let msg_tuples: Vec<(i64, String, String, String)> = rows[start_idx..]
            .iter()
            .map(|m| (m.id, m.from.clone(), m.subject.clone(), m.body_md.clone()))
            .collect();

        let system = llm::single_thread_system_prompt();
        let user = llm::single_thread_user_prompt(&msg_tuples);

        match llm::complete_system_user(
            ctx.cx(),
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
                        "summarize_thread_product.llm_skipped: could not parse LLM response"
                    );
                }
            }
            Err(e) => {
                tracing::debug!("summarize_thread_product.llm_skipped: {e}");
            }
        }
    }

    let with_examples = include_examples.unwrap_or(false);
    let mut examples = Vec::with_capacity(if with_examples { 10 } else { 0 });
    if with_examples {
        let start_idx = rows.len().saturating_sub(10);
        for row in &rows[start_idx..] {
            examples.push(ExampleMessage {
                id: row.id,
                from: row.from.clone(),
                subject: row.subject.clone(),
                created_ts: micros_to_iso(row.created_ts),
            });
        }
    }

    let response = SingleThreadResponse {
        thread_id,
        summary,
        examples,
    };

    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;
    use asupersync::runtime::RuntimeBuilder;
    use fastmcp::McpContext;
    use mcp_agent_mail_core::config::with_process_env_overrides_for_test;

    // -----------------------------------------------------------------------
    // collapse_whitespace
    // -----------------------------------------------------------------------

    #[test]
    fn collapse_whitespace_single_spaces() {
        assert_eq!(collapse_whitespace("hello world"), "hello world");
    }

    #[test]
    fn collapse_whitespace_multiple_spaces() {
        assert_eq!(collapse_whitespace("hello   world"), "hello world");
    }

    #[test]
    fn collapse_whitespace_tabs_and_newlines() {
        assert_eq!(collapse_whitespace("hello\t\n  world"), "hello world");
    }

    #[test]
    fn collapse_whitespace_leading_trailing() {
        assert_eq!(collapse_whitespace("  hello  "), "hello");
    }

    #[test]
    fn collapse_whitespace_empty() {
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn collapse_whitespace_only_spaces() {
        assert_eq!(collapse_whitespace("   "), "");
    }

    // -----------------------------------------------------------------------
    // is_hex_uid
    // -----------------------------------------------------------------------

    #[test]
    fn hex_uid_valid_8_chars() {
        assert!(is_hex_uid("abcdef12"));
    }

    #[test]
    fn hex_uid_valid_20_chars() {
        assert!(is_hex_uid("abcdef1234567890abcd"));
    }

    #[test]
    fn hex_uid_valid_64_chars() {
        assert!(is_hex_uid(&"a".repeat(64)));
    }

    #[test]
    fn hex_uid_too_short() {
        assert!(!is_hex_uid("abcdef1"));
    }

    #[test]
    fn hex_uid_too_long() {
        assert!(!is_hex_uid(&"a".repeat(65)));
    }

    #[test]
    fn hex_uid_empty() {
        assert!(!is_hex_uid(""));
    }

    #[test]
    fn hex_uid_non_hex_chars() {
        assert!(!is_hex_uid("abcdefgh12345678"));
    }

    #[test]
    fn hex_uid_with_whitespace_trimmed() {
        assert!(is_hex_uid("  abcdef12  "));
    }

    #[test]
    fn hex_uid_uppercase() {
        assert!(is_hex_uid("ABCDEF12"));
    }

    #[test]
    fn hex_uid_mixed_case() {
        assert!(is_hex_uid("AbCdEf12"));
    }

    // -----------------------------------------------------------------------
    // generate_product_uid
    // -----------------------------------------------------------------------

    #[test]
    fn product_uid_is_32_chars() {
        let uid = generate_product_uid(1_000_000);
        assert_eq!(uid.len(), 32);
    }

    #[test]
    fn product_uid_is_hex() {
        let uid = generate_product_uid(1_000_000);
        assert!(uid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn product_uid_is_lowercase() {
        let uid = generate_product_uid(1_000_000);
        assert_eq!(uid, uid.to_ascii_lowercase());
    }

    #[test]
    fn product_uid_unique() {
        let a = generate_product_uid(1_000_000);
        let b = generate_product_uid(1_000_000);
        assert_ne!(a, b, "sequential UIDs should differ (counter increments)");
    }

    #[test]
    fn product_uid_different_timestamps() {
        let a = generate_product_uid(1_000_000);
        let b = generate_product_uid(2_000_000);
        assert_ne!(a, b);
    }

    #[test]
    fn product_uid_zero_timestamp() {
        let uid = generate_product_uid(0);
        assert_eq!(uid.len(), 32);
        assert!(uid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // -----------------------------------------------------------------------
    // worktrees_required
    // -----------------------------------------------------------------------

    #[test]
    fn worktrees_error_is_feature_disabled() {
        let err = worktrees_required();
        let msg = err.to_string();
        assert!(msg.contains("disabled") || msg.contains("FEATURE_DISABLED"));
    }

    // -----------------------------------------------------------------------
    // worktrees_required error details (br-3h13.4.7)
    // -----------------------------------------------------------------------

    #[test]
    fn worktrees_required_mentions_env_var() {
        let err = worktrees_required();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("WORKTREES_ENABLED"),
            "error should mention WORKTREES_ENABLED: {msg}"
        );
    }

    #[test]
    fn worktrees_required_mentions_product_bus() {
        let err = worktrees_required();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("Product Bus"),
            "error should mention Product Bus: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // ProductResponse serde (br-3h13.4.7)
    // -----------------------------------------------------------------------

    #[test]
    fn product_response_round_trip() {
        let resp = ProductResponse {
            id: 42,
            product_uid: "abc123def456".to_string(),
            name: "Test Product".to_string(),
            created_at: "2026-02-12T12:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ProductResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.product_uid, "abc123def456");
        assert_eq!(parsed.name, "Test Product");
        assert!(parsed.created_at.contains("2026"));
    }

    // -----------------------------------------------------------------------
    // ProductSummary and ProjectSummary serde (br-3h13.4.7)
    // -----------------------------------------------------------------------

    #[test]
    fn product_summary_serde() {
        let summary = ProductSummary {
            id: 1,
            product_uid: "uid123".to_string(),
            name: "My Product".to_string(),
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: ProductSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.product_uid, "uid123");
        assert_eq!(parsed.name, "My Product");
    }

    #[test]
    fn project_summary_serde() {
        let summary = ProjectSummary {
            id: 5,
            slug: "my-project".to_string(),
            human_key: "/data/projects/my-project".to_string(),
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: ProjectSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 5);
        assert_eq!(parsed.slug, "my-project");
        assert!(parsed.human_key.contains("/data/projects"));
    }

    // -----------------------------------------------------------------------
    // ProductsLinkResponse serde (br-3h13.4.7)
    // -----------------------------------------------------------------------

    #[test]
    fn products_link_response_serde() {
        let resp = ProductsLinkResponse {
            product: ProductSummary {
                id: 1,
                product_uid: "prod123".to_string(),
                name: "Product A".to_string(),
            },
            project: ProjectSummary {
                id: 2,
                slug: "proj-a".to_string(),
                human_key: "/path/to/proj-a".to_string(),
            },
            linked: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ProductsLinkResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.linked);
        assert_eq!(parsed.product.product_uid, "prod123");
        assert_eq!(parsed.project.slug, "proj-a");
    }

    #[test]
    fn products_link_response_not_linked() {
        let resp = ProductsLinkResponse {
            product: ProductSummary {
                id: 1,
                product_uid: "uid".to_string(),
                name: "name".to_string(),
            },
            project: ProjectSummary {
                id: 2,
                slug: "slug".to_string(),
                human_key: "/path".to_string(),
            },
            linked: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ProductsLinkResponse = serde_json::from_str(&json).unwrap();
        assert!(!parsed.linked);
    }

    // -----------------------------------------------------------------------
    // ProductSearchItem and ProductSearchResponse serde (br-3h13.4.7)
    // -----------------------------------------------------------------------

    #[test]
    fn product_search_item_serde() {
        let item = ProductSearchItem {
            id: 100,
            subject: "Test message".to_string(),
            importance: "high".to_string(),
            ack_required: 1,
            created_ts: Some("2026-02-12T10:00:00Z".to_string()),
            thread_id: Some("br-123".to_string()),
            from: "GoldFox".to_string(),
            project_id: 5,
            body_md: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        let parsed: ProductSearchItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 100);
        assert_eq!(parsed.subject, "Test message");
        assert_eq!(parsed.importance, "high");
        assert_eq!(parsed.ack_required, 1);
        assert_eq!(parsed.thread_id, Some("br-123".to_string()));
        assert_eq!(parsed.from, "GoldFox");
        assert_eq!(parsed.project_id, 5);
        assert!(parsed.body_md.is_none());
        // Default serialization MUST omit body_md to keep result lists cheap.
        assert!(!json.contains("body_md"));
    }

    #[test]
    fn product_search_item_nullable_fields() {
        let item = ProductSearchItem {
            id: 1,
            subject: "Msg".to_string(),
            importance: "normal".to_string(),
            ack_required: 0,
            created_ts: None,
            thread_id: None,
            from: "Agent".to_string(),
            project_id: 1,
            body_md: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        let parsed: ProductSearchItem = serde_json::from_str(&json).unwrap();
        assert!(parsed.created_ts.is_none());
        assert!(parsed.thread_id.is_none());
    }

    #[test]
    fn product_search_item_includes_body_md_when_populated() {
        let item = ProductSearchItem {
            id: 7,
            subject: "Body test".to_string(),
            importance: "normal".to_string(),
            ack_required: 0,
            created_ts: None,
            thread_id: None,
            from: "Agent".to_string(),
            project_id: 1,
            body_md: Some("body contents".to_string()),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"body_md\":\"body contents\""));
        let parsed: ProductSearchItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.body_md.as_deref(), Some("body contents"));
    }

    #[test]
    fn product_search_response_empty() {
        let resp = ProductSearchResponse {
            result: Vec::new(),
            assistance: None,
            next_cursor: None,
            diagnostics: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ProductSearchResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.result.is_empty());
    }

    #[test]
    fn product_search_response_with_items() {
        let resp = ProductSearchResponse {
            result: vec![
                ProductSearchItem {
                    id: 1,
                    subject: "First".to_string(),
                    importance: "high".to_string(),
                    ack_required: 1,
                    created_ts: None,
                    thread_id: None,
                    from: "A".to_string(),
                    project_id: 1,
                    body_md: None,
                },
                ProductSearchItem {
                    id: 2,
                    subject: "Second".to_string(),
                    importance: "low".to_string(),
                    ack_required: 0,
                    created_ts: None,
                    thread_id: None,
                    from: "B".to_string(),
                    project_id: 2,
                    body_md: None,
                },
            ],
            assistance: None,
            next_cursor: None,
            diagnostics: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ProductSearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.result.len(), 2);
        assert_eq!(parsed.result[0].subject, "First");
        assert_eq!(parsed.result[1].subject, "Second");
    }

    #[test]
    fn product_search_response_serializes_cursor_when_present() {
        let resp = ProductSearchResponse {
            result: Vec::new(),
            assistance: None,
            next_cursor: Some("cursor-1".to_string()),
            diagnostics: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"next_cursor\":\"cursor-1\""));
        let parsed: ProductSearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.next_cursor.as_deref(), Some("cursor-1"));
    }

    #[test]
    fn fetch_inbox_product_limit_must_be_positive() {
        let err = parse_fetch_inbox_product_limit(Some(0)).expect_err("zero limit should fail");
        assert!(err.to_string().contains("limit must be at least 1"));

        let err =
            parse_fetch_inbox_product_limit(Some(-5)).expect_err("negative limit should fail");
        assert!(err.to_string().contains("limit must be at least 1"));
    }

    #[test]
    fn fetch_inbox_product_limit_caps_large_values() {
        assert_eq!(
            parse_fetch_inbox_product_limit(Some(5_000)).expect("large limit should cap"),
            1000
        );
    }

    #[test]
    fn product_search_limit_defaults_for_non_positive_inputs() {
        assert_eq!(parse_product_search_limit(None), 20);
        assert_eq!(parse_product_search_limit(Some(0)), 20);
        assert_eq!(parse_product_search_limit(Some(-5)), 20);
    }

    #[test]
    fn product_search_limit_caps_large_values() {
        assert_eq!(parse_product_search_limit(Some(5_000)), 1000);
    }

    #[test]
    fn search_messages_product_blank_query_still_validates_product_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("product-search-missing.sqlite3");

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
                ("WORKTREES_ENABLED", "1"),
            ],
            || {
                Config::reset_cached();
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                rt.block_on(async {
                    let cx = Cx::for_testing();
                    let ctx = McpContext::new(cx, 1);
                    let err = search_messages_product(
                        &ctx,
                        "missing-product".to_string(),
                        "   ".to_string(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await
                    .expect_err("missing product should be reported before empty query");
                    let message = format!("{err:?}");
                    assert!(
                        message.contains("Product not found: missing-product")
                            || message.contains("NOT_FOUND"),
                        "expected missing-product error, got: {message}"
                    );
                });
            },
        );
    }

    #[test]
    fn fetch_inbox_product_invalid_since_ts_is_ignored() {
        assert_eq!(parse_product_since_ts(Some("2026/03/09 12:00:00")), None);
    }

    #[test]
    fn summarize_thread_product_limit_must_be_positive() {
        let err =
            parse_product_thread_limit(Some(0)).expect_err("zero per_thread_limit should fail");
        assert!(
            err.to_string()
                .contains("per_thread_limit must be at least 1")
        );

        let err = parse_product_thread_limit(Some(-5))
            .expect_err("negative per_thread_limit should fail");
        assert!(
            err.to_string()
                .contains("per_thread_limit must be at least 1")
        );
    }

    #[test]
    fn summarize_thread_product_positive_limit_is_accepted() {
        assert_eq!(
            parse_product_thread_limit(Some(7)).expect("positive limit should pass"),
            7
        );
    }

    #[test]
    fn summarize_thread_product_applies_limit_after_merging_projects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("product-thread-limit.sqlite3");

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
                ("WORKTREES_ENABLED", "1"),
            ],
            || {
                Config::reset_cached();
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                rt.block_on(async {
                    let cx = Cx::for_testing();
                    let pool = get_db_pool().expect("db pool");

                    let first_project = match mcp_agent_mail_db::queries::ensure_project(
                        &cx,
                        &pool,
                        "/product-thread-limit-first",
                    )
                    .await
                    {
                        Outcome::Ok(project) => project,
                        other => panic!("ensure first project failed: {other:?}"),
                    };
                    let second_project = match mcp_agent_mail_db::queries::ensure_project(
                        &cx,
                        &pool,
                        "/product-thread-limit-second",
                    )
                    .await
                    {
                        Outcome::Ok(project) => project,
                        other => panic!("ensure second project failed: {other:?}"),
                    };

                    let first_sender = match mcp_agent_mail_db::queries::register_agent(
                        &cx,
                        &pool,
                        first_project.id.unwrap_or(0),
                        "BlueLake",
                        "coder",
                        "test",
                        None,
                        None,
                        None,
                    )
                    .await
                    {
                        Outcome::Ok(agent) => agent,
                        other => panic!("register first sender failed: {other:?}"),
                    };
                    let first_recipient = match mcp_agent_mail_db::queries::register_agent(
                        &cx,
                        &pool,
                        first_project.id.unwrap_or(0),
                        "RedPeak",
                        "coder",
                        "test",
                        None,
                        None,
                        None,
                    )
                    .await
                    {
                        Outcome::Ok(agent) => agent,
                        other => panic!("register first recipient failed: {other:?}"),
                    };
                    let second_sender = match mcp_agent_mail_db::queries::register_agent(
                        &cx,
                        &pool,
                        second_project.id.unwrap_or(0),
                        "GoldRiver",
                        "coder",
                        "test",
                        None,
                        None,
                        None,
                    )
                    .await
                    {
                        Outcome::Ok(agent) => agent,
                        other => panic!("register second sender failed: {other:?}"),
                    };
                    let second_recipient = match mcp_agent_mail_db::queries::register_agent(
                        &cx,
                        &pool,
                        second_project.id.unwrap_or(0),
                        "SilverStone",
                        "coder",
                        "test",
                        None,
                        None,
                        None,
                    )
                    .await
                    {
                        Outcome::Ok(agent) => agent,
                        other => panic!("register second recipient failed: {other:?}"),
                    };

                    let product = match mcp_agent_mail_db::queries::ensure_product(
                        &cx,
                        &pool,
                        None,
                        Some("prod-thread-limit"),
                    )
                    .await
                    {
                        Outcome::Ok(product) => product,
                        other => panic!("ensure product failed: {other:?}"),
                    };
                    match mcp_agent_mail_db::queries::link_product_to_projects(
                        &cx,
                        &pool,
                        product.id.unwrap_or(0),
                        &[
                            first_project.id.unwrap_or(0),
                            second_project.id.unwrap_or(0),
                        ],
                    )
                    .await
                    {
                        Outcome::Ok(_) => {}
                        other => panic!("link_product_to_projects failed: {other:?}"),
                    }

                    let mut created_messages = Vec::new();
                    for (project_id, sender_id, recipient_id, subject, body, created_ts) in [
                        (
                            first_project.id.unwrap_or(0),
                            first_sender.id.unwrap_or(0),
                            first_recipient.id.unwrap_or(0),
                            "first old",
                            "- first old point",
                            1_000_i64,
                        ),
                        (
                            second_project.id.unwrap_or(0),
                            second_sender.id.unwrap_or(0),
                            second_recipient.id.unwrap_or(0),
                            "second middle",
                            "- second middle point",
                            2_000_i64,
                        ),
                        (
                            second_project.id.unwrap_or(0),
                            second_sender.id.unwrap_or(0),
                            second_recipient.id.unwrap_or(0),
                            "second latest",
                            "- second latest point",
                            3_000_i64,
                        ),
                        (
                            first_project.id.unwrap_or(0),
                            first_sender.id.unwrap_or(0),
                            first_recipient.id.unwrap_or(0),
                            "first latest",
                            "- first latest point",
                            4_000_i64,
                        ),
                    ] {
                        let message =
                            match mcp_agent_mail_db::queries::create_message_with_recipients(
                                &cx,
                                &pool,
                                project_id,
                                sender_id,
                                subject,
                                body,
                                Some("PRODUCT-LIMIT-1"),
                                "normal",
                                false,
                                "[]",
                                &[(recipient_id, "to")],
                            )
                            .await
                            {
                                Outcome::Ok(message) => message,
                                other => panic!("create message failed: {other:?}"),
                            };
                        created_messages.push((message.id.unwrap_or(0), created_ts));
                    }

                    let conn = match pool.acquire(&cx).await {
                        Outcome::Ok(conn) => conn,
                        Outcome::Err(err) => panic!("acquire failed: {err}"),
                        Outcome::Cancelled(_) => panic!("acquire cancelled"),
                        Outcome::Panicked(panic) => {
                            panic!("acquire panicked: {}", panic.message())
                        }
                    };
                    for (message_id, created_ts) in created_messages {
                        conn.execute_sync(
                            "UPDATE messages SET created_ts = ? WHERE id = ?",
                            &[
                                mcp_agent_mail_db::sqlmodel::Value::BigInt(created_ts),
                                mcp_agent_mail_db::sqlmodel::Value::BigInt(message_id),
                            ],
                        )
                        .expect("set message created_ts");
                    }
                    drop(conn);

                    let ctx = McpContext::new(cx.clone(), 1);
                    let response = summarize_thread_product(
                        &ctx,
                        "prod-thread-limit".to_string(),
                        "PRODUCT-LIMIT-1".to_string(),
                        Some(true),
                        Some(false),
                        None,
                        Some(2),
                    )
                    .await
                    .expect("summarize_thread_product should succeed");
                    let parsed: SingleThreadResponse =
                        serde_json::from_str(&response).expect("parse product summary");

                    assert_eq!(parsed.summary.total_messages, 2);
                    assert_eq!(
                        parsed.summary.key_points,
                        vec![
                            "second latest point".to_string(),
                            "first latest point".to_string()
                        ]
                    );
                    assert_eq!(parsed.examples.len(), 2);
                    assert_eq!(parsed.examples[0].subject, "second latest");
                    assert_eq!(parsed.examples[1].subject, "first latest");
                });
            },
        );
    }

    #[test]
    fn fetch_inbox_product_uses_archive_snapshot_when_live_db_is_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("stale-product-inbox.sqlite3");
        let project_dir = storage_root
            .join("projects")
            .join("archive-product-project");
        let alice_dir = project_dir.join("agents").join("Alice");
        let bob_dir = project_dir.join("agents").join("Bob");
        let messages_dir = project_dir.join("messages").join("2026").join("04");
        std::fs::create_dir_all(&alice_dir).expect("create alice dir");
        std::fs::create_dir_all(&bob_dir).expect("create bob dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"archive-product-project","human_key":"/archive-product-project"}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            alice_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-04-01T12:00:00Z","last_active_ts":"2026-04-01T12:00:00Z"}"#,
        )
        .expect("write alice profile");
        std::fs::write(
            bob_dir.join("profile.json"),
            r#"{"name":"Bob","program":"coder","model":"test","inception_ts":"2026-04-01T12:00:00Z","last_active_ts":"2026-04-01T12:00:00Z"}"#,
        )
        .expect("write bob profile");
        std::fs::write(
            messages_dir.join("2026-04-01T12-00-00Z__archive__9.md"),
            r#"---json
{
  "id": 9,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Archive inbox",
  "importance": "high",
  "created_ts": "2026-04-01T12:00:00Z"
}
---

archive body
"#,
        )
        .expect("write archive message");

        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref())
            .expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'archive-product-project', '/archive-product-project', 1)",
            &[],
        )
        .expect("insert live project");
        conn.execute_sync(
            "INSERT INTO products (id, product_uid, name, created_at) VALUES (7, 'prod-stale', 'Product Stale', 2)",
            &[],
        )
        .expect("insert product");
        conn.execute_sync(
            "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (7, 1, 3)",
            &[],
        )
        .expect("insert product link");
        drop(conn);

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
                ("WORKTREES_ENABLED", "1"),
            ],
            || {
                Config::reset_cached();
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                rt.block_on(async {
                    let cx = Cx::for_testing();
                    let ctx = McpContext::new(cx.clone(), 1);
                    let response = fetch_inbox_product(
                        &ctx,
                        "prod-stale".to_string(),
                        "Bob".to_string(),
                        Some(10),
                        Some(false),
                        Some(true),
                        None,
                    )
                    .await
                    .expect("fetch_inbox_product should succeed");
                    let value: serde_json::Value =
                        serde_json::from_str(&response).expect("parse inbox json");
                    let messages = value.as_array().expect("messages array");
                    assert_eq!(messages.len(), 1);
                    assert_eq!(messages[0]["subject"], "Archive inbox");
                    assert_eq!(messages[0]["from"], "Alice");
                    assert!(
                        messages[0]["body_md"]
                            .as_str()
                            .is_some_and(|body| body.contains("archive body")),
                        "expected archive body in fetched message: {value}"
                    );
                });
            },
        );
    }

    #[test]
    fn fetch_inbox_product_surfaces_malformed_message_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("product-malformed-metadata.sqlite3");

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
                ("WORKTREES_ENABLED", "1"),
            ],
            || {
                Config::reset_cached();
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                rt.block_on(async {
                    let cx = Cx::for_testing();
                    let pool = get_db_pool().expect("db pool");
                    let project = match mcp_agent_mail_db::queries::ensure_project(
                        &cx,
                        &pool,
                        "/product-malformed-project",
                    )
                    .await
                    {
                        asupersync::Outcome::Ok(project) => project,
                        other => panic!("ensure_project failed: {other:?}"),
                    };
                    let project_id = project.id.unwrap_or(0);
                    let sender = match mcp_agent_mail_db::queries::register_agent(
                        &cx, &pool, project_id, "BlueLake", "coder", "test", None, None, None,
                    )
                    .await
                    {
                        asupersync::Outcome::Ok(agent) => agent,
                        other => panic!("register sender failed: {other:?}"),
                    };
                    let recipient = match mcp_agent_mail_db::queries::register_agent(
                        &cx, &pool, project_id, "RedPeak", "coder", "test", None, None, None,
                    )
                    .await
                    {
                        asupersync::Outcome::Ok(agent) => agent,
                        other => panic!("register recipient failed: {other:?}"),
                    };
                    let product = match mcp_agent_mail_db::queries::ensure_product(
                        &cx,
                        &pool,
                        None,
                        Some("prod-malformed"),
                    )
                    .await
                    {
                        asupersync::Outcome::Ok(product) => product,
                        other => panic!("ensure_product failed: {other:?}"),
                    };
                    match mcp_agent_mail_db::queries::link_product_to_projects(
                        &cx,
                        &pool,
                        product.id.unwrap_or(0),
                        &[project_id],
                    )
                    .await
                    {
                        asupersync::Outcome::Ok(_) => {}
                        other => panic!("link_product_to_projects failed: {other:?}"),
                    }
                    let message = match mcp_agent_mail_db::queries::create_message_with_recipients(
                        &cx,
                        &pool,
                        project_id,
                        sender.id.unwrap_or(0),
                        "Malformed Product Metadata",
                        "body",
                        Some("prod-malformed-thread"),
                        "high",
                        true,
                        "[]",
                        &[(recipient.id.unwrap_or(0), "to")],
                    )
                    .await
                    {
                        asupersync::Outcome::Ok(message) => message,
                        other => panic!("create_message_with_recipients failed: {other:?}"),
                    };

                    let conn = match pool.acquire(&cx).await {
                        asupersync::Outcome::Ok(conn) => conn,
                        asupersync::Outcome::Err(err) => panic!("acquire failed: {err}"),
                        asupersync::Outcome::Cancelled(_) => panic!("acquire cancelled"),
                        asupersync::Outcome::Panicked(panic) => {
                            panic!("acquire panicked: {}", panic.message())
                        }
                    };
                    conn.execute_sync(
                        "UPDATE messages SET recipients_json = ?, attachments = ? WHERE id = ?",
                        &[
                            mcp_agent_mail_db::sqlmodel::Value::Text(
                                r#"{"to":"RedPeak"}"#.to_string(),
                            ),
                            mcp_agent_mail_db::sqlmodel::Value::Text("{not-json".to_string()),
                            mcp_agent_mail_db::sqlmodel::Value::BigInt(message.id.unwrap_or(0)),
                        ],
                    )
                    .expect("poison message metadata");

                    let ctx = McpContext::new(cx.clone(), 1);
                    let response = fetch_inbox_product(
                        &ctx,
                        "prod-malformed".to_string(),
                        "RedPeak".to_string(),
                        Some(10),
                        Some(false),
                        Some(true),
                        None,
                    )
                    .await
                    .expect("fetch_inbox_product should succeed");
                    let value: serde_json::Value =
                        serde_json::from_str(&response).expect("parse inbox json");
                    let messages = value.as_array().expect("messages array");
                    assert_eq!(messages.len(), 1);
                    assert_eq!(
                        messages[0]["attachments"][0]["name"],
                        "[malformed-attachments-json]"
                    );
                });
            },
        );
    }

    #[test]
    fn products_link_accepts_orphaned_product_placeholder() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("product-placeholder-link.sqlite3");

        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", &format!("sqlite:///{}", db_path.display())),
                ("STORAGE_ROOT", &storage_root.display().to_string()),
                ("WORKTREES_ENABLED", "1"),
            ],
            || {
                Config::reset_cached();
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                rt.block_on(async {
                    let cx = Cx::for_testing();
                    let pool = get_db_pool().expect("db pool");
                    let existing_project = match mcp_agent_mail_db::queries::ensure_project(
                        &cx,
                        &pool,
                        "/data/projects/products-orphaned-existing",
                    )
                    .await
                    {
                        Outcome::Ok(project) => project,
                        other => panic!("ensure existing project failed: {other:?}"),
                    };
                    let target_project = match mcp_agent_mail_db::queries::ensure_project(
                        &cx,
                        &pool,
                        "/data/projects/products-orphaned-target",
                    )
                    .await
                    {
                        Outcome::Ok(project) => project,
                        other => panic!("ensure target project failed: {other:?}"),
                    };
                    let product = match mcp_agent_mail_db::queries::ensure_product(
                        &cx,
                        &pool,
                        None,
                        Some("prod-placeholder-link"),
                    )
                    .await
                    {
                        Outcome::Ok(product) => product,
                        other => panic!("ensure product failed: {other:?}"),
                    };
                    let product_id = product.id.expect("product id");
                    match mcp_agent_mail_db::queries::link_product_to_projects(
                        &cx,
                        &pool,
                        product_id,
                        &[existing_project.id.unwrap_or(0)],
                    )
                    .await
                    {
                        Outcome::Ok(_) => {}
                        other => panic!("seed product link failed: {other:?}"),
                    }

                    let conn = match pool.acquire(&cx).await {
                        Outcome::Ok(conn) => conn,
                        Outcome::Err(err) => panic!("acquire failed: {err}"),
                        Outcome::Cancelled(_) => panic!("acquire cancelled"),
                        Outcome::Panicked(panic) => {
                            panic!("acquire panicked: {}", panic.message())
                        }
                    };
                    conn.execute_sync(
                        "DELETE FROM products WHERE id = ?",
                        &[mcp_agent_mail_db::sqlmodel::Value::BigInt(product_id)],
                    )
                    .expect("delete products row");

                    let ctx = McpContext::new(cx.clone(), 1);
                    let response = products_link(
                        &ctx,
                        format!("[unknown-product-{product_id}]"),
                        target_project.human_key.clone(),
                    )
                    .await
                    .expect("products_link should succeed");
                    let parsed: ProductsLinkResponse =
                        serde_json::from_str(&response).expect("parse products_link response");
                    assert_eq!(parsed.product.id, product_id);
                    assert_eq!(
                        parsed.product.product_uid,
                        format!("[unknown-product-{product_id}]")
                    );
                    assert_eq!(parsed.project.id, target_project.id.unwrap_or(0));
                    assert_eq!(parsed.project.slug, target_project.slug);
                    assert!(parsed.linked, "expected idempotent link success");

                    let linked_projects = match mcp_agent_mail_db::queries::list_product_projects(
                        &cx, &pool, product_id,
                    )
                    .await
                    {
                        Outcome::Ok(projects) => projects,
                        other => panic!("list_product_projects failed: {other:?}"),
                    };
                    assert_eq!(
                        linked_projects.len(),
                        2,
                        "expected both the seeded and new project links to remain visible"
                    );
                    assert!(linked_projects.iter().any(|project| {
                        project.id == target_project.id && project.slug == target_project.slug
                    }));
                });
            },
        );
    }

    // -----------------------------------------------------------------------
    // generate_product_uid determinism (br-3h13.4.7)
    // -----------------------------------------------------------------------

    #[test]
    fn product_uid_from_different_pids_differs() {
        // UIDs include PID component, so sequential calls in same process differ via counter
        let a = generate_product_uid(1_000_000);
        let b = generate_product_uid(1_000_000);
        assert_ne!(
            a, b,
            "UIDs should differ even with same timestamp due to counter"
        );
    }

    #[test]
    fn product_uid_pads_to_32_chars() {
        // Even with a very small input, should pad to 32 chars
        let uid = generate_product_uid(1);
        assert_eq!(uid.len(), 32);
    }

    #[test]
    fn product_uid_large_timestamp() {
        let large_ts = 9_999_999_999_999_i64;
        let uid = generate_product_uid(large_ts);
        assert_eq!(uid.len(), 32);
        assert!(uid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // -----------------------------------------------------------------------
    // is_hex_uid boundary cases (br-3h13.4.7)
    // -----------------------------------------------------------------------

    #[test]
    fn hex_uid_exactly_8_chars() {
        assert!(is_hex_uid("abcdef12"));
        assert!(is_hex_uid("12345678"));
    }

    #[test]
    fn hex_uid_exactly_64_chars() {
        assert!(is_hex_uid(&"f".repeat(64)));
    }

    #[test]
    fn hex_uid_7_chars_rejected() {
        assert!(!is_hex_uid("abcdef1"));
    }

    #[test]
    fn hex_uid_65_chars_rejected() {
        assert!(!is_hex_uid(&"a".repeat(65)));
    }

    #[test]
    fn hex_uid_special_chars_rejected() {
        assert!(!is_hex_uid("abcdef1g")); // 'g' is not hex
        assert!(!is_hex_uid("abcdef12!")); // '!' is not hex
        assert!(!is_hex_uid("abc def12")); // space
    }

    #[test]
    fn hex_uid_whitespace_only_rejected() {
        assert!(!is_hex_uid("        ")); // 8 spaces after trim = empty
    }
}
