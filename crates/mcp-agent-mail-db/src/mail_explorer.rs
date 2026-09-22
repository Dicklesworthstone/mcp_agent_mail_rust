//! Unified inbox/outbox explorer data layer.
//!
//! Provides cross-project mailbox exploration with direction-aware filtering,
//! multiple grouping modes, and sort options. Used by TUI and MCP surfaces.
//!
//! # Architecture
//!
//! - [`ExplorerQuery`] describes what to fetch (direction, sort, group, filters).
//! - [`fetch_explorer_page`] runs the query and returns [`ExplorerPage`].
//! - All queries work across projects when `project_id` is `None`.

#[cfg(test)]
#[path = "mail_explorer_regression_tests.rs"]
mod regression_tests;

use serde::{Deserialize, Serialize};

use crate::error::DbError;
use crate::pool::DbPool;
use crate::queries::UNKNOWN_SENDER_DISPLAY;
use crate::tracking::record_query;

use asupersync::{Cx, Outcome};
use sqlmodel_core::{Row as SqlRow, Value};
use sqlmodel_query::raw_query;

// ────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────

/// Direction filter for the explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Direction {
    /// Show only messages received by the agent (inbox).
    Inbound,
    /// Show only messages sent by the agent (outbox).
    Outbound,
    /// Show both sent and received.
    #[default]
    All,
}

/// How to sort results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortMode {
    /// Newest first.
    #[default]
    DateDesc,
    /// Oldest first.
    DateAsc,
    /// Urgent/high first, then by date descending.
    ImportanceDesc,
    /// Group by sender/recipient agent name, then by date.
    AgentAlpha,
}

/// How to group results in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum GroupMode {
    /// Flat list, no grouping.
    #[default]
    None,
    /// Group by project.
    Project,
    /// Group by thread ID.
    Thread,
    /// Group by the other agent (sender for inbox, recipients for outbox).
    Agent,
}

/// Ack-status filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AckFilter {
    /// No ack filtering.
    #[default]
    All,
    /// Only messages with `ack_required = 1` that have NOT been acknowledged.
    PendingAck,
    /// Only messages that have been acknowledged.
    Acknowledged,
    /// Only unread messages.
    Unread,
}

/// A single message entry in explorer results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplorerEntry {
    pub message_id: i64,
    pub project_id: i64,
    pub project_slug: String,
    pub sender_name: String,
    pub to_agents: String,
    pub subject: String,
    pub body_md: String,
    pub thread_id: Option<String>,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: i64,
    /// For inbound messages: "to", "cc", or "bcc".
    pub kind: Option<String>,
    /// Whether the recipient has read this message.
    pub read_ts: Option<i64>,
    /// Whether the recipient has acknowledged this message.
    pub ack_ts: Option<i64>,
    /// Direction of this entry.
    pub direction: Direction,
}

/// A group header for grouped results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplorerGroup {
    pub key: String,
    pub label: String,
    pub count: usize,
    pub entries: Vec<ExplorerEntry>,
}

/// Query parameters for the explorer.
#[derive(Debug, Clone, Default)]
pub struct ExplorerQuery {
    /// Agent name to explore inbox/outbox for. Required.
    pub agent_name: String,
    /// Restrict to a single project. `None` = cross-project.
    pub project_id: Option<i64>,
    /// Direction filter.
    pub direction: Direction,
    /// Sort mode.
    pub sort: SortMode,
    /// Group mode.
    pub group: GroupMode,
    /// Ack-status filter.
    pub ack_filter: AckFilter,
    /// Importance filter (empty = all).
    pub importance_filter: Vec<String>,
    /// Text search within subject/body (empty = no text filter).
    pub text_filter: String,
    /// Maximum entries to return.
    pub limit: usize,
    /// Offset for pagination.
    pub offset: usize,
}

/// Result page from the explorer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplorerPage {
    /// Flat list of entries (regardless of grouping).
    pub entries: Vec<ExplorerEntry>,
    /// Grouped entries (only populated when `group != None`).
    pub groups: Vec<ExplorerGroup>,
    /// Total count matching the filters (before limit/offset).
    pub total_count: usize,
    /// Summary statistics.
    pub stats: ExplorerStats,
}

/// Aggregate stats for the current query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExplorerStats {
    pub inbound_count: usize,
    pub outbound_count: usize,
    pub unread_count: usize,
    pub pending_ack_count: usize,
    pub unique_threads: usize,
    pub unique_projects: usize,
    pub unique_agents: usize,
}

// ────────────────────────────────────────────────────────────────────
// Query execution
// ────────────────────────────────────────────────────────────────────

/// Fetch a page of explorer results.
///
/// This is the primary entry point for all inbox/outbox exploration.
///
/// # Errors
///
/// Returns `DbError` on database or pool errors.
pub async fn fetch_explorer_page(
    cx: &Cx,
    pool: &DbPool,
    query: &ExplorerQuery,
) -> Outcome<ExplorerPage, DbError> {
    let timer = std::time::Instant::now();
    // Validate before any database access: overflow must not become a negative
    // SQLite LIMIT (which means unlimited) or a panic while slicing the page.
    if let Err(error) = candidate_limit(query) {
        return Outcome::Err(error);
    }

    // Resolve agent_id across projects
    let agent_ids = match resolve_agent_ids(cx, pool, &query.agent_name, query.project_id).await {
        Outcome::Ok(ids) => ids,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    if agent_ids.is_empty() {
        return Outcome::Ok(ExplorerPage {
            entries: Vec::new(),
            groups: Vec::new(),
            total_count: 0,
            stats: ExplorerStats::default(),
        });
    }

    // Compute total counts independently of `limit` so pagination metadata is stable.
    let total_inbound = if query.direction == Direction::Outbound {
        0
    } else {
        match count_inbound(cx, pool, &agent_ids, query).await {
            Outcome::Ok(v) => v,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    };
    let total_outbound = if query.direction == Direction::Inbound {
        0
    } else {
        match count_outbound(cx, pool, &agent_ids, query).await {
            Outcome::Ok(v) => v,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    };
    let total_count = match total_inbound.checked_add(total_outbound) {
        Some(total) => total,
        None => {
            return Outcome::Err(DbError::Serialization(
                "mail explorer total count exceeds usize".to_string(),
            ));
        }
    };

    // Fetch inbound and/or outbound entries
    let mut all_entries = Vec::new();

    if query.direction != Direction::Outbound {
        match fetch_inbound(cx, pool, &agent_ids, query).await {
            Outcome::Ok(entries) => all_entries.extend(entries),
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

    if query.direction != Direction::Inbound {
        match fetch_outbound(cx, pool, &agent_ids, query).await {
            Outcome::Ok(entries) => all_entries.extend(entries),
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

    // Compute stats before filtering/sorting.
    // Note: `compute_stats` runs over the fetched entries (which are bounded by SQL LIMITs),
    // but inbound/outbound totals are forced to be correct for pagination metadata.
    let mut stats = compute_stats(&all_entries);
    stats.inbound_count = total_inbound;
    stats.outbound_count = total_outbound;

    // Sort
    sort_entries(&mut all_entries, query.sort);

    // Apply offset + limit
    let start = query.offset.min(all_entries.len());
    let end = start.saturating_add(query.limit).min(all_entries.len());
    let page_entries: Vec<ExplorerEntry> = all_entries[start..end].to_vec();

    // Group if requested
    let groups = if query.group == GroupMode::None {
        Vec::new()
    } else {
        build_groups(&page_entries, query.group)
    };

    record_query(
        "mail_explorer",
        u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX),
    );

    Outcome::Ok(ExplorerPage {
        entries: page_entries,
        groups,
        total_count,
        stats,
    })
}

// ────────────────────────────────────────────────────────────────────
// Internal: resolve agent IDs
// ────────────────────────────────────────────────────────────────────

/// (`project_id`, `agent_id`) pairs for the named agent across projects.
type AgentIds = Vec<(i64, i64)>;

async fn resolve_agent_ids(
    cx: &Cx,
    pool: &DbPool,
    agent_name: &str,
    project_id: Option<i64>,
) -> Outcome<AgentIds, DbError> {
    let (sql, params) = project_id.map_or_else(
        || {
            (
                "SELECT a.project_id, a.id FROM agents a WHERE a.name = ?1 COLLATE NOCASE"
                    .to_string(),
                vec![Value::Text(agent_name.to_string())],
            )
        },
        |pid| {
            (
                "SELECT a.project_id, a.id FROM agents a \
                 WHERE a.name = ?1 COLLATE NOCASE AND a.project_id = ?2"
                    .to_string(),
                vec![Value::Text(agent_name.to_string()), Value::BigInt(pid)],
            )
        },
    );

    let conn = match map_pool_outcome(pool.acquire(cx).await) {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let rows = match map_sql_outcome(raw_query(cx, &*conn, &sql, &params).await) {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    match rows.iter().map(map_agent_row).collect::<Result<AgentIds, _>>() {
        Ok(ids) => Outcome::Ok(ids),
        Err(error) => Outcome::Err(error),
    }
}

// ────────────────────────────────────────────────────────────────────
// Internal: count helpers (stable pagination metadata)
// ────────────────────────────────────────────────────────────────────

async fn count_inbound(
    cx: &Cx,
    pool: &DbPool,
    agent_ids: &AgentIds,
    query: &ExplorerQuery,
) -> Outcome<usize, DbError> {
    if agent_ids.is_empty() {
        return Outcome::Ok(0);
    }

    let conn = match map_pool_outcome(pool.acquire(cx).await) {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let mut total: usize = 0;
    for &(pid, aid) in agent_ids {
        let mut conditions = vec![
            "r.agent_id = ?1".to_string(),
            "m.project_id = ?2".to_string(),
        ];
        let mut params: Vec<Value> = vec![Value::BigInt(aid), Value::BigInt(pid)];
        apply_filters(&mut conditions, &mut params, query, true);
        let where_clause = conditions.join(" AND ");

        let sql = format!(
            "SELECT COUNT(DISTINCT m.id) AS cnt \
             FROM message_recipients r \
             JOIN messages m ON m.id = r.message_id \
             WHERE {where_clause}"
        );
        let rows = match map_sql_outcome(raw_query(cx, &*conn, &sql, &params).await) {
            Outcome::Ok(r) => r,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        let count = match map_count_rows(&rows) {
            Ok(count) => count,
            Err(error) => return Outcome::Err(error),
        };
        total = match total.checked_add(count) {
            Some(total) => total,
            None => {
                return Outcome::Err(DbError::Serialization(
                    "mail explorer total count exceeds usize".to_string(),
                ));
            }
        };
    }

    Outcome::Ok(total)
}

async fn count_outbound(
    cx: &Cx,
    pool: &DbPool,
    agent_ids: &AgentIds,
    query: &ExplorerQuery,
) -> Outcome<usize, DbError> {
    if agent_ids.is_empty() {
        return Outcome::Ok(0);
    }

    let conn = match map_pool_outcome(pool.acquire(cx).await) {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let mut total: usize = 0;
    for &(pid, aid) in agent_ids {
        let mut conditions = vec![
            "m.sender_id = ?1".to_string(),
            "m.project_id = ?2".to_string(),
        ];
        let mut params: Vec<Value> = vec![Value::BigInt(aid), Value::BigInt(pid)];
        apply_filters(&mut conditions, &mut params, query, false);
        let where_clause = conditions.join(" AND ");

        let sql = format!(
            "SELECT COUNT(DISTINCT m.id) AS cnt \
             FROM messages m \
             WHERE {where_clause}"
        );
        let rows = match map_sql_outcome(raw_query(cx, &*conn, &sql, &params).await) {
            Outcome::Ok(r) => r,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        let count = match map_count_rows(&rows) {
            Ok(count) => count,
            Err(error) => return Outcome::Err(error),
        };
        total = match total.checked_add(count) {
            Some(total) => total,
            None => {
                return Outcome::Err(DbError::Serialization(
                    "mail explorer total count exceeds usize".to_string(),
                ));
            }
        };
    }

    Outcome::Ok(total)
}

// ────────────────────────────────────────────────────────────────────
// Internal: fetch inbound messages
// ────────────────────────────────────────────────────────────────────

async fn fetch_inbound(
    cx: &Cx,
    pool: &DbPool,
    agent_ids: &AgentIds,
    query: &ExplorerQuery,
) -> Outcome<Vec<ExplorerEntry>, DbError> {
    if agent_ids.is_empty() {
        return Outcome::Ok(Vec::new());
    }

    let conn = match map_pool_outcome(pool.acquire(cx).await) {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let mut all_entries = Vec::new();

    for &(pid, aid) in agent_ids {
        let mut conditions = vec![
            "r.agent_id = ?1".to_string(),
            "m.project_id = ?2".to_string(),
        ];
        let mut params: Vec<Value> = vec![Value::BigInt(aid), Value::BigInt(pid)];

        apply_filters(&mut conditions, &mut params, query, true);

        let where_clause = conditions.join(" AND ");

        let sql = format!(
            "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
             m.importance, m.ack_required, m.created_ts, \
             r.kind, r.read_ts, r.ack_ts, \
             COALESCE(s.name, '{UNKNOWN_SENDER_DISPLAY}') AS sender_name, \
             COALESCE(NULLIF(TRIM(p.slug), ''), '[unknown-project-' || m.project_id || ']') AS project_slug, \
             COALESCE(GROUP_CONCAT(DISTINCT COALESCE(NULLIF(TRIM(a_recip.name), ''), '[unknown-agent-' || mr2.agent_id || ']')), '') AS to_agents \
             FROM message_recipients r \
             JOIN messages m ON m.id = r.message_id \
             LEFT JOIN agents s ON s.id = m.sender_id \
             LEFT JOIN projects p ON p.id = m.project_id \
             LEFT JOIN message_recipients mr2 ON mr2.message_id = m.id AND mr2.kind IN ('to', 'cc') \
             LEFT JOIN agents a_recip ON a_recip.id = mr2.agent_id \
             WHERE {where_clause} \
             GROUP BY m.id \
             ORDER BY {order_by} \
             LIMIT {limit}",
            order_by = sql_order_by(query.sort, true),
            limit = match candidate_limit(query) {
                Ok(limit) => limit,
                Err(error) => return Outcome::Err(error),
            }
        );

        let rows = match map_sql_outcome(raw_query(cx, &*conn, &sql, &params).await) {
            Outcome::Ok(r) => r,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        for row in &rows {
            match map_inbound_row(row) {
                Ok(entry) => all_entries.push(entry),
                Err(error) => return Outcome::Err(error),
            }
        }
    }

    Outcome::Ok(all_entries)
}

// ────────────────────────────────────────────────────────────────────
// Internal: fetch outbound messages
// ────────────────────────────────────────────────────────────────────

async fn fetch_outbound(
    cx: &Cx,
    pool: &DbPool,
    agent_ids: &AgentIds,
    query: &ExplorerQuery,
) -> Outcome<Vec<ExplorerEntry>, DbError> {
    if agent_ids.is_empty() {
        return Outcome::Ok(Vec::new());
    }

    let conn = match map_pool_outcome(pool.acquire(cx).await) {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let mut all_entries = Vec::new();

    for &(pid, aid) in agent_ids {
        let mut conditions = vec![
            "m.sender_id = ?1".to_string(),
            "m.project_id = ?2".to_string(),
        ];
        let mut params: Vec<Value> = vec![Value::BigInt(aid), Value::BigInt(pid)];

        apply_filters(&mut conditions, &mut params, query, false);

        let where_clause = conditions.join(" AND ");

        let sql = format!(
            "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
             m.importance, m.ack_required, m.created_ts, \
             COALESCE(s.name, '{UNKNOWN_SENDER_DISPLAY}') AS sender_name, \
             COALESCE(NULLIF(TRIM(p.slug), ''), '[unknown-project-' || m.project_id || ']') AS project_slug, \
             COALESCE(GROUP_CONCAT(DISTINCT COALESCE(NULLIF(TRIM(a_recip.name), ''), '[unknown-agent-' || mr.agent_id || ']')), '') AS to_agents \
             FROM messages m \
             LEFT JOIN agents s ON s.id = m.sender_id \
             LEFT JOIN projects p ON p.id = m.project_id \
             LEFT JOIN message_recipients mr ON mr.message_id = m.id \
             LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
             WHERE {where_clause} \
             GROUP BY m.id \
             ORDER BY {order_by} \
             LIMIT {limit}",
            order_by = sql_order_by(query.sort, false),
            limit = match candidate_limit(query) {
                Ok(limit) => limit,
                Err(error) => return Outcome::Err(error),
            }
        );

        let rows = match map_sql_outcome(raw_query(cx, &*conn, &sql, &params).await) {
            Outcome::Ok(r) => r,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        for row in &rows {
            match map_outbound_row(row) {
                Ok(entry) => all_entries.push(entry),
                Err(error) => return Outcome::Err(error),
            }
        }
    }

    Outcome::Ok(all_entries)
}

// ────────────────────────────────────────────────────────────────────
// Internal: filter application
// ────────────────────────────────────────────────────────────────────

fn apply_filters(
    conditions: &mut Vec<String>,
    params: &mut Vec<Value>,
    query: &ExplorerQuery,
    inbound: bool,
) {
    let mut idx = params.len() + 1;

    // Importance filter
    if !query.importance_filter.is_empty() {
        let placeholders: Vec<String> = query
            .importance_filter
            .iter()
            .map(|imp| {
                params.push(Value::Text(imp.clone()));
                let p = format!("?{idx}");
                idx += 1;
                p
            })
            .collect();
        conditions.push(format!("m.importance IN ({})", placeholders.join(",")));
    }

    // Ack filter
    match query.ack_filter {
        AckFilter::All => {}
        AckFilter::PendingAck => {
            conditions.push("m.ack_required = 1".to_string());
            if inbound {
                conditions.push("r.ack_ts IS NULL".to_string());
            } else {
                // Outbound rows do not carry per-recipient ack state.
                conditions.push("0".to_string());
            }
        }
        AckFilter::Acknowledged => {
            conditions.push("m.ack_required = 1".to_string());
            if inbound {
                conditions.push("r.ack_ts IS NOT NULL".to_string());
            } else {
                // Outbound rows do not carry per-recipient ack state.
                conditions.push("0".to_string());
            }
        }
        AckFilter::Unread => {
            if inbound {
                conditions.push("r.read_ts IS NULL".to_string());
            } else {
                // Outbound rows do not carry per-recipient read state.
                conditions.push("0".to_string());
            }
        }
    }

    // Text filter (LIKE fallback)
    if !query.text_filter.is_empty() {
        let escaped = query
            .text_filter
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        params.push(Value::Text(format!("%{escaped}%")));
        conditions.push(format!(
            "(m.subject LIKE ?{idx} ESCAPE '\\' OR m.body_md LIKE ?{idx} ESCAPE '\\')"
        ));
    }
}

// ────────────────────────────────────────────────────────────────────
// Internal: row mapping
// ────────────────────────────────────────────────────────────────────

// Missing or mistyped database fields are not an empty mailbox, a normal
// priority, or an unread receipt. Preserve real SQL NULLs, but fail the whole
// projection when its schema/values cannot be decoded.
fn explorer_decode_error(error: sqlmodel_core::Error) -> DbError {
    DbError::Serialization(format!("invalid mail explorer database projection: {error}"))
}

fn map_agent_row(row: &SqlRow) -> Result<(i64, i64), DbError> {
    Ok((
        row.get_named("project_id").map_err(explorer_decode_error)?,
        row.get_named("id").map_err(explorer_decode_error)?,
    ))
}

fn map_count_rows(rows: &[SqlRow]) -> Result<usize, DbError> {
    let row = rows.first().ok_or_else(|| {
        DbError::Serialization("mail explorer count query returned no row".to_string())
    })?;
    let count = row.get_named::<i64>("cnt").map_err(explorer_decode_error)?;
    usize::try_from(count).map_err(|_| {
        DbError::Serialization("mail explorer count is negative or exceeds usize".to_string())
    })
}

fn map_inbound_row(row: &SqlRow) -> Result<ExplorerEntry, DbError> {
    let message_id: i64 = row.get_as(0).map_err(explorer_decode_error)?;
    Ok(ExplorerEntry {
        message_id,
        project_id: row.get_as(1).map_err(explorer_decode_error)?,
        project_slug: row.get_as(13).map_err(explorer_decode_error)?,
        sender_name: row.get_as(12).map_err(explorer_decode_error)?,
        to_agents: row.get_as(14).map_err(explorer_decode_error)?,
        subject: row.get_as(4).map_err(explorer_decode_error)?,
        body_md: row.get_as(5).map_err(explorer_decode_error)?,
        thread_id: row.get_as::<Option<String>>(3).map_err(explorer_decode_error)?,
        importance: row.get_as(6).map_err(explorer_decode_error)?,
        ack_required: row.get_as::<i64>(7).map_err(explorer_decode_error)? != 0,
        created_ts: row.get_as(8).map_err(explorer_decode_error)?,
        kind: Some(row.get_as(9).map_err(explorer_decode_error)?),
        read_ts: row.get_as::<Option<i64>>(10).map_err(explorer_decode_error)?,
        ack_ts: row.get_as::<Option<i64>>(11).map_err(explorer_decode_error)?,
        direction: Direction::Inbound,
    })
}

fn map_outbound_row(row: &SqlRow) -> Result<ExplorerEntry, DbError> {
    let message_id: i64 = row.get_as(0).map_err(explorer_decode_error)?;
    Ok(ExplorerEntry {
        message_id,
        project_id: row.get_as(1).map_err(explorer_decode_error)?,
        project_slug: row.get_as(10).map_err(explorer_decode_error)?,
        sender_name: row.get_as(9).map_err(explorer_decode_error)?,
        to_agents: row.get_as(11).map_err(explorer_decode_error)?,
        subject: row.get_as(4).map_err(explorer_decode_error)?,
        body_md: row.get_as(5).map_err(explorer_decode_error)?,
        thread_id: row.get_as::<Option<String>>(3).map_err(explorer_decode_error)?,
        importance: row.get_as(6).map_err(explorer_decode_error)?,
        ack_required: row.get_as::<i64>(7).map_err(explorer_decode_error)? != 0,
        created_ts: row.get_as(8).map_err(explorer_decode_error)?,
        kind: None,
        read_ts: None,
        ack_ts: None,
        direction: Direction::Outbound,
    })
}

// ────────────────────────────────────────────────────────────────────
// Internal: sorting
// ────────────────────────────────────────────────────────────────────

/// Each project/direction must select its best offset + limit candidates using
/// the SAME order as the final merge. Sorting a newest-only subset afterward
/// loses old, urgent and alphabetically earlier messages permanently.
fn sql_order_by(mode: SortMode, inbound: bool) -> &'static str {
    match mode {
        SortMode::DateDesc => "m.created_ts DESC, m.id DESC",
        SortMode::DateAsc => "m.created_ts ASC, m.id ASC",
        SortMode::ImportanceDesc => {
            "CASE m.importance COLLATE BINARY WHEN 'urgent' THEN 4 WHEN 'high' THEN 3 WHEN 'normal' THEN 2 WHEN 'low' THEN 1 ELSE 0 END DESC, m.created_ts DESC, m.id DESC"
        }
        // SQLite NOCASE and the merge comparator both fold ASCII only.
        SortMode::AgentAlpha if inbound => {
            "sender_name COLLATE NOCASE ASC, m.created_ts DESC, m.id DESC"
        }
        SortMode::AgentAlpha => {
            "to_agents COLLATE NOCASE ASC, m.created_ts DESC, m.id DESC"
        }
    }
}

fn candidate_limit(query: &ExplorerQuery) -> Result<i64, DbError> {
    if query.limit == 0 {
        return Ok(0);
    }
    query
        .offset
        .checked_add(query.limit)
        .and_then(|limit| i64::try_from(limit).ok())
        .ok_or_else(|| DbError::InvalidArgument {
            field: "pagination",
            message: "offset + limit exceeds the supported SQLite integer range".to_string(),
        })
}

fn sort_entries(entries: &mut [ExplorerEntry], mode: SortMode) {
    entries.sort_by(|a, b| {
        let primary = match mode {
            SortMode::DateDesc => b.created_ts.cmp(&a.created_ts),
            SortMode::DateAsc => a.created_ts.cmp(&b.created_ts),
            SortMode::ImportanceDesc => importance_rank(&b.importance)
                .cmp(&importance_rank(&a.importance))
                .then_with(|| b.created_ts.cmp(&a.created_ts)),
            SortMode::AgentAlpha => {
                let agent_a = if a.direction == Direction::Inbound {
                    &a.sender_name
                } else {
                    &a.to_agents
                };
                let agent_b = if b.direction == Direction::Inbound {
                    &b.sender_name
                } else {
                    &b.to_agents
                };
                agent_a
                    .bytes()
                    .map(|byte| byte.to_ascii_lowercase())
                    .cmp(agent_b.bytes().map(|byte| byte.to_ascii_lowercase()))
                    .then_with(|| b.created_ts.cmp(&a.created_ts))
            }
        };
        primary
            .then_with(|| {
                if mode == SortMode::DateAsc {
                    a.message_id.cmp(&b.message_id)
                } else {
                    b.message_id.cmp(&a.message_id)
                }
            })
            // Self-addressed mail can appear once in each direction. Give it
            // stable page membership independently of project iteration order.
            .then_with(|| {
                let rank = |direction| match direction {
                    Direction::Inbound => 0_u8,
                    Direction::Outbound => 1,
                    Direction::All => 2,
                };
                rank(a.direction).cmp(&rank(b.direction))
            })
    });
}

fn importance_rank(imp: &str) -> u8 {
    match imp {
        "urgent" => 4,
        "high" => 3,
        "normal" => 2,
        "low" => 1,
        _ => 0,
    }
}

// ────────────────────────────────────────────────────────────────────
// Internal: grouping
// ────────────────────────────────────────────────────────────────────

fn build_groups(entries: &[ExplorerEntry], mode: GroupMode) -> Vec<ExplorerGroup> {
    use std::collections::BTreeMap;

    let mut map: BTreeMap<String, Vec<ExplorerEntry>> = BTreeMap::new();

    for entry in entries {
        let key = match mode {
            GroupMode::None => unreachable!(),
            GroupMode::Project => entry.project_slug.clone(),
            GroupMode::Thread => entry
                .thread_id
                .clone()
                .unwrap_or_else(|| "(no thread)".to_string()),
            GroupMode::Agent => {
                if entry.direction == Direction::Inbound {
                    entry.sender_name.clone()
                } else {
                    entry.to_agents.clone()
                }
            }
        };
        map.entry(key).or_default().push(entry.clone());
    }

    map.into_iter()
        .map(|(key, entries)| {
            let count = entries.len();
            let label = match mode {
                GroupMode::Project => format!("Project: {key}"),
                GroupMode::Thread => format!("Thread: {key}"),
                GroupMode::Agent => format!("Agent: {key}"),
                GroupMode::None => key.clone(),
            };
            ExplorerGroup {
                key,
                label,
                count,
                entries,
            }
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────
// Internal: stats
// ────────────────────────────────────────────────────────────────────

fn compute_stats(entries: &[ExplorerEntry]) -> ExplorerStats {
    use std::collections::HashSet;

    let mut projects = HashSet::new();
    let mut threads = HashSet::new();
    let mut agents = HashSet::new();
    let mut inbound = 0usize;
    let mut outbound = 0usize;
    let mut unread = 0usize;
    let mut pending_ack = 0usize;

    for e in entries {
        projects.insert(e.project_id);
        if let Some(ref tid) = e.thread_id {
            threads.insert(tid.clone());
        }
        collect_agent_names(&mut agents, &e.sender_name);
        collect_agent_names(&mut agents, &e.to_agents);

        if e.direction == Direction::Inbound {
            inbound += 1;
            if e.read_ts.is_none() {
                unread += 1;
            }
            if e.ack_required && e.ack_ts.is_none() {
                pending_ack += 1;
            }
        } else {
            outbound += 1;
        }
    }

    ExplorerStats {
        inbound_count: inbound,
        outbound_count: outbound,
        unread_count: unread,
        pending_ack_count: pending_ack,
        unique_threads: threads.len(),
        unique_projects: projects.len(),
        unique_agents: agents.len(),
    }
}

fn collect_agent_names(agents: &mut std::collections::HashSet<String>, names_csv: &str) {
    for raw in names_csv.split(',') {
        let name = raw.trim();
        if !name.is_empty() {
            agents.insert(name.to_string());
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

fn map_sql_outcome<T>(out: Outcome<T, sqlmodel_core::Error>) -> Outcome<T, DbError> {
    match out {
        Outcome::Ok(v) => Outcome::Ok(v),
        Outcome::Err(e) => Outcome::Err(DbError::Sqlite(e.to_string())),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

fn map_pool_outcome<T>(out: Outcome<T, sqlmodel_core::Error>) -> Outcome<T, DbError> {
    map_sql_outcome(out)
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_default_is_all() {
        assert_eq!(Direction::default(), Direction::All);
    }

    #[test]
    fn sort_mode_default_is_date_desc() {
        assert_eq!(SortMode::default(), SortMode::DateDesc);
    }

    #[test]
    fn group_mode_default_is_none() {
        assert_eq!(GroupMode::default(), GroupMode::None);
    }

    #[test]
    fn ack_filter_default_is_all() {
        assert_eq!(AckFilter::default(), AckFilter::All);
    }

    #[test]
    fn importance_rank_ordering() {
        assert!(importance_rank("urgent") > importance_rank("high"));
        assert!(importance_rank("high") > importance_rank("normal"));
        assert!(importance_rank("normal") > importance_rank("low"));
        assert!(importance_rank("low") > importance_rank("unknown"));
    }

    #[test]
    fn sort_entries_date_desc() {
        let mut entries = vec![
            test_entry(1, 100, Direction::Inbound),
            test_entry(2, 300, Direction::Outbound),
            test_entry(3, 200, Direction::Inbound),
        ];
        sort_entries(&mut entries, SortMode::DateDesc);
        assert_eq!(entries[0].message_id, 2);
        assert_eq!(entries[1].message_id, 3);
        assert_eq!(entries[2].message_id, 1);
    }

    #[test]
    fn sort_entries_date_asc() {
        let mut entries = vec![
            test_entry(1, 100, Direction::Inbound),
            test_entry(2, 300, Direction::Outbound),
            test_entry(3, 200, Direction::Inbound),
        ];
        sort_entries(&mut entries, SortMode::DateAsc);
        assert_eq!(entries[0].message_id, 1);
        assert_eq!(entries[1].message_id, 3);
        assert_eq!(entries[2].message_id, 2);
    }

    #[test]
    fn sort_entries_importance() {
        let mut entries = vec![
            test_entry_with_importance(1, 100, "normal"),
            test_entry_with_importance(2, 200, "urgent"),
            test_entry_with_importance(3, 300, "high"),
        ];
        sort_entries(&mut entries, SortMode::ImportanceDesc);
        assert_eq!(entries[0].message_id, 2); // urgent
        assert_eq!(entries[1].message_id, 3); // high
        assert_eq!(entries[2].message_id, 1); // normal
    }

    #[test]
    fn compute_stats_basic() {
        let entries = vec![
            test_entry(1, 100, Direction::Inbound),
            test_entry(2, 200, Direction::Outbound),
            test_entry(3, 300, Direction::Inbound),
        ];
        let stats = compute_stats(&entries);
        assert_eq!(stats.inbound_count, 2);
        assert_eq!(stats.outbound_count, 1);
    }

    #[test]
    fn compute_stats_unread_and_ack() {
        let mut e1 = test_entry(1, 100, Direction::Inbound);
        e1.read_ts = None;
        e1.ack_required = true;
        e1.ack_ts = None;

        let mut e2 = test_entry(2, 200, Direction::Inbound);
        e2.read_ts = Some(150);
        e2.ack_required = true;
        e2.ack_ts = Some(160);

        let stats = compute_stats(&[e1, e2]);
        assert_eq!(stats.unread_count, 1);
        assert_eq!(stats.pending_ack_count, 1);
    }

    #[test]
    fn build_groups_by_project() {
        let mut e1 = test_entry(1, 100, Direction::Inbound);
        e1.project_slug = "project-a".to_string();
        let mut e2 = test_entry(2, 200, Direction::Inbound);
        e2.project_slug = "project-b".to_string();
        let mut e3 = test_entry(3, 300, Direction::Inbound);
        e3.project_slug = "project-a".to_string();

        let groups = build_groups(&[e1, e2, e3], GroupMode::Project);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key, "project-a");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[1].key, "project-b");
        assert_eq!(groups[1].count, 1);
    }

    #[test]
    fn build_groups_by_thread() {
        let mut e1 = test_entry(1, 100, Direction::Inbound);
        e1.thread_id = Some("thread-1".to_string());
        let mut e2 = test_entry(2, 200, Direction::Inbound);
        e2.thread_id = None;
        let mut e3 = test_entry(3, 300, Direction::Inbound);
        e3.thread_id = Some("thread-1".to_string());

        let groups = build_groups(&[e1, e2, e3], GroupMode::Thread);
        assert_eq!(groups.len(), 2);
        // BTreeMap ordering: "(no thread)" < "thread-1"
        assert_eq!(groups[0].key, "(no thread)");
        assert_eq!(groups[0].count, 1);
        assert_eq!(groups[1].key, "thread-1");
        assert_eq!(groups[1].count, 2);
    }

    #[test]
    fn build_groups_by_agent() {
        let mut e1 = test_entry(1, 100, Direction::Inbound);
        e1.sender_name = "RedFox".to_string();
        let mut e2 = test_entry(2, 200, Direction::Outbound);
        e2.to_agents = "BlueLake".to_string();
        let mut e3 = test_entry(3, 300, Direction::Inbound);
        e3.sender_name = "RedFox".to_string();

        let groups = build_groups(&[e1, e2, e3], GroupMode::Agent);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn explorer_query_defaults() {
        let q = ExplorerQuery::default();
        assert_eq!(q.direction, Direction::All);
        assert_eq!(q.sort, SortMode::DateDesc);
        assert_eq!(q.group, GroupMode::None);
        assert_eq!(q.ack_filter, AckFilter::All);
        assert_eq!(q.importance_filter, [] as [String; 0]);
        assert_eq!(q.text_filter, "");
        assert_eq!(q.limit, 0);
        assert_eq!(q.offset, 0);
    }

    #[test]
    fn explorer_page_empty() {
        let page = ExplorerPage {
            entries: Vec::new(),
            groups: Vec::new(),
            total_count: 0,
            stats: ExplorerStats::default(),
        };
        assert_eq!(page.total_count, 0);
        assert!(page.entries.is_empty());
    }

    #[test]
    fn apply_filters_importance() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            importance_filter: vec!["high".to_string(), "urgent".to_string()],
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        assert_eq!(conditions.len(), 2);
        assert!(conditions[1].contains("m.importance IN"));
        assert_eq!(params.len(), 3); // 1 original + 2 importance
    }

    #[test]
    fn apply_filters_text() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            text_filter: "hello%world".to_string(),
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        assert_eq!(conditions.len(), 2);
        assert!(conditions[1].contains("LIKE"));
        // Check that % was escaped
        if let Value::Text(ref s) = params[1] {
            assert!(s.contains("hello\\%world"));
        } else {
            panic!("expected text param");
        }
    }

    // ── Test helpers ────────────────────────────────────────────

    fn test_entry(id: i64, ts: i64, direction: Direction) -> ExplorerEntry {
        ExplorerEntry {
            message_id: id,
            project_id: 1,
            project_slug: "test-project".to_string(),
            sender_name: "TestAgent".to_string(),
            to_agents: "OtherAgent".to_string(),
            subject: format!("Subject {id}"),
            body_md: String::new(),
            thread_id: None,
            importance: "normal".to_string(),
            ack_required: false,
            created_ts: ts,
            kind: None,
            read_ts: None,
            ack_ts: None,
            direction,
        }
    }

    fn test_entry_with_importance(id: i64, ts: i64, importance: &str) -> ExplorerEntry {
        ExplorerEntry {
            importance: importance.to_string(),
            ..test_entry(id, ts, Direction::Inbound)
        }
    }

    // ── New tests: sorting ─────────────────────────────────────

    #[test]
    fn sort_entries_agent_alpha() {
        let mut entries = vec![
            {
                let mut e = test_entry(1, 100, Direction::Inbound);
                e.sender_name = "ZuluFox".to_string();
                e
            },
            {
                let mut e = test_entry(2, 200, Direction::Outbound);
                e.to_agents = "AlphaWolf".to_string();
                e
            },
            {
                let mut e = test_entry(3, 300, Direction::Inbound);
                e.sender_name = "MidLake".to_string();
                e
            },
        ];
        sort_entries(&mut entries, SortMode::AgentAlpha);
        // AlphaWolf < MidLake < ZuluFox (case-insensitive)
        assert_eq!(entries[0].message_id, 2); // AlphaWolf
        assert_eq!(entries[1].message_id, 3); // MidLake
        assert_eq!(entries[2].message_id, 1); // ZuluFox
    }

    #[test]
    fn sort_entries_agent_alpha_tiebreak_by_date() {
        let mut entries = vec![
            {
                let mut e = test_entry(1, 100, Direction::Inbound);
                e.sender_name = "RedFox".to_string();
                e
            },
            {
                let mut e = test_entry(2, 300, Direction::Inbound);
                e.sender_name = "RedFox".to_string();
                e
            },
        ];
        sort_entries(&mut entries, SortMode::AgentAlpha);
        // Same agent name → tiebreak by date descending (newer first)
        assert_eq!(entries[0].message_id, 2); // ts=300
        assert_eq!(entries[1].message_id, 1); // ts=100
    }

    #[test]
    fn sort_entries_importance_tiebreak_by_date() {
        let entries_before = vec![
            test_entry_with_importance(1, 100, "urgent"),
            test_entry_with_importance(2, 300, "urgent"),
            test_entry_with_importance(3, 200, "urgent"),
        ];
        let mut entries = entries_before;
        sort_entries(&mut entries, SortMode::ImportanceDesc);
        // All urgent → tiebreak by date descending
        assert_eq!(entries[0].message_id, 2); // ts=300
        assert_eq!(entries[1].message_id, 3); // ts=200
        assert_eq!(entries[2].message_id, 1); // ts=100
    }

    #[test]
    fn sort_entries_empty_slice() {
        let mut entries: Vec<ExplorerEntry> = Vec::new();
        sort_entries(&mut entries, SortMode::DateDesc);
        assert!(entries.is_empty());
    }

    // ── New tests: apply_filters ────────────────────────────────

    #[test]
    fn apply_filters_ack_pending() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            ack_filter: AckFilter::PendingAck,
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        assert!(conditions.iter().any(|c| c.contains("m.ack_required = 1")));
        assert!(conditions.iter().any(|c| c.contains("r.ack_ts IS NULL")));
    }

    #[test]
    fn apply_filters_ack_acknowledged() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            ack_filter: AckFilter::Acknowledged,
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        assert!(conditions.iter().any(|c| c.contains("m.ack_required = 1")));
        assert!(
            conditions
                .iter()
                .any(|c| c.contains("r.ack_ts IS NOT NULL"))
        );
    }

    #[test]
    fn apply_filters_unread_adds_read_condition() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            ack_filter: AckFilter::Unread,
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        assert!(conditions.iter().any(|c| c.contains("r.read_ts IS NULL")));
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn apply_filters_outbound_ack_filter_returns_no_rows() {
        let mut conditions = vec!["m.sender_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            ack_filter: AckFilter::PendingAck,
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, false);
        assert!(conditions.iter().any(|c| c == "0"));
    }

    #[test]
    fn apply_filters_text_escapes_underscore() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            text_filter: "hello_world".to_string(),
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        if let Value::Text(ref s) = params[1] {
            assert!(
                s.contains("hello\\_world"),
                "underscore should be escaped: {s}"
            );
        } else {
            panic!("expected text param");
        }
    }

    #[test]
    fn apply_filters_text_escapes_backslash() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            text_filter: r"folder\name".to_string(),
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        if let Value::Text(ref s) = params[1] {
            assert!(
                s.contains(r"folder\\name"),
                "backslash should be escaped for LIKE/ESCAPE: {s}"
            );
        } else {
            panic!("expected text param");
        }
    }

    #[test]
    fn apply_filters_combined_importance_and_text() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery {
            importance_filter: vec!["urgent".to_string()],
            text_filter: "auth".to_string(),
            ..Default::default()
        };
        apply_filters(&mut conditions, &mut params, &query, true);
        // Should have: base + importance IN + LIKE
        assert_eq!(conditions.len(), 3);
        assert!(conditions[1].contains("m.importance IN"));
        assert!(conditions[2].contains("LIKE"));
        // params: base + "urgent" + "%auth%"
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn apply_filters_empty_query_is_noop() {
        let mut conditions = vec!["r.agent_id = ?1".to_string()];
        let mut params: Vec<Value> = vec![Value::BigInt(1)];
        let query = ExplorerQuery::default();
        apply_filters(&mut conditions, &mut params, &query, true);
        assert_eq!(conditions.len(), 1);
        assert_eq!(params.len(), 1);
    }

    // ── New tests: compute_stats ────────────────────────────────

    #[test]
    fn compute_stats_empty() {
        let stats = compute_stats(&[]);
        assert_eq!(stats.inbound_count, 0);
        assert_eq!(stats.outbound_count, 0);
        assert_eq!(stats.unread_count, 0);
        assert_eq!(stats.pending_ack_count, 0);
        assert_eq!(stats.unique_threads, 0);
        assert_eq!(stats.unique_projects, 0);
        assert_eq!(stats.unique_agents, 0);
    }

    #[test]
    fn compute_stats_unique_counting() {
        let mut e1 = test_entry(1, 100, Direction::Inbound);
        e1.project_id = 10;
        e1.sender_name = "RedFox".to_string();
        e1.thread_id = Some("t1".to_string());

        let mut e2 = test_entry(2, 200, Direction::Outbound);
        e2.project_id = 20;
        e2.sender_name = "BlueLake".to_string();
        e2.thread_id = Some("t1".to_string()); // same thread

        let mut e3 = test_entry(3, 300, Direction::Inbound);
        e3.project_id = 10; // same project as e1
        e3.sender_name = "RedFox".to_string(); // same agent as e1
        e3.thread_id = Some("t2".to_string());

        let stats = compute_stats(&[e1, e2, e3]);
        assert_eq!(stats.unique_projects, 2); // 10, 20
        assert_eq!(stats.unique_threads, 2); // t1, t2
        assert_eq!(stats.unique_agents, 3); // RedFox, BlueLake, OtherAgent
    }

    #[test]
    fn compute_stats_parses_multiple_recipient_agents() {
        let mut e = test_entry(1, 100, Direction::Outbound);
        e.sender_name = "BlueLake".to_string();
        e.to_agents = "GreenCastle, RedFox, BlueLake".to_string();
        let stats = compute_stats(&[e]);
        assert_eq!(stats.unique_agents, 3);
    }

    #[test]
    fn compute_stats_outbound_not_counted_as_unread() {
        let e = test_entry(1, 100, Direction::Outbound);
        // Outbound message with read_ts=None should NOT count as unread
        let stats = compute_stats(&[e]);
        assert_eq!(stats.unread_count, 0);
        assert_eq!(stats.outbound_count, 1);
    }

    // ── New tests: build_groups ─────────────────────────────────

    #[test]
    fn build_groups_empty() {
        let groups = build_groups(&[], GroupMode::Project);
        assert!(groups.is_empty());
    }

    #[test]
    fn build_groups_label_format() {
        let mut e = test_entry(1, 100, Direction::Inbound);
        e.project_slug = "my-project".to_string();
        let groups = build_groups(&[e], GroupMode::Project);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "Project: my-project");
        assert_eq!(groups[0].key, "my-project");
    }

    #[test]
    fn build_groups_thread_label_format() {
        let mut e = test_entry(1, 100, Direction::Inbound);
        e.thread_id = Some("bd-123".to_string());
        let groups = build_groups(&[e], GroupMode::Thread);
        assert_eq!(groups[0].label, "Thread: bd-123");
    }

    #[test]
    fn build_groups_agent_label_format() {
        let mut e = test_entry(1, 100, Direction::Inbound);
        e.sender_name = "RedFox".to_string();
        let groups = build_groups(&[e], GroupMode::Agent);
        assert_eq!(groups[0].label, "Agent: RedFox");
    }

    // ── New tests: serde roundtrips ─────────────────────────────

    #[test]
    fn direction_serde_roundtrip() {
        for dir in [Direction::Inbound, Direction::Outbound, Direction::All] {
            let json = serde_json::to_string(&dir).unwrap();
            let parsed: Direction = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, dir);
        }
    }

    #[test]
    fn sort_mode_serde_roundtrip() {
        for mode in [
            SortMode::DateDesc,
            SortMode::DateAsc,
            SortMode::ImportanceDesc,
            SortMode::AgentAlpha,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let parsed: SortMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn group_mode_serde_roundtrip() {
        for mode in [
            GroupMode::None,
            GroupMode::Project,
            GroupMode::Thread,
            GroupMode::Agent,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let parsed: GroupMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn ack_filter_serde_roundtrip() {
        for filter in [
            AckFilter::All,
            AckFilter::PendingAck,
            AckFilter::Acknowledged,
            AckFilter::Unread,
        ] {
            let json = serde_json::to_string(&filter).unwrap();
            let parsed: AckFilter = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, filter);
        }
    }

    #[test]
    fn explorer_entry_serde_roundtrip() {
        let entry = test_entry(42, 1_000_000, Direction::Inbound);
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: ExplorerEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.message_id, 42);
        assert_eq!(parsed.created_ts, 1_000_000);
    }

    #[test]
    fn explorer_stats_default() {
        let stats = ExplorerStats::default();
        assert_eq!(stats.inbound_count, 0);
        assert_eq!(stats.outbound_count, 0);
        assert_eq!(stats.unread_count, 0);
        assert_eq!(stats.pending_ack_count, 0);
        assert_eq!(stats.unique_threads, 0);
        assert_eq!(stats.unique_projects, 0);
        assert_eq!(stats.unique_agents, 0);
    }

    #[test]
    fn explorer_page_serde_roundtrip() {
        let page = ExplorerPage {
            entries: vec![test_entry(1, 100, Direction::Inbound)],
            groups: vec![ExplorerGroup {
                key: "proj-a".to_string(),
                label: "Project: proj-a".to_string(),
                count: 1,
                entries: vec![test_entry(1, 100, Direction::Inbound)],
            }],
            total_count: 1,
            stats: ExplorerStats::default(),
        };
        let json = serde_json::to_string(&page).unwrap();
        let parsed: ExplorerPage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_count, 1);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.groups.len(), 1);
        assert_eq!(parsed.groups[0].count, 1);
    }

    // ── New tests: importance_rank ──────────────────────────────

    #[test]
    fn importance_rank_unknown_returns_zero() {
        assert_eq!(importance_rank(""), 0);
        assert_eq!(importance_rank("critical"), 0);
        assert_eq!(importance_rank("URGENT"), 0); // case-sensitive
    }
}
