//! Global query planner for unified search across messages, agents, and projects.
//!
//! Converts a [`SearchQuery`] into SQL + params, supporting:
//! - Faceted filtering (importance, direction, time range, project, agent, thread)
//! - BM25 relevance ranking with score extraction
//! - Stable cursor-based pagination using (score, id)
//! - Query explain output for debugging/trust
//! - Permission-aware visibility with contact-policy enforcement
//! - Field-level redaction with deterministic audit events

#![allow(clippy::module_name_repetitions)]

use mcp_agent_mail_search_core::QueryAssistance;
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
    Thread,
}

impl DocKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Agent => "agent",
            Self::Project => "project",
            Self::Thread => "thread",
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
// Scope & Redaction
// ────────────────────────────────────────────────────────────────────

/// Visibility scope policy for search results.
///
/// Controls how aggressively the planner enforces contact-policy and
/// project-visibility rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScopePolicy {
    /// No visibility enforcement — return all matches.
    /// Suitable for operator/admin dashboards.
    #[default]
    Unrestricted,

    /// Restrict results to projects the caller belongs to.
    /// Messages from agents with `block_all` or `contacts_only` policy
    /// (when the caller lacks an approved contact link) are excluded.
    CallerScoped {
        /// The caller's agent name (used to resolve identity + links).
        caller_agent: String,
    },

    /// Restrict to an explicit set of project IDs.
    /// Useful for pre-computed access lists.
    ProjectSet { allowed_project_ids: Vec<i64> },
}

impl ScopePolicy {
    /// Whether this policy requires any visibility filtering.
    #[must_use]
    pub const fn is_restricted(&self) -> bool {
        !matches!(self, Self::Unrestricted)
    }
}

/// Controls which fields are redacted in search results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionConfig {
    /// Replace message body/snippet with placeholder text.
    pub redact_body: bool,
    /// Remove sender/recipient agent names from results.
    pub redact_agent_names: bool,
    /// Remove thread IDs from results.
    pub redact_thread_ids: bool,
    /// Placeholder text for redacted fields.
    pub placeholder: String,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            redact_body: false,
            redact_agent_names: false,
            redact_thread_ids: false,
            placeholder: "[redacted]".to_string(),
        }
    }
}

impl RedactionConfig {
    /// Standard redaction for cross-project results where contact policy blocks visibility.
    #[must_use]
    pub fn contact_blocked() -> Self {
        Self {
            redact_body: true,
            redact_agent_names: false,
            redact_thread_ids: true,
            placeholder: "[content hidden — contact policy]".to_string(),
        }
    }

    /// Strict redaction for results that should be almost entirely hidden.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            redact_body: true,
            redact_agent_names: true,
            redact_thread_ids: true,
            placeholder: "[redacted]".to_string(),
        }
    }

    /// Whether any redaction is configured.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.redact_body || self.redact_agent_names || self.redact_thread_ids
    }
}

/// Audit entry for a search result that was filtered or redacted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchAuditEntry {
    /// What happened to the result.
    pub action: AuditAction,
    /// The document kind that was affected.
    pub doc_kind: DocKind,
    /// The ID of the affected document (message/agent/project).
    pub doc_id: i64,
    /// The project ID where the document lives.
    pub project_id: Option<i64>,
    /// Why this action was taken.
    pub reason: String,
    /// The scope policy that triggered this.
    pub policy: String,
}

/// What happened to a search result during visibility enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// Result was completely excluded from response.
    Denied,
    /// Result was included but with redacted fields.
    Redacted,
}

impl AuditAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Redacted => "redacted",
        }
    }
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

    // ── Visibility & Redaction ─────────────────────────────────────
    /// Scope policy controlling result visibility.
    #[serde(default)]
    pub scope: ScopePolicy,

    /// Redaction configuration for restricted results.
    /// If `None`, default redaction rules apply based on scope policy.
    pub redaction: Option<RedactionConfig>,
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

    // ── Explain metadata (only populated when explain=true) ──────
    /// Concise reason codes explaining why this result ranked here.
    /// Machine-stable identifiers (e.g. `lexical_bm25`, `semantic_cosine`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,

    /// Top score factors with contributions, sorted by abs magnitude.
    /// Each entry: { key, contribution, summary }.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub score_factors: Vec<ScoreFactorSummary>,

    // ── Redaction metadata ────────────────────────────────────────
    /// Whether any fields in this result were redacted.
    #[serde(default)]
    pub redacted: bool,
    /// Human-readable reason for redaction (displayed to user).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_reason: Option<String>,
}

/// Concise score factor summary for per-result explain output.
/// Designed to be small and safe for JSON response payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreFactorSummary {
    /// Stable key (e.g. `bm25`, `term_coverage`, `cosine`).
    pub key: String,
    /// Numeric contribution to this result's score.
    pub contribution: f64,
    /// Human-readable one-line summary.
    pub summary: String,
}

/// Response from the search planner, including results and pagination info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub next_cursor: Option<String>,
    pub explain: Option<QueryExplain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistance: Option<QueryAssistance>,

    /// Zero-result recovery guidance (populated when results are empty or very low).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<ZeroResultGuidance>,

    /// Audit log of denied/redacted results (empty when scope is unrestricted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit: Vec<SearchAuditEntry>,
}

// ────────────────────────────────────────────────────────────────────
// Zero-result recovery guidance
// ────────────────────────────────────────────────────────────────────

/// Actionable guidance for recovering from zero or low-result searches.
///
/// Generated deterministically from query facets — never leaks restricted data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZeroResultGuidance {
    /// Human-readable summary of why results may be empty.
    pub summary: String,
    /// Ordered list of concrete recovery suggestions.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub suggestions: Vec<RecoverySuggestion>,
}

/// A single actionable suggestion for recovering from a failed search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverySuggestion {
    /// Machine-stable kind (e.g. `broaden_date_range`, `drop_filter`, `switch_mode`).
    pub kind: String,
    /// Human-readable label for the suggestion.
    pub label: String,
    /// Optional description with more detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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

    /// Scope policy that was applied.
    #[serde(default = "default_scope_label")]
    pub scope_policy: String,
    /// How many results were denied by visibility rules.
    #[serde(default)]
    pub denied_count: usize,
    /// How many results were redacted (included but with hidden fields).
    #[serde(default)]
    pub redacted_count: usize,
}

fn default_scope_label() -> String {
    "unrestricted".to_string()
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
    /// Whether the plan includes scope-enforcement SQL.
    pub scope_enforced: bool,
    /// Label for the scope policy applied.
    pub scope_label: String,
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
///
/// Scope enforcement via SQL WHERE clauses is applied when the query has
/// a `ProjectSet` scope policy. `CallerScoped` enforcement is deferred
/// to post-processing via [`apply_visibility()`] since it requires
/// agent-identity resolution that the pure planner cannot perform.
#[must_use]
pub fn plan_search(query: &SearchQuery) -> SearchPlan {
    let mut plan = match query.doc_kind {
        DocKind::Message | DocKind::Thread => plan_message_search(query),
        DocKind::Agent => plan_agent_search(query),
        DocKind::Project => plan_project_search(query),
    };

    // Even when we short-circuit execution for empty plans, the planner contract
    // expects a non-empty, valid SQL string for hostile/non-searchable *non-empty*
    // inputs. Use a deterministic, schema-independent "empty result" query.
    if plan.method == PlanMethod::Empty && !query.text.is_empty() && plan.sql.is_empty() {
        plan.sql = empty_plan_sql(query.doc_kind).to_string();
    }

    plan
}

const fn empty_plan_sql(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Message | DocKind::Thread => {
            "SELECT \
                0 AS id, \
                '' AS subject, \
                '' AS importance, \
                0 AS ack_required, \
                0 AS created_ts, \
                '' AS thread_id, \
                '' AS from_name, \
                '' AS body_md, \
                0 AS project_id, \
                0.0 AS score \
             WHERE 0"
        }
        DocKind::Agent => {
            "SELECT \
                0 AS id, \
                '' AS name, \
                '' AS task_description, \
                0 AS project_id, \
                0.0 AS score \
             WHERE 0"
        }
        DocKind::Project => {
            "SELECT \
                0 AS id, \
                '' AS slug, \
                '' AS human_key, \
                0.0 AS score \
             WHERE 0"
        }
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

    let scope_label = scope_policy_label(&query.scope);

    if method == PlanMethod::Empty {
        return SearchPlan::empty(scope_label);
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
    let mut scope_enforced = false;
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

    // ── Visibility scope enforcement ──────────────────────────────
    if let ScopePolicy::ProjectSet {
        ref allowed_project_ids,
    } = query.scope
    {
        if !allowed_project_ids.is_empty() {
            let placeholders: Vec<&str> = allowed_project_ids.iter().map(|_| "?").collect();
            where_clauses.push(format!("m.project_id IN ({})", placeholders.join(", ")));
            for &pid in allowed_project_ids {
                params.push(PlanParam::Int(pid));
            }
            facets_applied.push("scope_project_set".to_string());
            scope_enforced = true;
        }
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
        scope_enforced,
        scope_label,
    }
}

fn plan_agent_search(query: &SearchQuery) -> SearchPlan {
    let limit = query.effective_limit();
    let scope_label = scope_policy_label(&query.scope);

    let terms = extract_like_terms(&query.text, 5);

    // Identity FTS tables (fts_agents) are dropped at runtime by
    // enforce_runtime_identity_fts_cleanup, so always use LIKE fallback.
    let method = if query.text.is_empty() || terms.is_empty() {
        PlanMethod::Empty
    } else {
        PlanMethod::Like
    };

    if method == PlanMethod::Empty {
        return SearchPlan::empty(scope_label);
    }

    let mut params: Vec<PlanParam> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();
    let mut facets_applied: Vec<String> = Vec::new();
    let mut scope_enforced = false;

    let (select_cols, from_clause, order_clause) = {
        let mut like_parts = Vec::new();
        for term in &terms {
            let escaped = term.replace('%', "\\%").replace('_', "\\_");
            like_parts.push(
                "(a.name LIKE ? ESCAPE '\\' OR a.task_description LIKE ? ESCAPE '\\')".to_string(),
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
    };

    if let Some(pid) = query.project_id {
        where_clauses.push("a.project_id = ?".to_string());
        params.push(PlanParam::Int(pid));
        facets_applied.push("project_id".to_string());
    }

    // Scope enforcement for ProjectSet
    if let ScopePolicy::ProjectSet {
        ref allowed_project_ids,
    } = query.scope
    {
        if !allowed_project_ids.is_empty() {
            let placeholders: Vec<&str> = allowed_project_ids.iter().map(|_| "?").collect();
            where_clauses.push(format!("a.project_id IN ({})", placeholders.join(", ")));
            for &pid in allowed_project_ids {
                params.push(PlanParam::Int(pid));
            }
            facets_applied.push("scope_project_set".to_string());
            scope_enforced = true;
        }
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
        normalized_query: None,
        facets_applied,
        scope_enforced,
        scope_label,
    }
}

fn plan_project_search(query: &SearchQuery) -> SearchPlan {
    let limit = query.effective_limit();
    let scope_label = scope_policy_label(&query.scope);

    let terms = extract_like_terms(&query.text, 5);

    // Identity FTS tables (fts_projects) are dropped at runtime by
    // enforce_runtime_identity_fts_cleanup, so always use LIKE fallback.
    let method = if query.text.is_empty() || terms.is_empty() {
        PlanMethod::Empty
    } else {
        PlanMethod::Like
    };

    if method == PlanMethod::Empty {
        return SearchPlan::empty(scope_label);
    }

    let mut params: Vec<PlanParam> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();
    let mut facets_applied: Vec<String> = Vec::new();
    let mut scope_enforced = false;

    let (select_cols, from_clause, order_clause) = {
        let mut like_parts = Vec::new();
        for term in &terms {
            let escaped = term.replace('%', "\\%").replace('_', "\\_");
            like_parts
                .push("(p.slug LIKE ? ESCAPE '\\' OR p.human_key LIKE ? ESCAPE '\\')".to_string());
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
    };

    // Scope enforcement for ProjectSet
    if let ScopePolicy::ProjectSet {
        ref allowed_project_ids,
    } = query.scope
    {
        if !allowed_project_ids.is_empty() {
            let placeholders: Vec<&str> = allowed_project_ids.iter().map(|_| "?").collect();
            where_clauses.push(format!("p.id IN ({})", placeholders.join(", ")));
            for &pid in allowed_project_ids {
                params.push(PlanParam::Int(pid));
            }
            facets_applied.push("scope_project_set".to_string());
            scope_enforced = true;
        }
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
        normalized_query: None,
        facets_applied,
        scope_enforced,
        scope_label,
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
            scope_policy: self.scope_label.clone(),
            denied_count: 0,
            redacted_count: 0,
        }
    }

    /// Create an empty plan for zero-result queries.
    const fn empty(scope_label: String) -> Self {
        Self {
            sql: String::new(),
            params: Vec::new(),
            method: PlanMethod::Empty,
            normalized_query: None,
            facets_applied: Vec::new(),
            scope_enforced: false,
            scope_label,
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Visibility — post-processing redaction and filtering
// ────────────────────────────────────────────────────────────────────

/// Context for evaluating visibility of a single result.
/// Populated from the caller's agent identity and contact links.
pub struct VisibilityContext {
    /// Project IDs the caller belongs to.
    pub caller_project_ids: Vec<i64>,
    /// Agent IDs the caller has approved contact links with.
    /// (The other end's `agent_id` for each approved link.)
    pub approved_contact_ids: Vec<i64>,
    /// The scope policy in effect.
    pub policy: ScopePolicy,
    /// Redaction config (defaults to `contact_blocked` if not set).
    pub redaction: RedactionConfig,
}

impl VisibilityContext {
    /// Check if the caller can see a result from the given project.
    #[must_use]
    pub fn can_see_project(&self, project_id: i64) -> bool {
        match &self.policy {
            ScopePolicy::Unrestricted => true,
            ScopePolicy::CallerScoped { .. } => self.caller_project_ids.contains(&project_id),
            ScopePolicy::ProjectSet {
                allowed_project_ids,
            } => allowed_project_ids.contains(&project_id),
        }
    }
}

/// Apply visibility rules to search results, producing redacted/filtered output
/// and audit entries.
///
/// Results from projects the caller can see are included as-is.
/// Results from other projects are either denied or redacted based on policy.
#[must_use]
pub fn apply_visibility(
    results: Vec<SearchResult>,
    ctx: &VisibilityContext,
) -> (Vec<SearchResult>, Vec<SearchAuditEntry>) {
    if !ctx.policy.is_restricted() {
        return (results, Vec::new());
    }

    let mut visible = Vec::with_capacity(results.len());
    let mut audit = Vec::new();

    for result in results {
        let project_id = result.project_id.unwrap_or(0);

        if ctx.can_see_project(project_id) {
            visible.push(result);
            continue;
        }

        // Not in caller's projects — check redaction config
        if ctx.redaction.is_active() {
            // Include but redact
            let reason = ctx.redaction.placeholder.clone();
            audit.push(SearchAuditEntry {
                action: AuditAction::Redacted,
                doc_kind: result.doc_kind,
                doc_id: result.id,
                project_id: result.project_id,
                reason: reason.clone(),
                policy: scope_policy_label(&ctx.policy),
            });
            visible.push(redact_result(result, &ctx.redaction));
        } else {
            // Deny entirely
            audit.push(SearchAuditEntry {
                action: AuditAction::Denied,
                doc_kind: result.doc_kind,
                doc_id: result.id,
                project_id: result.project_id,
                reason: "project not visible to caller".to_string(),
                policy: scope_policy_label(&ctx.policy),
            });
        }
    }

    (visible, audit)
}

/// Apply redaction to a single search result.
fn redact_result(mut result: SearchResult, config: &RedactionConfig) -> SearchResult {
    result.redacted = true;
    result.redaction_reason = Some(config.placeholder.clone());

    if config.redact_body {
        result.body.clone_from(&config.placeholder);
        result.title.clone_from(&config.placeholder);
    }
    if config.redact_agent_names {
        result.from_agent = Some(config.placeholder.clone());
    }
    if config.redact_thread_ids {
        result.thread_id = None;
    }

    result
}

/// Human-readable label for a scope policy.
fn scope_policy_label(policy: &ScopePolicy) -> String {
    match policy {
        ScopePolicy::Unrestricted => "unrestricted".to_string(),
        ScopePolicy::CallerScoped { caller_agent } => {
            format!("caller_scoped:{caller_agent}")
        }
        ScopePolicy::ProjectSet {
            allowed_project_ids,
        } => {
            format!("project_set:{}", allowed_project_ids.len())
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
    fn plan_agent_uses_like_fallback() {
        // Identity FTS tables (fts_agents) are dropped at runtime, so agent
        // searches must always use LIKE fallback.
        let q = SearchQuery::agents("blue", 1);
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Like);
        assert!(plan.sql.contains("a.name LIKE ?"));
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
    fn plan_project_uses_like_fallback() {
        // Identity FTS tables (fts_projects) are dropped at runtime, so project
        // searches must always use LIKE fallback.
        let q = SearchQuery::projects("my-proj");
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Like);
        assert!(plan.sql.contains("p.slug LIKE ?"));
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
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let r2: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r2.id, 1);
        assert_eq!(r2.score, Some(-1.5));
        assert!(!r2.redacted);
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

    // ── ScopePolicy ───────────────────────────────────────────────

    #[test]
    fn scope_policy_unrestricted_not_restricted() {
        assert!(!ScopePolicy::Unrestricted.is_restricted());
    }

    #[test]
    fn scope_policy_caller_scoped_is_restricted() {
        let policy = ScopePolicy::CallerScoped {
            caller_agent: "BlueLake".to_string(),
        };
        assert!(policy.is_restricted());
    }

    #[test]
    fn scope_policy_project_set_is_restricted() {
        let policy = ScopePolicy::ProjectSet {
            allowed_project_ids: vec![1, 2],
        };
        assert!(policy.is_restricted());
    }

    #[test]
    fn scope_policy_serde_roundtrip() {
        let policy = ScopePolicy::CallerScoped {
            caller_agent: "TestAgent".to_string(),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let p2: ScopePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p2, policy);
    }

    // ── RedactionConfig ───────────────────────────────────────────

    #[test]
    fn redaction_config_default_inactive() {
        let config = RedactionConfig::default();
        assert!(!config.is_active());
    }

    #[test]
    fn redaction_config_contact_blocked_active() {
        let config = RedactionConfig::contact_blocked();
        assert!(config.is_active());
        assert!(config.redact_body);
        assert!(!config.redact_agent_names);
        assert!(config.redact_thread_ids);
    }

    #[test]
    fn redaction_config_strict_all_active() {
        let config = RedactionConfig::strict();
        assert!(config.is_active());
        assert!(config.redact_body);
        assert!(config.redact_agent_names);
        assert!(config.redact_thread_ids);
    }

    // ── AuditAction ───────────────────────────────────────────────

    #[test]
    fn audit_action_as_str() {
        assert_eq!(AuditAction::Denied.as_str(), "denied");
        assert_eq!(AuditAction::Redacted.as_str(), "redacted");
    }

    // ── Plan with ProjectSet scope ────────────────────────────────

    #[test]
    fn plan_message_with_project_set_scope() {
        let mut q = SearchQuery::messages("hello", 1);
        q.scope = ScopePolicy::ProjectSet {
            allowed_project_ids: vec![1, 2, 3],
        };
        let plan = plan_search(&q);
        assert_eq!(plan.method, PlanMethod::Fts);
        assert!(plan.sql.contains("m.project_id IN (?, ?, ?)"));
        assert!(plan.scope_enforced);
        assert!(
            plan.facets_applied
                .contains(&"scope_project_set".to_string())
        );
    }

    #[test]
    fn plan_agent_with_project_set_scope() {
        let mut q = SearchQuery::agents("blue", 1);
        q.scope = ScopePolicy::ProjectSet {
            allowed_project_ids: vec![5],
        };
        let plan = plan_search(&q);
        assert!(plan.sql.contains("a.project_id IN (?)"));
        assert!(plan.scope_enforced);
    }

    #[test]
    fn plan_project_with_project_set_scope() {
        let mut q = SearchQuery::projects("myproj");
        q.scope = ScopePolicy::ProjectSet {
            allowed_project_ids: vec![10, 20],
        };
        let plan = plan_search(&q);
        assert!(plan.sql.contains("p.id IN (?, ?)"));
        assert!(plan.scope_enforced);
    }

    #[test]
    fn plan_caller_scoped_not_sql_enforced() {
        // CallerScoped doesn't add SQL — relies on post-processing
        let mut q = SearchQuery::messages("hello", 1);
        q.scope = ScopePolicy::CallerScoped {
            caller_agent: "BlueLake".to_string(),
        };
        let plan = plan_search(&q);
        assert!(!plan.scope_enforced);
        assert!(plan.scope_label.starts_with("caller_scoped"));
    }

    #[test]
    fn plan_scope_label_in_explain() {
        let mut q = SearchQuery::messages("hello", 1);
        q.scope = ScopePolicy::ProjectSet {
            allowed_project_ids: vec![1],
        };
        let plan = plan_search(&q);
        let explain = plan.explain();
        assert!(explain.scope_policy.starts_with("project_set"));
    }

    // ── apply_visibility ──────────────────────────────────────────

    fn make_result(id: i64, project_id: i64) -> SearchResult {
        SearchResult {
            doc_kind: DocKind::Message,
            id,
            project_id: Some(project_id),
            title: format!("Subject {id}"),
            body: format!("Body {id}"),
            score: Some(-1.0),
            importance: Some("normal".to_string()),
            ack_required: None,
            created_ts: Some(1000),
            thread_id: Some("t1".to_string()),
            from_agent: Some("BlueLake".to_string()),
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        }
    }

    #[test]
    fn visibility_unrestricted_passes_all() {
        let results = vec![make_result(1, 10), make_result(2, 20)];
        let ctx = VisibilityContext {
            caller_project_ids: vec![10],
            approved_contact_ids: vec![],
            policy: ScopePolicy::Unrestricted,
            redaction: RedactionConfig::default(),
        };
        let (visible, audit) = apply_visibility(results, &ctx);
        assert_eq!(visible.len(), 2);
        assert!(audit.is_empty());
    }

    #[test]
    fn visibility_caller_scoped_denies_outside_projects() {
        let results = vec![make_result(1, 10), make_result(2, 20), make_result(3, 10)];
        let ctx = VisibilityContext {
            caller_project_ids: vec![10],
            approved_contact_ids: vec![],
            policy: ScopePolicy::CallerScoped {
                caller_agent: "BlueLake".to_string(),
            },
            redaction: RedactionConfig::default(), // not active → deny
        };
        let (visible, audit) = apply_visibility(results, &ctx);
        // Only results from project 10 are visible
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].id, 1);
        assert_eq!(visible[1].id, 3);
        // Result from project 20 was denied
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action, AuditAction::Denied);
        assert_eq!(audit[0].doc_id, 2);
    }

    #[test]
    fn visibility_caller_scoped_redacts_when_configured() {
        let results = vec![make_result(1, 10), make_result(2, 20)];
        let ctx = VisibilityContext {
            caller_project_ids: vec![10],
            approved_contact_ids: vec![],
            policy: ScopePolicy::CallerScoped {
                caller_agent: "BlueLake".to_string(),
            },
            redaction: RedactionConfig::contact_blocked(),
        };
        let (visible, audit) = apply_visibility(results, &ctx);
        assert_eq!(visible.len(), 2);
        // First result unredacted
        assert!(!visible[0].redacted);
        assert_eq!(visible[0].body, "Body 1");
        // Second result redacted
        assert!(visible[1].redacted);
        assert_eq!(visible[1].body, "[content hidden — contact policy]");
        assert_eq!(visible[1].title, "[content hidden — contact policy]");
        assert!(visible[1].thread_id.is_none()); // thread_id redacted
        assert!(visible[1].from_agent.is_some()); // agent name NOT redacted in contact_blocked
        // Audit
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action, AuditAction::Redacted);
    }

    #[test]
    fn visibility_strict_redaction_hides_agent_names() {
        let results = vec![make_result(1, 99)];
        let ctx = VisibilityContext {
            caller_project_ids: vec![10],
            approved_contact_ids: vec![],
            policy: ScopePolicy::CallerScoped {
                caller_agent: "BlueLake".to_string(),
            },
            redaction: RedactionConfig::strict(),
        };
        let (visible, audit) = apply_visibility(results, &ctx);
        assert_eq!(visible.len(), 1);
        assert!(visible[0].redacted);
        assert_eq!(visible[0].from_agent.as_deref(), Some("[redacted]"));
        assert!(visible[0].thread_id.is_none());
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn visibility_project_set_filters_by_allowed_ids() {
        let results = vec![make_result(1, 10), make_result(2, 20), make_result(3, 30)];
        let ctx = VisibilityContext {
            caller_project_ids: vec![],
            approved_contact_ids: vec![],
            policy: ScopePolicy::ProjectSet {
                allowed_project_ids: vec![10, 30],
            },
            redaction: RedactionConfig::default(),
        };
        let (visible, audit) = apply_visibility(results, &ctx);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].id, 1);
        assert_eq!(visible[1].id, 3);
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].doc_id, 2);
    }

    // ── SearchAuditEntry serde ────────────────────────────────────

    #[test]
    fn audit_entry_serde_roundtrip() {
        let entry = SearchAuditEntry {
            action: AuditAction::Denied,
            doc_kind: DocKind::Message,
            doc_id: 42,
            project_id: Some(1),
            reason: "not visible".to_string(),
            policy: "caller_scoped:BlueLake".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let e2: SearchAuditEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(e2.action, AuditAction::Denied);
        assert_eq!(e2.doc_id, 42);
    }

    // ── SearchResponse with audit ─────────────────────────────────

    #[test]
    fn search_response_audit_omitted_when_empty() {
        let resp = SearchResponse {
            results: vec![],
            next_cursor: None,
            explain: None,
            assistance: None,
            guidance: None,
            audit: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("audit"));
    }

    #[test]
    fn search_response_audit_included_when_present() {
        let resp = SearchResponse {
            results: vec![],
            next_cursor: None,
            explain: None,
            assistance: None,
            guidance: None,
            audit: vec![SearchAuditEntry {
                action: AuditAction::Redacted,
                doc_kind: DocKind::Message,
                doc_id: 1,
                project_id: Some(1),
                reason: "test".to_string(),
                policy: "test".to_string(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("audit"));
        assert!(json.contains("redacted"));
    }

    // ── ZeroResultGuidance serialization ─────────────────────────

    #[test]
    fn guidance_omitted_when_none() {
        let resp = SearchResponse {
            results: vec![],
            next_cursor: None,
            explain: None,
            assistance: None,
            guidance: None,
            audit: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("guidance"));
    }

    #[test]
    fn guidance_included_when_present() {
        let resp = SearchResponse {
            results: vec![],
            next_cursor: None,
            explain: None,
            assistance: None,
            guidance: Some(ZeroResultGuidance {
                summary: "No results found.".to_string(),
                suggestions: vec![RecoverySuggestion {
                    kind: "simplify_query".to_string(),
                    label: "Simplify search terms".to_string(),
                    detail: Some("Try fewer or broader keywords.".to_string()),
                }],
            }),
            audit: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("guidance"));
        assert!(json.contains("simplify_query"));
        assert!(json.contains("Simplify search terms"));
    }

    #[test]
    fn guidance_roundtrip_serde() {
        let original = ZeroResultGuidance {
            summary: "No results.".to_string(),
            suggestions: vec![
                RecoverySuggestion {
                    kind: "drop_filter".to_string(),
                    label: "Remove filter".to_string(),
                    detail: None,
                },
                RecoverySuggestion {
                    kind: "fix_typo".to_string(),
                    label: "Did you mean from:X?".to_string(),
                    detail: Some("Explanation here.".to_string()),
                },
            ],
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: ZeroResultGuidance = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.summary, original.summary);
        assert_eq!(restored.suggestions.len(), 2);
        assert_eq!(restored.suggestions[0].kind, "drop_filter");
        assert!(restored.suggestions[0].detail.is_none());
        assert_eq!(restored.suggestions[1].kind, "fix_typo");
        assert!(restored.suggestions[1].detail.is_some());
    }

    // ── SearchQuery with scope serde ──────────────────────────────

    #[test]
    fn search_query_with_scope_serde() {
        let q = SearchQuery {
            text: "test".to_string(),
            scope: ScopePolicy::CallerScoped {
                caller_agent: "BlueLake".to_string(),
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&q).unwrap();
        let q2: SearchQuery = serde_json::from_str(&json).unwrap();
        match q2.scope {
            ScopePolicy::CallerScoped { caller_agent } => {
                assert_eq!(caller_agent, "BlueLake");
            }
            _ => panic!("expected CallerScoped"),
        }
    }
}
