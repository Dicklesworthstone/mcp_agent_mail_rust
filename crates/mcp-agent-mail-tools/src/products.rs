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
use mcp_agent_mail_db::{DbPool, ProductRow, micros_to_iso};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::llm;
use crate::messaging::InboxMessage;
use crate::search::{ExampleMessage, SingleThreadResponse};
use crate::tool_util::{
    db_outcome_to_mcp_result, get_db_pool, legacy_tool_error, resolve_agent, resolve_project,
};

static PRODUCT_UID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn worktrees_required() -> McpError {
    legacy_tool_error(
        "FEATURE_DISABLED",
        "Product Bus is disabled. Enable WORKTREES_ENABLED to use this tool.",
        false,
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
    let seq = PRODUCT_UID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    let raw = format!("{now_micros:x}{pid:x}{seq:x}");
    let mut out = String::with_capacity(20);
    for ch in raw.chars() {
        if ch.is_ascii_hexdigit() {
            out.push(ch.to_ascii_lowercase());
        }
        if out.len() == 20 {
            break;
        }
    }
    while out.len() < 20 {
        out.push('0');
    }
    out
}

async fn get_product_by_key(cx: &Cx, pool: &DbPool, key: &str) -> McpResult<Option<ProductRow>> {
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
    let elapsed = mcp_agent_mail_db::elapsed_us(start);
    mcp_agent_mail_db::tracking::record_query(sql, elapsed);
    let rows = rows.map_err(|e| McpError::internal_error(e.to_string()))?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let product =
        ProductRow::from_row(&row).map_err(|e| McpError::internal_error(e.to_string()))?;
    Ok(Some(product))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductResponse {
    pub id: i64,
    pub product_uid: String,
    pub name: String,
    pub created_at: String,
}

/// Ensure a Product exists. If not, create one.
#[tool(description = "Ensure a Product exists. If not, create one.")]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductSearchResponse {
    pub result: Vec<ProductSearchItem>,
}

/// Full-text search across all projects linked to a product.
#[tool(description = "Full-text search across all projects linked to a product.")]
pub async fn search_messages_product(
    ctx: &McpContext,
    product_key: String,
    query: String,
    limit: Option<i32>,
) -> McpResult<String> {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return Err(worktrees_required());
    }

    let trimmed = query.trim();
    if trimmed.is_empty() {
        let response = ProductSearchResponse { result: Vec::new() };
        return serde_json::to_string(&response)
            .map_err(|e| McpError::internal_error(format!("JSON error: {e}")));
    }

    let pool = get_db_pool()?;
    let max_results = usize::try_from(limit.unwrap_or(20)).unwrap_or(20);

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

    // Build a product-wide SearchQuery and execute via the unified search service
    let search_query =
        mcp_agent_mail_db::search_planner::SearchQuery::product_messages(trimmed, product_id);
    let search_query = mcp_agent_mail_db::search_planner::SearchQuery {
        limit: Some(max_results),
        ..search_query
    };

    let planner_response = db_outcome_to_mcp_result(
        mcp_agent_mail_db::search_service::execute_search_simple(ctx.cx(), &pool, &search_query)
            .await,
    )?;

    // Map planner SearchResult → tool ProductSearchItem (legacy format)
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
        })
        .collect();

    let response = ProductSearchResponse { result };
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Retrieve recent messages for an agent across all projects linked to a product (non-mutating).
#[tool(
    description = "Retrieve recent messages for an agent across all projects linked to a product."
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
    let product_id = product.id.unwrap_or(0);

    let projects = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_product_projects(ctx.cx(), &pool, product_id).await,
    )?;

    let max_messages = usize::try_from(limit.unwrap_or(20)).unwrap_or(20);
    let urgent = urgent_only.unwrap_or(false);
    let with_bodies = include_bodies.unwrap_or(false);
    let since_micros = since_ts
        .as_deref()
        .and_then(mcp_agent_mail_db::iso_to_micros);

    let mut items: Vec<(i64, i64, InboxMessage)> =
        Vec::with_capacity(usize::try_from(limit.unwrap_or(20)).unwrap_or(20)); // (created_ts, id, msg)
    for p in projects {
        let project_id = p.id.unwrap_or(0);
        // Skip if agent doesn't exist in this project.
        let Ok(agent) = resolve_agent(ctx, &pool, project_id, &agent_name).await else {
            continue;
        };
        let rows = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::fetch_inbox(
                ctx.cx(),
                &pool,
                project_id,
                agent.id.unwrap_or(0),
                urgent,
                since_micros,
                max_messages,
            )
            .await,
        )?;
        for row in rows {
            let msg = row.message;
            let created_ts = msg.created_ts;
            let id = msg.id.unwrap_or(0);
            items.push((
                created_ts,
                id,
                InboxMessage {
                    id,
                    project_id: msg.project_id,
                    sender_id: msg.sender_id,
                    thread_id: msg.thread_id,
                    subject: msg.subject,
                    importance: msg.importance,
                    ack_required: msg.ack_required != 0,
                    from: row.sender_name,
                    created_ts: None,
                    kind: row.kind,
                    attachments: serde_json::from_str(&msg.attachments).unwrap_or_default(),
                    body_md: if with_bodies { Some(msg.body_md) } else { None },
                },
            ));
        }
    }

    items.sort_by(|(a_ts, a_id, _), (b_ts, b_id, _)| b_ts.cmp(a_ts).then_with(|| a_id.cmp(b_id)));
    let out: Vec<InboxMessage> = items
        .into_iter()
        .take(max_messages)
        .map(|(_, _, m)| m)
        .collect();

    serde_json::to_string(&out).map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Summarize a thread (by id or thread key) across all projects linked to a product.
#[tool(description = "Summarize a thread across all projects linked to a product.")]
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
    let product_id = product.id.unwrap_or(0);

    let projects = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::list_product_projects(ctx.cx(), &pool, product_id).await,
    )?;

    let mut rows: Vec<mcp_agent_mail_db::queries::ThreadMessageRow> =
        Vec::with_capacity(usize::try_from(per_thread_limit.unwrap_or(50)).unwrap_or(50));
    for p in projects {
        let project_id = p.id.unwrap_or(0);
        let limit = per_thread_limit.and_then(|v| usize::try_from(v).ok());
        let msgs = db_outcome_to_mcp_result(
            mcp_agent_mail_db::queries::list_thread_messages(
                ctx.cx(),
                &pool,
                project_id,
                &thread_id,
                limit,
            )
            .await,
        )?;
        rows.extend(msgs);
    }

    rows.sort_by_key(|a| a.created_ts);
    let use_llm = llm_mode.unwrap_or(true);
    let mut summary = crate::search::summarize_messages(&rows);

    // Optional LLM refinement (legacy parity: same merge semantics as summarize_thread).
    if use_llm && config.llm_enabled {
        let msg_tuples: Vec<(i64, String, String, String)> = rows
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
        for row in rows.iter().take(10) {
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
    fn product_uid_is_20_chars() {
        let uid = generate_product_uid(1_000_000);
        assert_eq!(uid.len(), 20);
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
        assert_eq!(uid.len(), 20);
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
}
