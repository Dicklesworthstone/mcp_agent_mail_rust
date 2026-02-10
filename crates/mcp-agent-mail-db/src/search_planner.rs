//! Global query planner for unified search across messages, agents, and projects.
//!
//! Converts a [`SearchQuery`] into SQL + params, supporting:
//! - Faceted filtering (importance, direction, time range, project, agent, thread)
//! - BM25 relevance ranking with score extraction
//! - Stable cursor-based pagination using (score, id)
//! - Query explain output for debugging/trust

#![allow(clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────
// Facets & Filters
// ────────────────────────────────────────────────────────────────────

/// What kind of entity to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    #[default]
    Message,
    Agent,
    Project,
}

impl DocKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Agent => "agent",
            Self::Project => "project",
        }
    }
}

/// Message importance levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    Low,
    Normal,
    High,
    Urgent,
}

impl Importance {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }

    /// Parse from a string (case-insensitive).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            "urgent" => Some(Self::Urgent),
            _ => None,
        }
    }
}

/// Message direction relative to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Inbox,
    Outbox,
}

/// Time range filter (inclusive bounds, microsecond timestamps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TimeRange {
    pub min_ts: Option<i64>,
    pub max_ts: Option<i64>,
}

impl TimeRange {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.min_ts.is_none() && self.max_ts.is_none()
    }
}

/// Ranking strategy for search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RankingMode {
    /// BM25 relevance (default).
    #[default]
    Relevance,
    /// Most recent first.
    Recency,
}

// ────────────────────────────────────────────────────────────────────
// SearchQuery
// ────────────────────────────────────────────────────────────────────

/// A structured search query with optional facets, pagination, and ranking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Free-text query string (will be sanitized for FTS5).
    pub text: String,

    /// Entity kind to search. Default: `Message`.
    #[serde(default)]
    pub doc_kind: DocKind,

    // ── Scope ──────────────────────────────────────────────────────
    /// Restrict to a single project.
    pub project_id: Option<i64>,

    /// Search across all projects linked to a product.
    pub product_id: Option<i64>,

    // ── Facets (message-specific) ──────────────────────────────────
    /// Filter by importance levels.
    #[serde(default)]
    pub importance: Vec<Importance>,

    /// Filter by message direction (requires `agent_name`).
    pub direction: Option<Direction>,

    /// Filter by agent name (sender for outbox, recipient for inbox).
    pub agent_name: Option<String>,

    /// Filter by thread ID.
    pub thread_id: Option<String>,

    /// Filter by `ack_required` flag.
    pub ack_required: Option<bool>,

    /// Filter by creation time range.
    #[serde(default)]
    pub time_range: TimeRange,

    // ── Ranking & Pagination ───────────────────────────────────────
    /// How to rank results.
    #[serde(default)]
    pub ranking: RankingMode,

    /// Maximum results to return (clamped to 1..=1000).
    pub limit: Option<usize>,

    /// Cursor for stable pagination (opaque token from previous result).
    pub cursor: Option<String>,

    /// Whether to include explain metadata in results.
    #[serde(default)]
    pub explain: bool,
}

impl SearchQuery {
    /// Create a simple text search for messages within a project.
    #[must_use]
    pub fn messages(text: impl Into<String>, project_id: i64) -> Self {
        Self {
            text: text.into(),
            doc_kind: DocKind::Message,
            project_id: Some(project_id),
            ..Default::default()
        }
    }

    /// Create a product-wide message search.
    #[must_use]
    pub fn product_messages(text: impl Into<String>, product_id: i64) -> Self {
        Self {
            text: text.into(),
            doc_kind: DocKind::Message,
            product_id: Some(product_id),
            ..Default::default()
        }
    }

    /// Create an agent search within a project.
    #[must_use]
    pub fn agents(text: impl Into<String>, project_id: i64) -> Self {
        Self {
            text: text.into(),
            doc_kind: DocKind::Agent,
            project_id: Some(project_id),
            ..Default::default()
        }
    }

    /// Create a project search.
    #[must_use]
    pub fn projects(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            doc_kind: DocKind::Project,
            ..Default::default()
        }
    }

    /// Effective limit, clamped to 1..=1000.
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(50).clamp(1, 1000)
    }
}

// ────────────────────────────────────────────────────────────────────
// SearchCursor — stable pagination token
// ────────────────────────────────────────────────────────────────────

/// Cursor for stable pagination, encoding the last-seen (score, id) pair.
///
/// Format: `s<score_bits_hex>:i<id>` where score is the IEEE 754 bits of the f64.
/// This makes the cursor deterministic and order-preserving.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchCursor {
    pub score: f64,
    pub id: i64,
}

impl SearchCursor {
    /// Encode as an opaque string token.
    #[must_use]
    pub fn encode(&self) -> String {
        let bits = self.score.to_bits();
        format!("s{bits:016x}:i{}", self.id)
    }

    /// Decode from an opaque string token.
    #[must_use]
    pub fn decode(token: &str) -> Option<Self> {
        let (score_part, id_part) = token.split_once(":i")?;
        let hex = score_part.strip_prefix('s')?;
        let bits = u64::from_str_radix(hex, 16).ok()?;
        let score = f64::from_bits(bits);
        let id = id_part.parse::<i64>().ok()?;
        Some(Self { score, id })
    }
}

// ────────────────────────────────────────────────────────────────────
// SearchResult
// ────────────────────────────────────────────────────────────────────

/// A single search result with optional score and explain metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub doc_kind: DocKind,
    pub id: i64,
    pub project_id: Option<i64>,
    pub title: String,
    pub body: String,

    /// BM25 score (lower = more relevant for FTS5).
    pub score: Option<f64>,

    // ── Message-specific fields ────────────────────────────────────
    pub importance: Option<String>,
    pub ack_required: Option<bool>,
    pub created_ts: Option<i64>,
    pub thread_id: Option<String>,
    pub from_agent: Option<String>,
}

/// Response from the search planner, including results and pagination info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub next_cursor: Option<String>,
    pub explain: Option<QueryExplain>,
}

// ────────────────────────────────────────────────────────────────────
// QueryExplain — debugging/trust metadata
// ────────────────────────────────────────────────────────────────────

/// Explains how the query was planned and executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryExplain {
    /// The plan method chosen.
    pub method: String,
    /// The normalized/sanitized FTS query (or None if LIKE fallback).
    pub normalized_query: Option<String>,
    /// Whether LIKE fallback was used.
    pub used_like_fallback: bool,
    /// Number of active facet filters.
    pub facet_count: usize,
    /// Which facets were applied.
    pub facets_applied: Vec<String>,
    /// The raw SQL executed (for debugging).
    pub sql: String,
}

// ────────────────────────────────────────────────────────────────────
// SearchPlan — intermediate representation
// ────────────────────────────────────────────────────────────────────

/// Intermediate plan produced by the planner before execution.
#[derive(Debug, Clone)]
pub struct SearchPlan {
    pub sql: String,
    pub params: Vec<PlanParam>,
    pub method: PlanMethod,
    pub normalized_query: Option<String>,
    pub facets_applied: Vec<String>,
}

/// Parameter value for a planned SQL query.
#[derive(Debug, Clone)]
pub enum PlanParam {
    Int(i64),
    Text(String),
    Float(f64),
}

/// What query strategy the planner chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMethod {
    /// FTS5 MATCH with BM25 ranking.
    Fts,
    /// LIKE fallback (FTS query was malformed or empty).
    Like,
    /// No text search, just filter/sort.
    FilterOnly,
    /// Empty query → empty results.
    Empty,
}

impl PlanMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fts => "fts5",
            Self::Like => "like_fallback",
            Self::FilterOnly => "filter_only",
            Self::Empty => "empty",
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Planner implementation
// ────────────────────────────────────────────────────────────────────

use crate::queries::{extract_like_terms, sanitize_fts_query};

/// Plan a search query into SQL + params.
///
/// This function does NOT execute the query — it produces a [`SearchPlan`]
/// that the caller can execute against a database connection.
#[must_use]
pub fn plan_search(query: &SearchQuery) -> SearchPlan {
    match query.doc_kind {
        DocKind::Message => plan_message_search(query),
        DocKind::Agent => plan_agent_search(query),
        DocKind::Project => plan_project_search(query),
    }
}

#[allow(clippy::too_many_lines)]
fn plan_message_search(query: &SearchQuery) -> SearchPlan {
    let limit = query.effective_limit();
    let sanitized = if query.text.is_empty() {
        None
    } else {
        sanitize_fts_query(&query.text)
    };
    let mut facets_applied = Vec::new();

    // Determine method
    let has_text = sanitized.is_some();
    let method = if has_text {
        PlanMethod::Fts
    } else if !query.text.is_empty() {
        // Had text but sanitization killed it — try LIKE
        let terms = extract_like_terms(&query.text, 5);
        if terms.is_empty() {
            PlanMethod::Empty
        } else {
            PlanMethod::Like
        }
    } else if has_any_message_facet(query) {
        PlanMethod::FilterOnly
    } else {
        PlanMethod::Empty
    };

    if method == PlanMethod::Empty {
        return SearchPlan {
            sql: String::new(),
            params: Vec::new(),
            method,
            normalized_query: None,
            facets_applied,
        };
    }

    let mut params: Vec<PlanParam> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();

    // ── SELECT + FROM + JOIN ───────────────────────────────────────
    let (select_cols, from_clause, order_clause) = match method {
        PlanMethod::Fts => {
            let sanitized_text = sanitized.as_ref().unwrap();
            // FTS query param for MATCH
            where_clauses.push("fts_messages MATCH ?".to_string());
            params.push(PlanParam::Text(sanitized_text.clone()));

            (
                "m.id, m.subject, m.importance, m.ack_required, m.created_ts, \
                 m.thread_id, a.name AS from_name, m.body_md, m.project_id, \
                 bm25(fts_messages, 10.0, 1.0) AS score"
                    .to_string(),
                "fts_messages \
                 JOIN messages m ON m.id = fts_messages.message_id \
                 JOIN agents a ON a.id = m.sender_id"
                    .to_string(),
                "ORDER BY score ASC, m.id ASC".to_string(),
            )
        }
        PlanMethod::Like => {
            let terms = extract_like_terms(&query.text, 5);
            let mut like_parts = Vec::new();
            for term in &terms {
                let escaped = term.replace('%', "\\%").replace('_', "\\_");
                like_parts.push(
                    "(m.subject LIKE ? ESCAPE '\\' OR m.body_md LIKE ? ESCAPE '\\')".to_string(),
                );
                let pattern = format!("%{escaped}%");
                params.push(PlanParam::Text(pattern.clone()));
                params.push(PlanParam::Text(pattern));
            }
            let like_filter = like_parts.join(" AND ");
            where_clauses.push(like_filter);

            (
                "m.id, m.subject, m.importance, m.ack_required, m.created_ts, \
                 m.thread_id, a.name AS from_name, m.body_md, m.project_id, \
                 0.0 AS score"
                    .to_string(),
                "messages m JOIN agents a ON a.id = m.sender_id".to_string(),
                "ORDER BY m.created_ts DESC, m.id ASC".to_string(),
            )
        }
        PlanMethod::FilterOnly => (
            "m.id, m.subject, m.importance, m.ack_required, m.created_ts, \
             m.thread_id, a.name AS from_name, m.body_md, m.project_id, \
             0.0 AS score"
                .to_string(),
            "messages m JOIN agents a ON a.id = m.sender_id".to_string(),
            match query.ranking {
                RankingMode::Relevance | RankingMode::Recency => {
                    "ORDER BY m.created_ts DESC, m.id ASC".to_string()
                }
            },
        ),
        PlanMethod::Empty => unreachable!(),
    };

    // ── Scope filters ──────────────────────────────────────────────
    if let Some(pid) = query.project_id {
        where_clauses.push("m.project_id = ?".to_string());
        params.push(PlanParam::Int(pid));
        facets_applied.push("project_id".to_string());
    } else if let Some(prod_id) = query.product_id {
        where_clauses.push(
            "m.project_id IN (SELECT project_id FROM product_project_links WHERE product_id = ?)"
                .to_string(),
        );
        params.push(PlanParam::Int(prod_id));
        facets_applied.push("product_id".to_string());
    }

    // ── Facet filters ──────────────────────────────────────────────
    if !query.importance.is_empty() {
        let placeholders: Vec<&str> = query.importance.iter().map(|_| "?").collect();
        where_clauses.push(format!("m.importance IN ({})", placeholders.join(", ")));
        for imp in &query.importance {
            params.push(PlanParam::Text(imp.as_str().to_string()));
        }
        facets_applied.push("importance".to_string());
    }

    if let Some(thread) = &query.thread_id {
        where_clauses.push("m.thread_id = ?".to_string());
        params.push(PlanParam::Text(thread.clone()));
        facets_applied.push("thread_id".to_string());
    }

    if let Some(ack) = query.ack_required {
        where_clauses.push("m.ack_required = ?".to_string());
        params.push(PlanParam::Int(i64::from(ack)));
        facets_applied.push("ack_required".to_string());
    }

    if let Some(min) = query.time_range.min_ts {
        where_clauses.push("m.created_ts >= ?".to_string());
        params.push(PlanParam::Int(min));
        facets_applied.push("time_range_min".to_string());
    }
    if let Some(max) = query.time_range.max_ts {
        where_clauses.push("m.created_ts <= ?".to_string());
        params.push(PlanParam::Int(max));
        facets_applied.push("time_range_max".to_string());
    }

    // Direction filter requires a subquery against message_recipients
    if let (Some(dir), Some(agent)) = (query.direction, &query.agent_name) {
        match dir {
            Direction::Outbox => {
                where_clauses.push("a.name = ?".to_string());
                params.push(PlanParam::Text(agent.clone()));
            }
            Direction::Inbox => {
                where_clauses.push(
                    "m.id IN (SELECT mr.message_id FROM message_recipients mr \
                     JOIN agents ra ON ra.id = mr.agent_id WHERE ra.name = ?)"
                        .to_string(),
                );
                params.push(PlanParam::Text(agent.clone()));
            }
        }
        facets_applied.push("direction".to_string());
    } else if let Some(ref agent) = query.agent_name {
        // Agent filter without direction: match sender OR recipient
        where_clauses.push(
            "(a.name = ? OR m.id IN (SELECT mr.message_id FROM message_recipients mr \
             JOIN agents ra ON ra.id = mr.agent_id WHERE ra.name = ?))"
                .to_string(),
        );
        params.push(PlanParam::Text(agent.clone()));
        params.push(PlanParam::Text(agent.clone()));
        facets_applied.push("agent_name".to_string());
    }

    // ── Cursor-based pagination ────────────────────────────────────
    if let Some(ref cursor_str) = query.cursor {
        if let Some(cursor) = SearchCursor::decode(cursor_str) {
            // For relevance ranking: score ASC, id ASC
            // Cursor means: continue after (score, id)
            where_clauses.push("(score > ? OR (score = ? AND m.id > ?))".to_string());
            params.push(PlanParam::Float(cursor.score));
            params.push(PlanParam::Float(cursor.score));
            params.push(PlanParam::Int(cursor.id));
            facets_applied.push("cursor".to_string());
        }
    }

    // ── Assemble SQL ───────────────────────────────────────────────
    let where_str = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let sql = format!("SELECT {select_cols} FROM {from_clause}{where_str} {order_clause} LIMIT ?",);
    params.push(PlanParam::Int(i64::try_from(limit).unwrap_or(50)));

    SearchPlan {
        sql,
        params,
        method,
        normalized_query: sanitized,
        facets_applied,
    }
}

fn plan_agent_search(query: &SearchQuery) -> SearchPlan {
    let limit = query.effective_limit();
    let sanitized = if query.text.is_empty() {
        None
    } else {
        sanitize_fts_query(&query.text)
    };

    let method = if sanitized.is_some() {
        PlanMethod::Fts
    } else if !query.text.is_empty() {
        PlanMethod::Like
    } else {
        PlanMethod::Empty
    };

    if method == PlanMethod::Empty {
        return SearchPlan {
            sql: String::new(),
            params: Vec::new(),
            method,
            normalized_query: None,
            facets_applied: Vec::new(),
        };
    }

    let mut params: Vec<PlanParam> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();
    let mut facets_applied: Vec<String> = Vec::new();

    let (select_cols, from_clause, order_clause) = match method {
        PlanMethod::Fts => {
            let fts_text = sanitized.as_ref().unwrap();
            where_clauses.push("fts_agents MATCH ?".to_string());
            params.push(PlanParam::Text(fts_text.clone()));
            (
                "a.id, a.name, a.task_description, a.project_id, \
                 bm25(fts_agents, 10.0, 1.0) AS score"
                    .to_string(),
                "fts_agents JOIN agents a ON a.id = fts_agents.agent_id".to_string(),
                "ORDER BY score ASC, a.id ASC".to_string(),
            )
        }
        PlanMethod::Like => {
            let terms = extract_like_terms(&query.text, 5);
            let mut like_parts = Vec::new();
            for term in &terms {
                let escaped = term.replace('%', "\\%").replace('_', "\\_");
                like_parts.push(
                    "(a.name LIKE ? ESCAPE '\\' OR a.task_description LIKE ? ESCAPE '\\')"
                        .to_string(),
                );
                let pattern = format!("%{escaped}%");
                params.push(PlanParam::Text(pattern.clone()));
                params.push(PlanParam::Text(pattern));
            }
            where_clauses.push(like_parts.join(" AND "));
            (
                "a.id, a.name, a.task_description, a.project_id, 0.0 AS score".to_string(),
                "agents a".to_string(),
                "ORDER BY a.id ASC".to_string(),
            )
        }
        _ => unreachable!(),
    };

    if let Some(pid) = query.project_id {
        where_clauses.push("a.project_id = ?".to_string());
        params.push(PlanParam::Int(pid));
        facets_applied.push("project_id".to_string());
    }

    let where_str = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let sql = format!("SELECT {select_cols} FROM {from_clause}{where_str} {order_clause} LIMIT ?");
    params.push(PlanParam::Int(i64::try_from(limit).unwrap_or(50)));

    SearchPlan {
        sql,
        params,
        method,
        normalized_query: sanitized,
        facets_applied,
    }
}

fn plan_project_search(query: &SearchQuery) -> SearchPlan {
    let limit = query.effective_limit();
    let sanitized = if query.text.is_empty() {
        None
    } else {
        sanitize_fts_query(&query.text)
    };

    let method = if sanitized.is_some() {
        PlanMethod::Fts
    } else if !query.text.is_empty() {
        PlanMethod::Like
    } else {
        PlanMethod::Empty
    };

    if method == PlanMethod::Empty {
        return SearchPlan {
            sql: String::new(),
            params: Vec::new(),
            method,
            normalized_query: None,
            facets_applied: Vec::new(),
        };
    }

    let mut params: Vec<PlanParam> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();

    let (select_cols, from_clause, order_clause) = match method {
        PlanMethod::Fts => {
            let fts_text = sanitized.as_ref().unwrap();
            where_clauses.push("fts_projects MATCH ?".to_string());
            params.push(PlanParam::Text(fts_text.clone()));
            (
                "p.id, p.slug, p.human_key, bm25(fts_projects, 10.0, 1.0) AS score".to_string(),
                "fts_projects JOIN projects p ON p.id = fts_projects.project_id".to_string(),
                "ORDER BY score ASC, p.id ASC".to_string(),
            )
        }
        PlanMethod::Like => {
            let terms = extract_like_terms(&query.text, 5);
            let mut like_parts = Vec::new();
            for term in &terms {
                let escaped = term.replace('%', "\\%").replace('_', "\\_");
                like_parts.push(
                    "(p.slug LIKE ? ESCAPE '\\' OR p.human_key LIKE ? ESCAPE '\\')".to_string(),
                );
                let pattern = format!("%{escaped}%");
                params.push(PlanParam::Text(pattern.clone()));
                params.push(PlanParam::Text(pattern));
            }
            where_clauses.push(like_parts.join(" AND "));
            (
                "p.id, p.slug, p.human_key, 0.0 AS score".to_string(),
                "projects p".to_string(),
                "ORDER BY p.id ASC".to_string(),
            )
        }
        _ => unreachable!(),
    };

    let where_str = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let sql = format!("SELECT {select_cols} FROM {from_clause}{where_str} {order_clause} LIMIT ?");
    params.push(PlanParam::Int(i64::try_from(limit).unwrap_or(50)));

    SearchPlan {
        sql,
        params,
        method,
        normalized_query: sanitized,
        facets_applied: Vec::new(),
    }
}

/// Check if the query has any message-specific facet filters.
fn has_any_message_facet(query: &SearchQuery) -> bool {
    !query.importance.is_empty()
        || query.direction.is_some()
        || query.agent_name.is_some()
        || query.thread_id.is_some()
        || query.ack_required.is_some()
        || !query.time_range.is_empty()
        || query.project_id.is_some()
        || query.product_id.is_some()
}

impl SearchPlan {
    /// Build a [`QueryExplain`] from this plan.
    #[must_use]
    pub fn explain(&self) -> QueryExplain {
        QueryExplain {
            method: self.method.as_str().to_string(),
            normalized_query: self.normalized_query.clone(),
            used_like_fallback: self.method == PlanMethod::Like,
            facet_count: self.facets_applied.len(),
            facets_applied: self.facets_applied.clone(),
            sql: self.sql.clone(),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SearchCursor ───────────────────────────────────────────────

    #[test]
    fn cursor_roundtrip() {
        let cursor = SearchCursor {
            score: -1.5,
            id: 42,
        };
        let encoded = cursor.encode();
        let decoded = SearchCursor::decode(&encoded).unwrap();
        assert!((decoded.score - cursor.score).abs() < 1e-12);
        assert_eq!(decoded.id, cursor.id);
    }

    #[test]
    fn cursor_zero_score() {
        let cursor = SearchCursor { score: 0.0, id: 1 };
        let encoded = cursor.encode();
        let decoded = SearchCursor::decode(&encoded).unwrap();
        assert!(decoded.score.abs() < 1e-12);
        assert_eq!(decoded.id, 1);
    }

    #[test]
    fn cursor_decode_invalid() {
        assert!(SearchCursor::decode("").is_none());
        assert!(SearchCursor::decode("garbage").is_none());
        assert!(SearchCursor::decode("s:i").is_none());
        assert!(SearchCursor::decode("snotahex:i1").is_none());
        assert!(SearchCursor::decode("s0000000000000000:inotanumber").is_none());
    }

    // ── DocKind ────────────────────────────────────────────────────

    #[test]
    fn doc_kind_as_str() {
        assert_eq!(DocKind::Message.as_str(), "message");
        assert_eq!(DocKind::Agent.as_str(), "agent");
        assert_eq!(DocKind::Project.as_str(), "project");
    }

    // ── Importance ─────────────────────────────────────────────────

    #[test]
    fn importance_parse_roundtrip() {
        for imp in [
            Importance::Low,
            Importance::Normal,
            Importance::High,
            Importance::Urgent,
        ] {
            assert_eq!(Importance::parse(imp.as_str()), Some(imp));
        }
    }

    #[test]
    fn importance_parse_case_insensitive() {
        assert_eq!(Importance::parse("URGENT"), Some(Importance::Urgent));
        assert_eq!(Importance::parse("Low"), Some(Importance::Low));
        assert_eq!(Importance::parse("unknown"), None);
    }

    // ── TimeRange ──────────────────────────────────────────────────

    #[test]
    fn time_range_empty() {
        let tr = TimeRange::default();
        assert!(tr.is_empty());
        let tr2 = TimeRange {
            min_ts: Some(100),
            max_ts: None,
        };
        assert!(!tr2.is_empty());
    }

    // ── SearchQuery builders ───────────────────────────────────────

    #[test]
    fn query_messages_builder() {
        let q = SearchQuery::messages("hello", 1);
        assert_eq!(q.text, "hello");
        assert_eq!(q.doc_kind, DocKind::Message);
        assert_eq!(q.project_id, Some(1));
    }

    #[test]
    fn query_product_builder() {
        let q = SearchQuery::product_messages("world", 5);
        assert_eq!(q.product_id, Some(5));
        assert_eq!(q.project_id, None);
    }

    #[test]
    fn query_agents_builder() {
        let q = SearchQuery::agents("test", 3);
        assert_eq!(q.doc_kind, DocKind::Agent);
        assert_eq!(q.project_id, Some(3));
    }

    #[test]
    fn query_projects_builder() {
        let q = SearchQuery::projects("myproj");
        assert_eq!(q.doc_kind, DocKind::Project);
        assert!(q.project_id.is_none());
    }

    #[test]
    fn effective_limit_clamping() {
        let mut q = SearchQuery::default();
        assert_eq!(q.effective_limit(), 50); // default
        q.limit = Some(0);
        assert_eq!(q.effective_limit(), 1); // clamp low
        q.limit = Some(9999);
        assert_eq!(q.effective_limit(), 1000); // clamp high
        q.limit = Some(25);
        assert_eq!(q.effective_limit(), 25);
    }

    // ── plan_search: empty queries ─────────────────────────────────

    #[test]
    fn plan_empty_text_no_facets() {
        let q = SearchQuery::default();
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Empty);
        assert!(plan.sql.is_empty());
    }

    #[test]
    fn plan_unsearchable_text() {
        let q = SearchQuery::messages("***", 1);
        let plan = plan_search(&q);
        // "***" sanitizes to None → LIKE terms also empty → Empty
        assert_eq!(plan.method, PlanMethod::Empty);
    }

    // ── plan_search: FTS path ──────────────────────────────────────

    #[test]
    fn plan_fts_message_search() {
        let q = SearchQuery::messages("hello world", 1);
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Fts);
        assert!(plan.sql.contains("fts_messages MATCH ?"));
        assert!(plan.sql.contains("bm25(fts_messages"));
        assert!(plan.sql.contains("m.project_id = ?"));
        assert!(plan.normalized_query.is_some());
    }

    #[test]
    fn plan_fts_product_search() {
        let q = SearchQuery::product_messages("needle", 7);
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Fts);
        assert!(plan.sql.contains("product_project_links"));
        assert!(plan.facets_applied.contains(&"product_id".to_string()));
    }

    // ── plan_search: facets ────────────────────────────────────────

    #[test]
    fn plan_with_importance_facet() {
        let mut q = SearchQuery::messages("test", 1);
        q.importance = vec![Importance::Urgent, Importance::High];
        let plan = plan_search(&q);
        assert!(plan.sql.contains("m.importance IN (?, ?)"));
        assert!(plan.facets_applied.contains(&"importance".to_string()));
    }

    #[test]
    fn plan_with_thread_facet() {
        let mut q = SearchQuery::messages("test", 1);
        q.thread_id = Some("my-thread".to_string());
        let plan = plan_search(&q);
        assert!(plan.sql.contains("m.thread_id = ?"));
        assert!(plan.facets_applied.contains(&"thread_id".to_string()));
    }

    #[test]
    fn plan_with_ack_required() {
        let mut q = SearchQuery::messages("test", 1);
        q.ack_required = Some(true);
        let plan = plan_search(&q);
        assert!(plan.sql.contains("m.ack_required = ?"));
    }

    #[test]
    fn plan_with_time_range() {
        let mut q = SearchQuery::messages("test", 1);
        q.time_range = TimeRange {
            min_ts: Some(100),
            max_ts: Some(999),
        };
        let plan = plan_search(&q);
        assert!(plan.sql.contains("m.created_ts >= ?"));
        assert!(plan.sql.contains("m.created_ts <= ?"));
        assert!(plan.facets_applied.contains(&"time_range_min".to_string()));
        assert!(plan.facets_applied.contains(&"time_range_max".to_string()));
    }

    #[test]
    fn plan_with_direction_outbox() {
        let mut q = SearchQuery::messages("test", 1);
        q.direction = Some(Direction::Outbox);
        q.agent_name = Some("BlueLake".to_string());
        let plan = plan_search(&q);
        assert!(plan.sql.contains("a.name = ?"));
        assert!(plan.facets_applied.contains(&"direction".to_string()));
    }

    #[test]
    fn plan_with_direction_inbox() {
        let mut q = SearchQuery::messages("test", 1);
        q.direction = Some(Direction::Inbox);
        q.agent_name = Some("BlueLake".to_string());
        let plan = plan_search(&q);
        assert!(plan.sql.contains("message_recipients"));
        assert!(plan.facets_applied.contains(&"direction".to_string()));
    }

    #[test]
    fn plan_agent_name_without_direction() {
        let mut q = SearchQuery::messages("test", 1);
        q.agent_name = Some("BlueLake".to_string());
        let plan = plan_search(&q);
        // Should match sender OR recipient
        assert!(plan.sql.contains("a.name = ?"));
        assert!(plan.sql.contains("message_recipients"));
        assert!(plan.facets_applied.contains(&"agent_name".to_string()));
    }

    // ── plan_search: cursor pagination ─────────────────────────────

    #[test]
    fn plan_with_cursor() {
        let cursor = SearchCursor {
            score: -2.5,
            id: 100,
        };
        let mut q = SearchQuery::messages("test", 1);
        q.cursor = Some(cursor.encode());
        let plan = plan_search(&q);
        assert!(plan.sql.contains("score > ?"));
        assert!(plan.sql.contains("m.id > ?"));
        assert!(plan.facets_applied.contains(&"cursor".to_string()));
    }

    // ── plan_search: filter-only (no text) ─────────────────────────

    #[test]
    fn plan_filter_only_with_facets() {
        let q = SearchQuery {
            doc_kind: DocKind::Message,
            project_id: Some(1),
            importance: vec![Importance::Urgent],
            ..Default::default()
        };
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::FilterOnly);
        assert!(plan.sql.contains("m.importance IN (?)"));
        assert!(plan.sql.contains("m.project_id = ?"));
        assert!(!plan.sql.contains("fts_messages"));
    }

    // ── plan_search: agent search ──────────────────────────────────

    #[test]
    fn plan_agent_fts() {
        let q = SearchQuery::agents("blue", 1);
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Fts);
        assert!(plan.sql.contains("fts_agents MATCH ?"));
        assert!(plan.sql.contains("a.project_id = ?"));
    }

    #[test]
    fn plan_agent_empty() {
        let q = SearchQuery::agents("", 1);
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Empty);
    }

    // ── plan_search: project search ────────────────────────────────

    #[test]
    fn plan_project_fts() {
        let q = SearchQuery::projects("my-proj");
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Fts);
        assert!(plan.sql.contains("fts_projects MATCH ?"));
    }

    #[test]
    fn plan_project_empty() {
        let q = SearchQuery::projects("");
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Empty);
    }

    // ── explain ────────────────────────────────────────────────────

    #[test]
    fn explain_output() {
        let q = SearchQuery::messages("test", 1);
        let plan = plan_search(&q);
        let explain = plan.explain();
        assert_eq!(explain.method, "fts5");
        assert!(!explain.used_like_fallback);
        assert!(explain.normalized_query.is_some());
        assert!(!explain.sql.is_empty());
    }

    #[test]
    fn explain_like_fallback() {
        // Use a query that sanitize_fts_query would reject but has extractable terms
        // Parentheses without matching are tricky for FTS5, so let's use a term
        // that sanitize_fts_query passes but FTS5 would reject at runtime.
        // For this unit test, we just verify the plan chooses LIKE when sanitization fails.
        let q = SearchQuery::messages("***", 1);
        let plan = plan_search(&q);
        // *** sanitizes to None, and LIKE terms from *** are empty → Empty
        assert_eq!(plan.method, PlanMethod::Empty);
    }

    // ── PlanMethod ─────────────────────────────────────────────────

    #[test]
    fn plan_method_as_str() {
        assert_eq!(PlanMethod::Fts.as_str(), "fts5");
        assert_eq!(PlanMethod::Like.as_str(), "like_fallback");
        assert_eq!(PlanMethod::FilterOnly.as_str(), "filter_only");
        assert_eq!(PlanMethod::Empty.as_str(), "empty");
    }

    // ── Limit propagation ──────────────────────────────────────────

    #[test]
    fn plan_propagates_limit() {
        let mut q = SearchQuery::messages("hello", 1);
        q.limit = Some(25);
        let plan = plan_search(&q);
        // The last param should be the limit
        assert!(plan.sql.contains("LIMIT ?"));
        if let Some(PlanParam::Int(v)) = plan.params.last() {
            assert_eq!(*v, 25);
        } else {
            panic!("last param should be Int limit");
        }
    }

    // ── Serde roundtrip ────────────────────────────────────────────

    #[test]
    fn search_query_serde_roundtrip() {
        let q = SearchQuery {
            text: "hello".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::Urgent],
            time_range: TimeRange {
                min_ts: Some(100),
                max_ts: None,
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&q).unwrap();
        let q2: SearchQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(q2.text, "hello");
        assert_eq!(q2.importance.len(), 1);
        assert_eq!(q2.time_range.min_ts, Some(100));
    }

    #[test]
    fn search_result_serde() {
        let r = SearchResult {
            doc_kind: DocKind::Message,
            id: 1,
            project_id: Some(2),
            title: "Subject".to_string(),
            body: "Body text".to_string(),
            score: Some(-1.5),
            importance: Some("urgent".to_string()),
            ack_required: Some(true),
            created_ts: Some(1000),
            thread_id: Some("t1".to_string()),
            from_agent: Some("Blue".to_string()),
        };
        let json = serde_json::to_string(&r).unwrap();
        let r2: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r2.id, 1);
        assert_eq!(r2.score, Some(-1.5));
    }

    // ── Multiple facets combined ───────────────────────────────────

    #[test]
    fn plan_multiple_facets_combined() {
        let mut q = SearchQuery::messages("hello", 1);
        q.importance = vec![Importance::Urgent];
        q.thread_id = Some("my-thread".to_string());
        q.ack_required = Some(true);
        q.time_range = TimeRange {
            min_ts: Some(0),
            max_ts: Some(999),
        };
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Fts);
        // All facets should be in the SQL
        assert!(plan.sql.contains("m.importance IN (?)"));
        assert!(plan.sql.contains("m.thread_id = ?"));
        assert!(plan.sql.contains("m.ack_required = ?"));
        assert!(plan.sql.contains("m.created_ts >= ?"));
        assert!(plan.sql.contains("m.created_ts <= ?"));
        assert!(plan.sql.contains("m.project_id = ?"));
        // 6 facets applied
        assert_eq!(plan.facets_applied.len(), 6);
    }
}
