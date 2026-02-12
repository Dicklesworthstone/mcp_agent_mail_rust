//! Unified search service bridging the query planner, SQL execution, and scope enforcement.
//!
//! This module provides [`execute_search`] — the single entry point for all search
//! operations across TUI, MCP tools, and web surfaces. It:
//!
//! 1. Plans the query via [`plan_search`]
//! 2. Executes the resulting SQL against the database
//! 3. Applies scope and redaction via [`apply_scope`]
//! 4. Tracks query telemetry
//! 5. Returns a rich [`SearchResponse`] with pagination, explain, and audit

use crate::error::DbError;
use crate::pool::DbPool;
use crate::search_planner::{
    DocKind, PlanMethod, PlanParam, SearchCursor, SearchQuery, SearchResponse, SearchResult,
    plan_search,
};
use crate::search_scope::{
    RedactionPolicy, ScopeAuditSummary, ScopeContext, ScopedSearchResult, apply_scope,
};
use crate::tracking::record_query;
use mcp_agent_mail_core::config::SearchEngine;

use asupersync::{Cx, Outcome};
use serde::{Deserialize, Serialize};
use sqlmodel_core::{Row as SqlRow, Value};
use sqlmodel_query::raw_query;
// ────────────────────────────────────────────────────────────────────
// Search service options
// ────────────────────────────────────────────────────────────────────

/// Options for search execution beyond what `SearchQuery` carries.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Scope context for permission enforcement. `None` = operator mode.
    pub scope_ctx: Option<ScopeContext>,
    /// Redaction policy for scope-filtered results. Defaults to standard.
    pub redaction_policy: Option<RedactionPolicy>,
    /// Whether to emit telemetry events for this query.
    pub track_telemetry: bool,
    /// Search engine override. `None` = use global config default.
    pub search_engine: Option<SearchEngine>,
}

// ────────────────────────────────────────────────────────────────────
// Search response types
// ────────────────────────────────────────────────────────────────────

/// Full search response including scope audit information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopedSearchResponse {
    /// Visible results (after scope filtering + redaction).
    pub results: Vec<ScopedSearchResult>,
    /// Pagination cursor for next page.
    pub next_cursor: Option<String>,
    /// Query explain metadata (when requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<crate::search_planner::QueryExplain>,
    /// Audit summary of scope enforcement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_summary: Option<ScopeAuditSummary>,
    /// Total rows returned from SQL before scope filtering.
    pub sql_row_count: usize,
}

/// Lightweight response for simple (unscoped) searches.
pub type SimpleSearchResponse = SearchResponse;

// ────────────────────────────────────────────────────────────────────
// SQL parameter conversion
// ────────────────────────────────────────────────────────────────────

fn plan_param_to_value(param: &PlanParam) -> Value {
    match param {
        PlanParam::Int(v) => Value::BigInt(*v),
        PlanParam::Text(s) => Value::Text(s.clone()),
        PlanParam::Float(f) => Value::Double(*f),
    }
}

// ────────────────────────────────────────────────────────────────────
// Internal helpers
// ────────────────────────────────────────────────────────────────────

fn map_sql_error(e: &sqlmodel_core::Error) -> DbError {
    DbError::Sqlite(e.to_string())
}

fn map_sql_outcome<T>(out: Outcome<T, sqlmodel_core::Error>) -> Outcome<T, DbError> {
    match out {
        Outcome::Ok(v) => Outcome::Ok(v),
        Outcome::Err(e) => Outcome::Err(map_sql_error(&e)),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

async fn acquire_conn(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<sqlmodel_pool::PooledConnection<crate::DbConn>, DbError> {
    map_sql_outcome(pool.acquire(cx).await)
}

// ────────────────────────────────────────────────────────────────────
// Tantivy routing helpers (feature-gated)
// ────────────────────────────────────────────────────────────────────

/// Try executing a search via the Tantivy bridge. Returns `None` if the
/// bridge is not initialized (feature disabled or `init_bridge` not called).
#[cfg(feature = "search-v3")]
fn try_tantivy_search(query: &SearchQuery) -> Option<Vec<SearchResult>> {
    let bridge = crate::search_v3::get_bridge()?;
    Some(bridge.search(query))
}

#[cfg(not(feature = "search-v3"))]
fn try_tantivy_search(_query: &SearchQuery) -> Option<Vec<SearchResult>> {
    None
}

/// Log a comparison between FTS5 and Tantivy results in shadow mode.
fn log_shadow_comparison(fts5: &[SearchResult], tantivy: &[SearchResult], query: &SearchQuery) {
    let fts5_ids: Vec<i64> = fts5.iter().map(|r| r.id).collect();
    let tantivy_ids: Vec<i64> = tantivy.iter().map(|r| r.id).collect();
    let overlap = fts5_ids
        .iter()
        .filter(|id| tantivy_ids.contains(id))
        .count();
    tracing::info!(
        query = %query.text,
        fts5_count = fts5.len(),
        tantivy_count = tantivy.len(),
        overlap_count = overlap,
        "shadow search comparison"
    );
}

// ────────────────────────────────────────────────────────────────────
// Core execution
// ────────────────────────────────────────────────────────────────────

/// Execute a search query with full plan → SQL → scope pipeline.
///
/// This is the primary entry point for all search operations.
///
/// # Errors
///
/// Returns `DbError` on database or pool errors.
pub async fn execute_search(
    cx: &Cx,
    pool: &DbPool,
    query: &SearchQuery,
    options: &SearchOptions,
) -> Outcome<ScopedSearchResponse, DbError> {
    let timer = std::time::Instant::now();
    let engine = options.search_engine.unwrap_or_default();

    // ── Tantivy-only fast path ──────────────────────────────────────
    if engine == SearchEngine::Lexical {
        if let Some(raw_results) = try_tantivy_search(query) {
            if options.track_telemetry {
                record_query(
                    "search_service_tantivy",
                    u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX),
                );
            }
            return finish_scoped_response(raw_results, query, options);
        }
        // Bridge not initialized → fall through to FTS5
    }

    // ── Shadow: pre-fetch Tantivy results for comparison ────────────
    #[allow(deprecated)]
    let shadow_tantivy = if engine.is_shadow() {
        try_tantivy_search(query)
    } else {
        None
    };

    // ── FTS5 path (default + Shadow primary) ────────────────────────

    // Step 1: Plan the query
    let plan = plan_search(query);

    if plan.method == PlanMethod::Empty {
        let explain = if query.explain {
            Some(plan.explain())
        } else {
            None
        };
        return Outcome::Ok(ScopedSearchResponse {
            results: Vec::new(),
            next_cursor: None,
            explain,
            audit_summary: None,
            sql_row_count: 0,
        });
    }

    // Step 2: Acquire connection
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    // Step 3: Execute SQL
    let values: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
    let rows_out = map_sql_outcome(raw_query(cx, &*conn, &plan.sql, &values).await);

    let rows = match rows_out {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    if options.track_telemetry {
        record_query(
            "search_service",
            u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX),
        );
    }

    // Step 4: Map rows to SearchResult
    let raw_results = map_rows_to_results(&rows, query.doc_kind);

    // Shadow comparison logging
    if let Some(ref tantivy_results) = shadow_tantivy {
        log_shadow_comparison(&raw_results, tantivy_results, query);
    }

    let sql_row_count = raw_results.len();

    // Step 5: Compute pagination cursor
    let next_cursor = compute_next_cursor(&raw_results, query.effective_limit());

    // Step 6: Apply scope enforcement
    let redaction = options.redaction_policy.clone().unwrap_or_default();
    let scope_ctx = options.scope_ctx.clone().unwrap_or_else(|| ScopeContext {
        viewer: None,
        approved_contacts: Vec::new(),
        viewer_project_ids: Vec::new(),
        sender_policies: Vec::new(),
        recipient_map: Vec::new(),
    });

    let (scoped_results, audit_summary) = apply_scope(raw_results, &scope_ctx, &redaction);

    // Step 7: Build explain
    let explain = if query.explain {
        let mut e = plan.explain();
        e.denied_count = audit_summary.denied_count;
        e.redacted_count = audit_summary.redacted_count;
        e.scope_policy.clone_from(&plan.scope_label);
        Some(e)
    } else {
        None
    };

    let audit = if scope_ctx.viewer.is_some() {
        Some(audit_summary)
    } else {
        None
    };

    Outcome::Ok(ScopedSearchResponse {
        results: scoped_results,
        next_cursor,
        explain,
        audit_summary: audit,
        sql_row_count,
    })
}

/// Apply scope enforcement and build a `ScopedSearchResponse` from raw results.
///
/// Shared by both the Tantivy and FTS5 paths to avoid duplicating scope logic.
fn finish_scoped_response(
    raw_results: Vec<SearchResult>,
    query: &SearchQuery,
    options: &SearchOptions,
) -> Outcome<ScopedSearchResponse, DbError> {
    let sql_row_count = raw_results.len();
    let next_cursor = compute_next_cursor(&raw_results, query.effective_limit());
    let redaction = options.redaction_policy.clone().unwrap_or_default();
    let scope_ctx = options.scope_ctx.clone().unwrap_or_else(|| ScopeContext {
        viewer: None,
        approved_contacts: Vec::new(),
        viewer_project_ids: Vec::new(),
        sender_policies: Vec::new(),
        recipient_map: Vec::new(),
    });
    let (scoped_results, audit_summary) = apply_scope(raw_results, &scope_ctx, &redaction);
    let audit = if scope_ctx.viewer.is_some() {
        Some(audit_summary)
    } else {
        None
    };
    Outcome::Ok(ScopedSearchResponse {
        results: scoped_results,
        next_cursor,
        explain: None,
        audit_summary: audit,
        sql_row_count,
    })
}

/// Execute a simple (unscoped) search — for backward compatibility with existing tools.
///
/// Always uses the FTS5 engine regardless of global config. Callers that want
/// Tantivy routing should use [`execute_search`] with [`SearchOptions::search_engine`].
///
/// Returns a `SearchResponse` without scope enforcement or audit.
///
/// # Errors
///
/// Returns `DbError` on database or pool errors.
pub async fn execute_search_simple(
    cx: &Cx,
    pool: &DbPool,
    query: &SearchQuery,
) -> Outcome<SimpleSearchResponse, DbError> {
    let timer = std::time::Instant::now();

    let plan = plan_search(query);

    if plan.method == PlanMethod::Empty {
        let explain = if query.explain {
            Some(plan.explain())
        } else {
            None
        };
        return Outcome::Ok(SearchResponse {
            results: Vec::new(),
            next_cursor: None,
            explain,
            audit: Vec::new(),
        });
    }

    // Acquire connection
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let values: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
    let rows_out = map_sql_outcome(raw_query(cx, &*conn, &plan.sql, &values).await);

    let rows = match rows_out {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    record_query(
        "search_service_simple",
        u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX),
    );

    let _ = timer; // prevent unused warning if telemetry is off

    let raw_results = map_rows_to_results(&rows, query.doc_kind);
    let next_cursor = compute_next_cursor(&raw_results, query.effective_limit());

    let explain = if query.explain {
        Some(plan.explain())
    } else {
        None
    };

    Outcome::Ok(SearchResponse {
        results: raw_results,
        next_cursor,
        explain,
        audit: Vec::new(),
    })
}

// ────────────────────────────────────────────────────────────────────
// Row mapping
// ────────────────────────────────────────────────────────────────────

/// Map database rows to `SearchResult` based on document kind.
fn map_rows_to_results(rows: &[SqlRow], doc_kind: DocKind) -> Vec<SearchResult> {
    rows.iter()
        .filter_map(|row| match doc_kind {
            DocKind::Message => map_message_row(row),
            DocKind::Agent => map_agent_row(row),
            DocKind::Project => map_project_row(row),
        })
        .collect()
}

fn map_message_row(row: &SqlRow) -> Option<SearchResult> {
    let id: i64 = row.get_named("id").ok()?;
    let subject: String = row.get_named("subject").unwrap_or_default();
    let body: String = row.get_named("body_md").unwrap_or_default();
    let importance: Option<String> = row.get_named("importance").ok();
    let ack_required: Option<bool> = row.get_named::<i64>("ack_required").ok().map(|v| v != 0);
    let created_ts: Option<i64> = row.get_named("created_ts").ok();
    let thread_id: Option<String> = row.get_named("thread_id").ok();
    let from_agent: Option<String> = row.get_named("from_name").ok();
    let project_id: Option<i64> = row.get_named("project_id").ok();
    let score: Option<f64> = row.get_named("score").ok();

    Some(SearchResult {
        doc_kind: DocKind::Message,
        id,
        project_id,
        title: subject,
        body,
        score,
        importance,
        ack_required,
        created_ts,
        thread_id,
        from_agent,
        redacted: false,
        redaction_reason: None,
    })
}

fn map_agent_row(row: &SqlRow) -> Option<SearchResult> {
    let id: i64 = row.get_named("id").ok()?;
    let name: String = row.get_named("name").unwrap_or_default();
    let task_desc: String = row.get_named("task_description").unwrap_or_default();
    let project_id: Option<i64> = row.get_named("project_id").ok();
    let score: Option<f64> = row.get_named("score").ok();

    Some(SearchResult {
        doc_kind: DocKind::Agent,
        id,
        project_id,
        title: name,
        body: task_desc,
        score,
        importance: None,
        ack_required: None,
        created_ts: None,
        thread_id: None,
        from_agent: None,
        redacted: false,
        redaction_reason: None,
    })
}

fn map_project_row(row: &SqlRow) -> Option<SearchResult> {
    let id: i64 = row.get_named("id").ok()?;
    let slug: String = row.get_named("slug").unwrap_or_default();
    let human_key: String = row.get_named("human_key").unwrap_or_default();
    let score: Option<f64> = row.get_named("score").ok();

    Some(SearchResult {
        doc_kind: DocKind::Project,
        id,
        project_id: Some(id),
        title: slug,
        body: human_key,
        score,
        importance: None,
        ack_required: None,
        created_ts: None,
        thread_id: None,
        from_agent: None,
        redacted: false,
        redaction_reason: None,
    })
}

// ────────────────────────────────────────────────────────────────────
// Pagination
// ────────────────────────────────────────────────────────────────────

/// Compute the next cursor if there are more results.
fn compute_next_cursor(results: &[SearchResult], limit: usize) -> Option<String> {
    if results.len() < limit {
        return None; // fewer than limit means no more pages
    }
    // Use the last result's (score, id) as cursor
    results.last().map(|r| {
        let cursor = SearchCursor {
            score: r.score.unwrap_or(0.0),
            id: r.id,
        };
        cursor.encode()
    })
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_planner::SearchCursor;

    #[test]
    fn plan_param_conversion() {
        assert!(matches!(
            plan_param_to_value(&PlanParam::Int(42)),
            Value::BigInt(42)
        ));
        assert!(matches!(
            plan_param_to_value(&PlanParam::Float(1.5)),
            Value::Double(_)
        ));
        if let Value::Text(s) = plan_param_to_value(&PlanParam::Text("hello".to_string())) {
            assert_eq!(s, "hello");
        } else {
            panic!("expected Text");
        }
    }

    #[test]
    fn next_cursor_none_when_underfull() {
        let results = vec![SearchResult {
            doc_kind: DocKind::Message,
            id: 1,
            project_id: Some(1),
            title: "t".to_string(),
            body: "b".to_string(),
            score: Some(-1.0),
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            redacted: false,
            redaction_reason: None,
        }];
        assert!(compute_next_cursor(&results, 50).is_none());
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn next_cursor_present_when_full() {
        let results: Vec<SearchResult> = (0..50)
            .map(|i| SearchResult {
                doc_kind: DocKind::Message,
                id: i,
                project_id: Some(1),
                title: format!("t{i}"),
                body: String::new(),
                score: Some(-(i as f64)),
                importance: None,
                ack_required: None,
                created_ts: None,
                thread_id: None,
                from_agent: None,
                redacted: false,
                redaction_reason: None,
            })
            .collect();
        let cursor = compute_next_cursor(&results, 50).unwrap();
        let decoded = SearchCursor::decode(&cursor).unwrap();
        assert_eq!(decoded.id, 49);
    }

    #[test]
    fn next_cursor_empty_results() {
        assert!(compute_next_cursor(&[], 50).is_none());
    }

    #[test]
    fn search_options_default() {
        let opts = SearchOptions::default();
        assert!(opts.scope_ctx.is_none());
        assert!(opts.redaction_policy.is_none());
        assert!(!opts.track_telemetry);
    }
}
