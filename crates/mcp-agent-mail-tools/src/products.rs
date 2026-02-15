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
    let sql = "SELECT id, product_uid, name, created_at FROM products WHERE product_uid = ? OR name = ? LIMIT 1";
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

    // Use the dedicated cross-project/product DB query path.
    let rows = db_outcome_to_mcp_result(
        mcp_agent_mail_db::queries::search_messages_for_product(
            ctx.cx(),
            &pool,
            product_id,
            trimmed,
            max_results,
        )
        .await,
    )?;

    let result: Vec<ProductSearchItem> = rows
        .into_iter()
        .map(|r| ProductSearchItem {
            id: r.id,
            subject: r.subject,
            importance: r.importance,
            ack_required: i32::try_from(r.ack_required).unwrap_or_default(),
            created_ts: Some(micros_to_iso(r.created_ts)),
            thread_id: r.thread_id,
            from: r.from,
            project_id: r.project_id,
        })
        .collect();

    let response = ProductSearchResponse { result };
    serde_json::to_string(&response)
        .map_err(|e| McpError::internal_error(format!("JSON error: {e}")))
}

/// Retrieve recent messages for an agent across all projects linked to a product (non-mutating).
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
        let Ok(agent) =
            resolve_agent(ctx, &pool, project_id, &agent_name, &p.slug, &p.human_key).await
        else {
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
        };
        let json = serde_json::to_string(&item).unwrap();
        let parsed: ProductSearchItem = serde_json::from_str(&json).unwrap();
        assert!(parsed.created_ts.is_none());
        assert!(parsed.thread_id.is_none());
    }

    #[test]
    fn product_search_response_empty() {
        let resp = ProductSearchResponse { result: Vec::new() };
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
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ProductSearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.result.len(), 2);
        assert_eq!(parsed.result[0].subject, "First");
        assert_eq!(parsed.result[1].subject, "Second");
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
    fn product_uid_pads_to_20_chars() {
        // Even with a very small input, should pad to 20 chars
        let uid = generate_product_uid(1);
        assert_eq!(uid.len(), 20);
    }

    #[test]
    fn product_uid_large_timestamp() {
        let large_ts = 9_999_999_999_999_i64;
        let uid = generate_product_uid(large_ts);
        assert_eq!(uid.len(), 20);
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
