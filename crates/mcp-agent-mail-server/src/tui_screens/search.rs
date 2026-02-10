//! Search Cockpit screen with query bar, facet rail, and results.
//!
//! Provides a unified search interface across messages, agents, and projects
//! using the global search planner and search service.  Facet toggles allow
//! composable filtering by document kind, importance, ack status, and more.

use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers, PackedRgba, Style};
use ftui_runtime::program::Cmd;
use ftui_widgets::input::TextInput;

use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::search_planner::{
    DocKind, Importance, RankingMode, SearchQuery, plan_search,
};
use mcp_agent_mail_db::search_recipes::{
    QueryHistoryEntry, ScopeMode, SearchRecipe, insert_history, insert_recipe, list_recent_history,
    list_recipes, touch_recipe,
};
use mcp_agent_mail_db::sqlmodel::Value;
use mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection;
use mcp_agent_mail_db::timestamps::{micros_to_iso, now_micros};

use crate::tui_bridge::TuiSharedState;
use crate::tui_markdown;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max results to display.
const MAX_RESULTS: usize = 200;

/// Debounce delay in ticks (~100ms each, so 3 ticks = ~300ms).
const DEBOUNCE_TICKS: u8 = 3;

/// Max chars for the message snippet shown in the detail pane.
const MAX_SNIPPET_CHARS: usize = 180;

/// Hard cap on highlight terms to keep rendering predictable.
const MAX_HIGHLIGHT_TERMS: usize = 8;

/// Minimum title width required before we show a snippet in the results list.
const RESULTS_MIN_TITLE_CHARS: usize = 18;
/// Minimum snippet width required before we show it in the results list.
const RESULTS_MIN_SNIPPET_CHARS: usize = 18;
/// Max chars allocated to the snippet column in the results list.
const RESULTS_MAX_SNIPPET_CHARS_IN_LIST: usize = 60;
/// Separator between title and snippet in the results list.
const RESULTS_SNIPPET_SEP: &str = " | ";

// ──────────────────────────────────────────────────────────────────────
// Facet types
// ──────────────────────────────────────────────────────────────────────

/// Which document kinds to include in results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocKindFilter {
    /// Search messages only (default).
    Messages,
    /// Search agents only.
    Agents,
    /// Search projects only.
    Projects,
    /// Search all document types.
    All,
}

impl DocKindFilter {
    const fn label(self) -> &'static str {
        match self {
            Self::Messages => "Messages",
            Self::Agents => "Agents",
            Self::Projects => "Projects",
            Self::All => "All",
        }
    }

    const fn route_value(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Agents => "agents",
            Self::Projects => "projects",
            Self::All => "all",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Messages => Self::Agents,
            Self::Agents => Self::Projects,
            Self::Projects => Self::All,
            Self::All => Self::Messages,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Messages => Self::All,
            Self::Agents => Self::Messages,
            Self::Projects => Self::Agents,
            Self::All => Self::Projects,
        }
    }

    const fn doc_kind(self) -> Option<DocKind> {
        match self {
            Self::Messages => Some(DocKind::Message),
            Self::Agents => Some(DocKind::Agent),
            Self::Projects => Some(DocKind::Project),
            Self::All => None,
        }
    }
}

/// Importance filter for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportanceFilter {
    Any,
    Urgent,
    High,
    Normal,
}

impl ImportanceFilter {
    const fn label(self) -> &'static str {
        match self {
            Self::Any => "Any",
            Self::Urgent => "Urgent",
            Self::High => "High",
            Self::Normal => "Normal",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Any => Self::Urgent,
            Self::Urgent => Self::High,
            Self::High => Self::Normal,
            Self::Normal => Self::Any,
        }
    }

    const fn importance(self) -> Option<Importance> {
        match self {
            Self::Any => None,
            Self::Urgent => Some(Importance::Urgent),
            Self::High => Some(Importance::High),
            Self::Normal => Some(Importance::Normal),
        }
    }

    fn filter_string(self) -> Option<String> {
        match self {
            Self::Any => None,
            Self::Urgent => Some("urgent".to_string()),
            Self::High => Some("high".to_string()),
            Self::Normal => Some("normal".to_string()),
        }
    }
}

/// Ack-required filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckFilter {
    Any,
    Required,
    NotRequired,
}

impl AckFilter {
    const fn label(self) -> &'static str {
        match self {
            Self::Any => "Any",
            Self::Required => "Ack",
            Self::NotRequired => "No Ack",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Any => Self::Required,
            Self::Required => Self::NotRequired,
            Self::NotRequired => Self::Any,
        }
    }

    const fn filter_value(self) -> Option<bool> {
        match self {
            Self::Any => None,
            Self::Required => Some(true),
            Self::NotRequired => Some(false),
        }
    }
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDirection {
    /// Most recent first (default).
    NewestFirst,
    /// Oldest first.
    OldestFirst,
    /// By relevance score (when searching).
    Relevance,
}

impl SortDirection {
    const fn label(self) -> &'static str {
        match self {
            Self::NewestFirst => "Newest",
            Self::OldestFirst => "Oldest",
            Self::Relevance => "Relevance",
        }
    }

    const fn route_value(self) -> &'static str {
        match self {
            Self::NewestFirst => "newest",
            Self::OldestFirst => "oldest",
            Self::Relevance => "relevance",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::NewestFirst => Self::OldestFirst,
            Self::OldestFirst => Self::Relevance,
            Self::Relevance => Self::NewestFirst,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Search result entry
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ResultEntry {
    id: i64,
    doc_kind: DocKind,
    title: String,
    body_preview: String,
    /// Full message body for markdown preview (messages only, lazy-loaded).
    full_body: Option<String>,
    score: Option<f64>,
    importance: Option<String>,
    ack_required: Option<bool>,
    created_ts: Option<i64>,
    thread_id: Option<String>,
    from_agent: Option<String>,
    project_id: Option<i64>,
}

// ──────────────────────────────────────────────────────────────────────
// Query highlighting + snippet extraction
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryTermKind {
    Word,
    Phrase,
    Prefix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryTerm {
    text: String,
    kind: QueryTermKind,
    negated: bool,
}

fn clean_token(token: &str) -> String {
    token
        .trim_matches(|c: char| {
            !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.' | '/' | '*')
        })
        .to_string()
}

fn extract_query_terms(raw: &str) -> Vec<QueryTerm> {
    let mut terms: Vec<QueryTerm> = Vec::new();
    let mut chars = raw.chars().peekable();
    let mut negate_next = false;

    while let Some(ch) = chars.peek().copied() {
        if ch.is_whitespace() {
            let _ = chars.next();
            continue;
        }

        // Quoted phrase
        if ch == '"' {
            let _ = chars.next();
            let mut phrase = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                phrase.push(c);
            }
            let phrase = phrase.trim();
            if phrase.len() >= 2 {
                terms.push(QueryTerm {
                    text: phrase.to_string(),
                    kind: QueryTermKind::Phrase,
                    negated: std::mem::take(&mut negate_next),
                });
            }
            if terms.len() >= MAX_HIGHLIGHT_TERMS {
                break;
            }
            continue;
        }

        // Unquoted token
        let mut token = String::new();
        while let Some(c) = chars.peek().copied() {
            if c.is_whitespace() {
                break;
            }
            token.push(c);
            let _ = chars.next();
        }

        let token = clean_token(&token);
        if token.is_empty() {
            continue;
        }

        match token.to_ascii_uppercase().as_str() {
            "AND" | "OR" | "NEAR" => continue,
            "NOT" => {
                negate_next = true;
                continue;
            }
            _ => {}
        }

        let (kind, text) = if let Some(stripped) = token.strip_suffix('*') {
            if stripped.len() >= 2 {
                (QueryTermKind::Prefix, stripped.to_string())
            } else {
                continue;
            }
        } else if token.len() >= 2 {
            (QueryTermKind::Word, token)
        } else {
            continue;
        };

        terms.push(QueryTerm {
            text,
            kind,
            negated: std::mem::take(&mut negate_next),
        });
        if terms.len() >= MAX_HIGHLIGHT_TERMS {
            break;
        }
    }

    terms
}

fn clamp_to_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn extract_snippet(text: &str, terms: &[QueryTerm], max_chars: usize) -> String {
    let mut best_pos: Option<usize> = None;
    let mut best_len: usize = 0;

    if !terms.is_empty() {
        let hay = text.to_ascii_lowercase();
        for term in terms.iter().filter(|t| !t.negated) {
            if term.text.len() < 2 {
                continue;
            }
            let needle = term.text.to_ascii_lowercase();
            if let Some(pos) = hay.find(&needle) {
                if best_pos.is_none() || pos < best_pos.unwrap_or(usize::MAX) {
                    best_pos = Some(pos);
                    best_len = needle.len();
                }
            }
        }
    }

    let Some(pos) = best_pos else {
        return truncate_str(text.trim(), max_chars);
    };

    // Byte-based window with UTF-8 boundary clamping.
    let context = max_chars / 2;
    let start = clamp_to_char_boundary(text, pos.saturating_sub(context));
    let end = clamp_to_char_boundary(text, (pos + best_len + context).min(text.len()));
    let slice = text[start..end].trim();

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('\u{2026}');
    }
    snippet.push_str(slice);
    if end < text.len() {
        snippet.push('\u{2026}');
    }

    truncate_str(&snippet, max_chars)
}

fn highlight_spans(
    text: &str,
    terms: &[QueryTerm],
    base_style: Option<Style>,
    highlight_style: Style,
) -> Vec<Span<'static>> {
    let needles: Vec<String> = terms
        .iter()
        .filter(|t| !t.negated)
        .map(|t| t.text.to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .take(MAX_HIGHLIGHT_TERMS)
        .collect();
    if needles.is_empty() {
        return vec![base_style.map_or_else(
            || Span::raw(text.to_string()),
            |style| Span::styled(text.to_string(), style),
        )];
    }

    let hay = text.to_ascii_lowercase();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut i = 0usize;
    while i < text.len() {
        let mut best: Option<(usize, usize)> = None;
        for needle in &needles {
            if let Some(rel) = hay[i..].find(needle) {
                let start = i + rel;
                let end = start + needle.len();
                best = match best {
                    None => Some((start, end)),
                    Some((bs, be)) => {
                        if start < bs || (start == bs && (end - start) > (be - bs)) {
                            Some((start, end))
                        } else {
                            Some((bs, be))
                        }
                    }
                };
            }
        }

        let Some((start, end)) = best else {
            out.push(base_style.map_or_else(
                || Span::raw(text[i..].to_string()),
                |style| Span::styled(text[i..].to_string(), style),
            ));
            break;
        };

        if start > i {
            out.push(base_style.map_or_else(
                || Span::raw(text[i..start].to_string()),
                |style| Span::styled(text[i..start].to_string(), style),
            ));
        }
        if end > start {
            out.push(Span::styled(text[start..end].to_string(), highlight_style));
        }
        i = end;
    }

    out
}

// ──────────────────────────────────────────────────────────────────────
// Focus state
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    QueryBar,
    FacetRail,
    ResultList,
}

/// Which facet is currently highlighted in the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FacetSlot {
    Scope,
    DocKind,
    Importance,
    AckStatus,
    SortOrder,
}

impl FacetSlot {
    const fn next(self) -> Self {
        match self {
            Self::Scope => Self::DocKind,
            Self::DocKind => Self::Importance,
            Self::Importance => Self::AckStatus,
            Self::AckStatus => Self::SortOrder,
            Self::SortOrder => Self::Scope,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Scope => Self::SortOrder,
            Self::DocKind => Self::Scope,
            Self::Importance => Self::DocKind,
            Self::AckStatus => Self::Importance,
            Self::SortOrder => Self::AckStatus,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// SearchCockpitScreen
// ──────────────────────────────────────────────────────────────────────

/// Unified search cockpit with query bar, facet rail, and results.
pub struct SearchCockpitScreen {
    // Query input
    query_input: TextInput,

    // Facet state
    scope_mode: ScopeMode,
    doc_kind_filter: DocKindFilter,
    importance_filter: ImportanceFilter,
    ack_filter: AckFilter,
    sort_direction: SortDirection,
    thread_filter: Option<String>,
    highlight_terms: Vec<QueryTerm>,

    // Results
    results: Vec<ResultEntry>,
    cursor: usize,
    detail_scroll: usize,
    total_sql_rows: usize,

    // Focus
    focus: Focus,
    active_facet: FacetSlot,

    // Search state
    db_conn: Option<SqliteConnection>,
    db_conn_attempted: bool,
    last_query: String,
    last_error: Option<String>,
    debounce_remaining: u8,
    search_dirty: bool,

    // Recipes and history
    saved_recipes: Vec<SearchRecipe>,
    query_history: Vec<QueryHistoryEntry>,
    history_cursor: Option<usize>,
    recipes_loaded: bool,

    /// Synthetic event for the focused search result (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
}

impl SearchCockpitScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            query_input: TextInput::new()
                .with_placeholder("Search across messages, agents, projects... (/ to focus)")
                .with_focused(false),
            scope_mode: ScopeMode::Global,
            doc_kind_filter: DocKindFilter::Messages,
            importance_filter: ImportanceFilter::Any,
            ack_filter: AckFilter::Any,
            sort_direction: SortDirection::NewestFirst,
            thread_filter: None,
            highlight_terms: Vec::new(),
            results: Vec::new(),
            cursor: 0,
            detail_scroll: 0,
            total_sql_rows: 0,
            focus: Focus::ResultList,
            active_facet: FacetSlot::DocKind,
            db_conn: None,
            db_conn_attempted: false,
            last_query: String::new(),
            last_error: None,
            debounce_remaining: 0,
            search_dirty: true,
            saved_recipes: Vec::new(),
            query_history: Vec::new(),
            history_cursor: None,
            recipes_loaded: false,
            focused_synthetic: None,
        }
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected search result.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self.results.get(self.cursor).and_then(|entry| {
            match entry.doc_kind {
                DocKind::Message => Some(crate::tui_events::MailEvent::message_sent(
                    entry.id,
                    entry.from_agent.as_deref().unwrap_or(""),
                    vec![], // to-agents not stored in search results
                    &entry.title,
                    entry.thread_id.as_deref().unwrap_or(""),
                    "", // project slug not directly available
                )),
                DocKind::Agent => Some(crate::tui_events::MailEvent::agent_registered(
                    &entry.title,
                    "",
                    "",
                    "",
                )),
                DocKind::Project => None, // no good synthetic event for projects
            }
        });
    }

    /// Ensure we have a DB connection.
    fn ensure_db_conn(&mut self, state: &TuiSharedState) {
        if self.db_conn.is_some() || self.db_conn_attempted {
            return;
        }
        self.db_conn_attempted = true;
        let db_url = &state.config_snapshot().database_url;
        let cfg = DbPoolConfig {
            database_url: db_url.clone(),
            ..Default::default()
        };
        if let Ok(path) = cfg.sqlite_path() {
            self.db_conn = SqliteConnection::open_file(&path).ok();
            if self.db_conn.is_some() {
                self.ensure_recipes_loaded();
            }
        }
    }

    /// Build a `SearchQuery` from the current facet state.
    #[cfg(test)]
    fn build_query(&self) -> SearchQuery {
        let raw = self.query_input.value().trim().to_string();
        let doc_kind = self.doc_kind_filter.doc_kind().unwrap_or(DocKind::Message);

        let mut query = SearchQuery {
            text: raw,
            doc_kind,
            limit: Some(MAX_RESULTS),
            ..Default::default()
        };

        // Apply ranking mode
        query.ranking = match self.sort_direction {
            SortDirection::Relevance => RankingMode::Relevance,
            SortDirection::NewestFirst | SortDirection::OldestFirst => RankingMode::Recency,
        };

        // Apply importance facet
        if let Some(imp) = self.importance_filter.importance() {
            query.importance = vec![imp];
        }

        // Apply ack filter
        if let Some(ack) = self.ack_filter.filter_value() {
            query.ack_required = Some(ack);
        }

        // Apply thread filter
        if let Some(ref tid) = self.thread_filter {
            query.thread_id = Some(tid.clone());
        }

        query
    }

    /// Execute the search using sync DB connection.
    fn execute_search(&mut self, state: &TuiSharedState) {
        let raw = self.query_input.value().trim().to_string();
        self.last_query.clone_from(&raw);
        self.last_error = validate_query_syntax(&raw);
        if self.last_error.is_some() {
            self.highlight_terms.clear();
            self.results.clear();
            self.total_sql_rows = 0;
            self.cursor = 0;
            self.detail_scroll = 0;
            self.search_dirty = false;
            return;
        }

        self.highlight_terms = extract_query_terms(&raw);

        self.ensure_db_conn(state);
        let Some(conn) = self.db_conn.take() else {
            return;
        };

        if self.doc_kind_filter == DocKindFilter::All {
            // Run all three kinds and merge
            let mut all_results = Vec::new();
            for kind in &[DocKind::Message, DocKind::Agent, DocKind::Project] {
                let results = self.run_kind_search(&conn, *kind, &raw);
                all_results.extend(results);
            }
            sort_results(&mut all_results, self.sort_direction);
            all_results.truncate(MAX_RESULTS);
            self.total_sql_rows = all_results.len();
            self.results = all_results;
        } else {
            let kind = self.doc_kind_filter.doc_kind().unwrap_or(DocKind::Message);
            let results = self.run_kind_search(&conn, kind, &raw);
            let mut results = results;
            sort_results(&mut results, self.sort_direction);
            self.total_sql_rows = results.len();
            self.results = results;
        }

        self.db_conn = Some(conn);

        // Clamp cursor
        if self.results.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.results.len() - 1);
        }
        self.detail_scroll = 0;
        self.search_dirty = false;
        self.record_history();
    }

    /// Run a search for a single doc kind using sync queries.
    fn run_kind_search(
        &mut self,
        conn: &SqliteConnection,
        kind: DocKind,
        raw: &str,
    ) -> Vec<ResultEntry> {
        match kind {
            DocKind::Message => self.search_messages(conn, raw),
            DocKind::Agent => Self::search_agents(conn, raw),
            DocKind::Project => Self::search_projects(conn, raw),
        }
    }

    /// Search messages using the global planner for non-empty queries.
    fn search_messages(&mut self, conn: &SqliteConnection, raw: &str) -> Vec<ResultEntry> {
        if raw.is_empty() {
            return self.search_messages_recent(conn);
        }

        let mut query = SearchQuery {
            text: raw.to_string(),
            doc_kind: DocKind::Message,
            limit: Some(MAX_RESULTS),
            ..Default::default()
        };
        query.ranking = match self.sort_direction {
            SortDirection::Relevance => RankingMode::Relevance,
            SortDirection::NewestFirst | SortDirection::OldestFirst => RankingMode::Recency,
        };

        if let Some(imp) = self.importance_filter.importance() {
            query.importance = vec![imp];
        }
        if let Some(ack) = self.ack_filter.filter_value() {
            query.ack_required = Some(ack);
        }
        if let Some(ref tid) = self.thread_filter {
            query.thread_id = Some(tid.clone());
        }

        let plan = plan_search(&query);
        if plan.sql.is_empty() {
            return Vec::new();
        }

        let params: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
        match query_message_rows(conn, &plan.sql, &params, &self.highlight_terms) {
            Ok(results) => results,
            Err(e) => {
                self.last_error = Some(format!("Search failed: {e}"));
                Vec::new()
            }
        }
    }

    /// Recent messages view (empty query).
    fn search_messages_recent(&mut self, conn: &SqliteConnection) -> Vec<ResultEntry> {
        let mut where_clauses: Vec<&str> = Vec::new();
        let mut params: Vec<Value> = Vec::new();

        if let Some(ref imp) = self.importance_filter.filter_string() {
            where_clauses.push("m.importance = ?");
            params.push(Value::Text(imp.clone()));
        }
        if let Some(ack) = self.ack_filter.filter_value() {
            where_clauses.push("m.ack_required = ?");
            params.push(Value::BigInt(i64::from(ack)));
        }
        if let Some(ref tid) = self.thread_filter {
            where_clauses.push("m.thread_id = ?");
            params.push(Value::Text(tid.clone()));
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", where_clauses.join(" AND "))
        };

        let order_clause = match self.sort_direction {
            SortDirection::OldestFirst => "m.created_ts ASC, m.id ASC",
            SortDirection::NewestFirst | SortDirection::Relevance => "m.created_ts DESC, m.id ASC",
        };

        let sql = format!(
            "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, \
             m.thread_id, a.name AS from_name, m.body_md, m.project_id, 0.0 AS score \
             FROM messages m \
             LEFT JOIN agents a ON a.id = m.sender_id{where_sql} \
             ORDER BY {order_clause} \
             LIMIT ?"
        );
        params.push(Value::BigInt(i64::try_from(MAX_RESULTS).unwrap_or(50)));

        match query_message_rows(conn, &sql, &params, &self.highlight_terms) {
            Ok(results) => results,
            Err(e) => {
                self.last_error = Some(format!("Search failed: {e}"));
                Vec::new()
            }
        }
    }

    /// Search agents.
    fn search_agents(conn: &SqliteConnection, raw: &str) -> Vec<ResultEntry> {
        if raw.is_empty() {
            let sql = "SELECT id, name, task_description, project_id, 0.0 AS score \
                       FROM agents ORDER BY name LIMIT 100";
            return query_agent_rows(conn, sql, &[]);
        }

        let query = SearchQuery {
            text: raw.to_string(),
            doc_kind: DocKind::Agent,
            limit: Some(MAX_RESULTS),
            ..Default::default()
        };
        let plan = plan_search(&query);
        if plan.sql.is_empty() {
            return Vec::new();
        }
        let params: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
        query_agent_rows(conn, &plan.sql, &params)
    }

    /// Search projects.
    fn search_projects(conn: &SqliteConnection, raw: &str) -> Vec<ResultEntry> {
        if raw.is_empty() {
            let sql = "SELECT id, slug, human_key, 0.0 AS score \
                       FROM projects ORDER BY slug LIMIT 100";
            return query_project_rows(conn, sql, &[]);
        }

        let query = SearchQuery {
            text: raw.to_string(),
            doc_kind: DocKind::Project,
            limit: Some(MAX_RESULTS),
            ..Default::default()
        };
        let plan = plan_search(&query);
        if plan.sql.is_empty() {
            return Vec::new();
        }
        let params: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
        query_project_rows(conn, &plan.sql, &params)
    }

    /// Toggle the active facet's value.
    #[allow(clippy::missing_const_for_fn)] // mutates self through .next() chains
    fn toggle_active_facet(&mut self) {
        match self.active_facet {
            FacetSlot::Scope => self.scope_mode = self.scope_mode.next(),
            FacetSlot::DocKind => self.doc_kind_filter = self.doc_kind_filter.next(),
            FacetSlot::Importance => self.importance_filter = self.importance_filter.next(),
            FacetSlot::AckStatus => self.ack_filter = self.ack_filter.next(),
            FacetSlot::SortOrder => self.sort_direction = self.sort_direction.next(),
        }
        self.search_dirty = true;
        self.debounce_remaining = 0;
    }

    /// Clear all facets to defaults.
    fn reset_facets(&mut self) {
        self.scope_mode = ScopeMode::Global;
        self.doc_kind_filter = DocKindFilter::Messages;
        self.importance_filter = ImportanceFilter::Any;
        self.ack_filter = AckFilter::Any;
        self.sort_direction = SortDirection::NewestFirst;
        self.thread_filter = None;
        self.search_dirty = true;
        self.debounce_remaining = 0;
    }

    /// Load saved recipes and recent history from the DB (once).
    fn ensure_recipes_loaded(&mut self) {
        if self.recipes_loaded {
            return;
        }
        self.recipes_loaded = true;
        if let Some(ref conn) = self.db_conn {
            self.saved_recipes = list_recipes(conn).unwrap_or_default();
            self.query_history = list_recent_history(conn, 50).unwrap_or_default();
        }
    }

    /// Record the current query to history.
    fn record_history(&mut self) {
        let text = self.query_input.value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let entry = QueryHistoryEntry {
            query_text: text,
            doc_kind: self.doc_kind_filter.route_value().to_string(),
            scope_mode: self.scope_mode,
            scope_id: None,
            result_count: i64::try_from(self.results.len()).unwrap_or(0),
            executed_ts: now_micros(),
            ..Default::default()
        };
        if let Some(ref conn) = self.db_conn {
            let _ = insert_history(conn, &entry);
        }
        // Prepend to in-memory history
        self.query_history.insert(0, entry);
        self.query_history.truncate(50);
        self.history_cursor = None;
    }

    /// Save current search state as a named recipe.
    #[allow(dead_code)] // In-progress: called once recipe save UI is wired up.
    fn save_current_as_recipe(&mut self, name: String) {
        let recipe = SearchRecipe {
            name,
            query_text: self.query_input.value().trim().to_string(),
            doc_kind: self.doc_kind_filter.route_value().to_string(),
            scope_mode: self.scope_mode,
            importance_filter: self.importance_filter.filter_string().unwrap_or_default(),
            ack_filter: match self.ack_filter {
                AckFilter::Any => "any".to_string(),
                AckFilter::Required => "required".to_string(),
                AckFilter::NotRequired => "not_required".to_string(),
            },
            sort_mode: self.sort_direction.route_value().to_string(),
            thread_filter: self.thread_filter.clone(),
            ..Default::default()
        };
        if let Some(ref conn) = self.db_conn {
            if let Ok(id) = insert_recipe(conn, &recipe) {
                let mut saved = recipe;
                saved.id = Some(id);
                self.saved_recipes.insert(0, saved);
            }
        }
    }

    /// Load a recipe into the current search state.
    #[allow(dead_code)] // In-progress: called once recipe load UI is wired up.
    fn load_recipe(&mut self, recipe: &SearchRecipe) {
        self.query_input.set_value(&recipe.query_text);
        self.scope_mode = recipe.scope_mode;
        self.doc_kind_filter = match recipe.doc_kind.as_str() {
            "agents" => DocKindFilter::Agents,
            "projects" => DocKindFilter::Projects,
            "all" => DocKindFilter::All,
            _ => DocKindFilter::Messages,
        };
        self.sort_direction = match recipe.sort_mode.as_str() {
            "oldest" => SortDirection::OldestFirst,
            "relevance" => SortDirection::Relevance,
            _ => SortDirection::NewestFirst,
        };
        self.ack_filter = match recipe.ack_filter.as_str() {
            "required" => AckFilter::Required,
            "not_required" => AckFilter::NotRequired,
            _ => AckFilter::Any,
        };
        self.thread_filter.clone_from(&recipe.thread_filter);
        self.search_dirty = true;
        self.debounce_remaining = 0;

        // Touch the recipe's use count
        if let (Some(conn), Some(id)) = (&self.db_conn, recipe.id) {
            let _ = touch_recipe(conn, id);
        }
    }

    fn route_string(&self) -> String {
        let mut params: Vec<(&'static str, String)> = Vec::new();

        let q = self.query_input.value().trim();
        if !q.is_empty() {
            params.push(("q", url_encode_component(q)));
        }
        if self.scope_mode != ScopeMode::Global {
            params.push(("scope", self.scope_mode.as_str().to_string()));
        }
        if self.doc_kind_filter != DocKindFilter::Messages {
            params.push(("type", self.doc_kind_filter.route_value().to_string()));
        }
        if let Some(imp) = self.importance_filter.filter_string() {
            params.push(("imp", url_encode_component(&imp)));
        }
        if let Some(ack) = self.ack_filter.filter_value() {
            params.push((
                "ack",
                if ack {
                    "1".to_string()
                } else {
                    "0".to_string()
                },
            ));
        }
        if self.sort_direction != SortDirection::NewestFirst {
            params.push(("sort", self.sort_direction.route_value().to_string()));
        }
        if let Some(ref tid) = self.thread_filter {
            params.push(("thread", url_encode_component(tid)));
        }

        if params.is_empty() {
            return "/search".to_string();
        }

        let mut out = String::from("/search?");
        for (i, (k, v)) in params.into_iter().enumerate() {
            if i > 0 {
                out.push('&');
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&v);
        }
        out
    }
}

fn validate_query_syntax(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Simple, deterministic validation: avoid FTS5 parse failures while typing.
    let quote_count = trimmed.chars().filter(|c| *c == '"').count();
    if quote_count % 2 == 1 {
        return Some("Unbalanced quotes: close your \"phrase\"".to_string());
    }

    // Bare boolean operators can't yield meaningful results.
    match trimmed.to_ascii_uppercase().as_str() {
        "AND" | "OR" | "NOT" => {
            return Some("Query must include search terms (bare boolean operator)".to_string());
        }
        _ => {}
    }

    None
}

const fn doc_kind_order(kind: DocKind) -> u8 {
    match kind {
        DocKind::Message => 0,
        DocKind::Agent => 1,
        DocKind::Project => 2,
    }
}

fn sort_results(results: &mut [ResultEntry], mode: SortDirection) {
    match mode {
        SortDirection::Relevance => results.sort_by(|a, b| {
            let sa = a.score.unwrap_or(f64::INFINITY);
            let sb = b.score.unwrap_or(f64::INFINITY);
            let ord = sa.total_cmp(&sb);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            let ord = doc_kind_order(a.doc_kind).cmp(&doc_kind_order(b.doc_kind));
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            let ta = a.created_ts.unwrap_or(i64::MIN);
            let tb = b.created_ts.unwrap_or(i64::MIN);
            let ord = tb.cmp(&ta); // newest first as a stable tiebreak
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            a.id.cmp(&b.id)
        }),
        SortDirection::NewestFirst => results.sort_by(|a, b| {
            let ta = a.created_ts.unwrap_or(i64::MIN);
            let tb = b.created_ts.unwrap_or(i64::MIN);
            let ord = tb.cmp(&ta);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            let ord = doc_kind_order(a.doc_kind).cmp(&doc_kind_order(b.doc_kind));
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            a.id.cmp(&b.id)
        }),
        SortDirection::OldestFirst => results.sort_by(|a, b| {
            let ta = a.created_ts.unwrap_or(i64::MAX);
            let tb = b.created_ts.unwrap_or(i64::MAX);
            let ord = ta.cmp(&tb);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            let ord = doc_kind_order(a.doc_kind).cmp(&doc_kind_order(b.doc_kind));
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            a.id.cmp(&b.id)
        }),
    }
}

impl Default for SearchCockpitScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for SearchCockpitScreen {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match self.focus {
                    Focus::QueryBar => match key.code {
                        KeyCode::Enter => {
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                            self.focus = Focus::ResultList;
                            self.query_input.set_focused(false);
                            self.history_cursor = None;
                        }
                        KeyCode::Escape => {
                            self.focus = Focus::ResultList;
                            self.query_input.set_focused(false);
                            self.history_cursor = None;
                        }
                        KeyCode::Tab => {
                            self.focus = Focus::FacetRail;
                            self.query_input.set_focused(false);
                        }
                        KeyCode::Up => {
                            // Recall previous history entry
                            if !self.query_history.is_empty() {
                                let next = match self.history_cursor {
                                    None => 0,
                                    Some(c) => (c + 1).min(self.query_history.len() - 1),
                                };
                                self.history_cursor = Some(next);
                                self.query_input
                                    .set_value(&self.query_history[next].query_text);
                                self.search_dirty = true;
                                self.debounce_remaining = DEBOUNCE_TICKS;
                            }
                        }
                        KeyCode::Down => {
                            // Recall more recent history entry
                            if let Some(c) = self.history_cursor {
                                if c == 0 {
                                    self.history_cursor = None;
                                    self.query_input.clear();
                                } else {
                                    let next = c - 1;
                                    self.history_cursor = Some(next);
                                    self.query_input
                                        .set_value(&self.query_history[next].query_text);
                                }
                                self.search_dirty = true;
                                self.debounce_remaining = DEBOUNCE_TICKS;
                            }
                        }
                        _ => {
                            let before = self.query_input.value().to_string();
                            self.query_input.handle_event(event);
                            if self.query_input.value() != before {
                                self.search_dirty = true;
                                self.debounce_remaining = DEBOUNCE_TICKS;
                                self.history_cursor = None;
                            }
                        }
                    },

                    Focus::FacetRail => match key.code {
                        KeyCode::Escape | KeyCode::Char('q') | KeyCode::Tab => {
                            self.focus = Focus::ResultList;
                        }
                        KeyCode::Char('/') => {
                            self.focus = Focus::QueryBar;
                            self.query_input.set_focused(true);
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            self.active_facet = self.active_facet.next();
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            self.active_facet = self.active_facet.prev();
                        }
                        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => {
                            self.toggle_active_facet();
                        }
                        KeyCode::Left => {
                            // Reverse toggle
                            match self.active_facet {
                                FacetSlot::Scope => {
                                    self.scope_mode = self.scope_mode.next();
                                }
                                FacetSlot::DocKind => {
                                    self.doc_kind_filter = self.doc_kind_filter.prev();
                                }
                                FacetSlot::Importance => {
                                    // cycle backwards not worth adding, just use next
                                    self.importance_filter = self.importance_filter.next();
                                }
                                FacetSlot::AckStatus => {
                                    self.ack_filter = self.ack_filter.next();
                                }
                                FacetSlot::SortOrder => {
                                    self.sort_direction = self.sort_direction.next();
                                }
                            }
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                        }
                        KeyCode::Char('r') => {
                            self.reset_facets();
                        }
                        _ => {}
                    },

                    Focus::ResultList => match key.code {
                        KeyCode::Char('/') => {
                            self.focus = Focus::QueryBar;
                            self.query_input.set_focused(true);
                        }
                        KeyCode::Tab | KeyCode::Char('f') => {
                            self.focus = Focus::FacetRail;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if !self.results.is_empty() {
                                self.cursor = (self.cursor + 1).min(self.results.len() - 1);
                                self.detail_scroll = 0;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            self.cursor = self.cursor.saturating_sub(1);
                            self.detail_scroll = 0;
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            if !self.results.is_empty() {
                                self.cursor = self.results.len() - 1;
                                self.detail_scroll = 0;
                            }
                        }
                        KeyCode::Char('g') | KeyCode::Home => {
                            self.cursor = 0;
                            self.detail_scroll = 0;
                        }
                        KeyCode::Char('d') | KeyCode::PageDown => {
                            if !self.results.is_empty() {
                                self.cursor = (self.cursor + 20).min(self.results.len() - 1);
                                self.detail_scroll = 0;
                            }
                        }
                        KeyCode::Char('u') | KeyCode::PageUp => {
                            self.cursor = self.cursor.saturating_sub(20);
                            self.detail_scroll = 0;
                        }
                        KeyCode::Char('J') => {
                            self.detail_scroll += 1;
                        }
                        KeyCode::Char('K') => {
                            self.detail_scroll = self.detail_scroll.saturating_sub(1);
                        }
                        // Deep-link: Enter on result
                        KeyCode::Enter => {
                            if let Some(entry) = self.results.get(self.cursor) {
                                return Cmd::msg(match entry.doc_kind {
                                    DocKind::Message => MailScreenMsg::DeepLink(
                                        DeepLinkTarget::MessageById(entry.id),
                                    ),
                                    DocKind::Agent => MailScreenMsg::DeepLink(
                                        DeepLinkTarget::AgentByName(entry.title.clone()),
                                    ),
                                    DocKind::Project => MailScreenMsg::DeepLink(
                                        DeepLinkTarget::ProjectBySlug(entry.title.clone()),
                                    ),
                                });
                            }
                        }
                        // Cycle doc kind from results
                        KeyCode::Char('t') => {
                            self.doc_kind_filter = self.doc_kind_filter.next();
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                        }
                        // Cycle importance from results
                        KeyCode::Char('i') => {
                            self.importance_filter = self.importance_filter.next();
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                        }
                        // Jump to thread (messages only)
                        KeyCode::Char('o') => {
                            if let Some(entry) = self.results.get(self.cursor) {
                                if let Some(ref tid) = entry.thread_id {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::ThreadById(tid.clone()),
                                    ));
                                }
                            }
                        }
                        // Jump to agent profile
                        KeyCode::Char('a') => {
                            if let Some(entry) = self.results.get(self.cursor) {
                                if let Some(ref agent) = entry.from_agent {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::AgentByName(agent.clone()),
                                    ));
                                }
                            }
                        }
                        // Jump to timeline at message time
                        KeyCode::Char('T') => {
                            if let Some(entry) = self.results.get(self.cursor) {
                                if let Some(ts) = entry.created_ts {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::TimelineAtTime(ts),
                                    ));
                                }
                            }
                        }
                        // Clear search
                        KeyCode::Char('c') if key.modifiers.contains(Modifiers::CTRL) => {
                            self.query_input.clear();
                            self.reset_facets();
                        }
                        _ => {}
                    },
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, _tick_count: u64, state: &TuiSharedState) {
        if self.search_dirty {
            if self.debounce_remaining > 0 {
                self.debounce_remaining -= 1;
            } else {
                self.execute_search(state);
            }
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 4 || area.width < 30 {
            return;
        }

        // Layout: query bar (3-4h) + body
        let query_h: u16 = if area.height >= 6 { 4 } else { 3 };
        let body_h = area.height.saturating_sub(query_h);

        let query_area = Rect::new(area.x, area.y, area.width, query_h);
        let body_area = Rect::new(area.x, area.y + query_h, area.width, body_h);

        // Render query bar
        render_query_bar(frame, query_area, &self.query_input, self);

        // Body: facet rail (left) + results + detail (right)
        let facet_w: u16 = if area.width >= 100 { 20 } else { 16 };
        let remaining_w = body_area.width.saturating_sub(facet_w);

        let facet_area = Rect::new(body_area.x, body_area.y, facet_w, body_area.height);

        // Results + detail split
        if remaining_w >= 60 {
            let results_w = remaining_w * 45 / 100;
            let detail_w = remaining_w - results_w;
            let results_area = Rect::new(
                body_area.x + facet_w,
                body_area.y,
                results_w,
                body_area.height,
            );
            let detail_area = Rect::new(
                body_area.x + facet_w + results_w,
                body_area.y,
                detail_w,
                body_area.height,
            );

            render_facet_rail(frame, facet_area, self);
            render_results(
                frame,
                results_area,
                &self.results,
                self.cursor,
                &self.highlight_terms,
                self.sort_direction,
            );
            render_detail(
                frame,
                detail_area,
                self.results.get(self.cursor),
                self.detail_scroll,
                &self.highlight_terms,
            );
        } else {
            // Narrow: facet rail + results only
            let results_area = Rect::new(
                body_area.x + facet_w,
                body_area.y,
                remaining_w,
                body_area.height,
            );
            render_facet_rail(frame, facet_area, self);
            render_results(
                frame,
                results_area,
                &self.results,
                self.cursor,
                &self.highlight_terms,
                self.sort_direction,
            );
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "/",
                action: "Focus query bar",
            },
            HelpEntry {
                key: "f",
                action: "Focus facet rail",
            },
            HelpEntry {
                key: "Tab",
                action: "Cycle focus",
            },
            HelpEntry {
                key: "j/k",
                action: "Navigate",
            },
            HelpEntry {
                key: "Enter",
                action: "Toggle facet / Deep-link",
            },
            HelpEntry {
                key: "t",
                action: "Cycle doc type",
            },
            HelpEntry {
                key: "i",
                action: "Cycle importance",
            },
            HelpEntry {
                key: "d/u",
                action: "Page down/up",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail",
            },
            HelpEntry {
                key: "o",
                action: "Open thread",
            },
            HelpEntry {
                key: "a",
                action: "Jump to agent",
            },
            HelpEntry {
                key: "T",
                action: "Timeline at time",
            },
            HelpEntry {
                key: "Ctrl+C",
                action: "Clear all",
            },
            HelpEntry {
                key: "r",
                action: "Reset facets",
            },
            HelpEntry {
                key: "\u{2191}/\u{2193}",
                action: "Query history (in query bar)",
            },
            HelpEntry {
                key: "\"phrase\"",
                action: "Phrase search",
            },
            HelpEntry {
                key: "term*",
                action: "Prefix search",
            },
            HelpEntry {
                key: "AND/OR/NOT",
                action: "Boolean operators",
            },
            HelpEntry {
                key: "NOT term",
                action: "Exclude term",
            },
        ]
    }

    fn consumes_text_input(&self) -> bool {
        matches!(self.focus, Focus::QueryBar)
    }

    fn title(&self) -> &'static str {
        "Search"
    }

    fn tab_label(&self) -> &'static str {
        "Find"
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        match target {
            DeepLinkTarget::ThreadById(tid) => {
                // Set thread filter and search
                self.thread_filter = Some(tid.clone());
                self.doc_kind_filter = DocKindFilter::Messages;
                self.search_dirty = true;
                self.debounce_remaining = 0;
                true
            }
            _ => false,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// DB query helpers
// ──────────────────────────────────────────────────────────────────────

fn plan_param_to_value(param: &mcp_agent_mail_db::search_planner::PlanParam) -> Value {
    match param {
        mcp_agent_mail_db::search_planner::PlanParam::Int(v) => Value::BigInt(*v),
        mcp_agent_mail_db::search_planner::PlanParam::Text(s) => Value::Text(s.clone()),
        mcp_agent_mail_db::search_planner::PlanParam::Float(f) => Value::Double(*f),
    }
}

fn query_message_rows(
    conn: &SqliteConnection,
    sql: &str,
    params: &[Value],
    highlight_terms: &[QueryTerm],
) -> Result<Vec<ResultEntry>, String> {
    conn.query_sync(sql, params)
        .map_err(|e| e.to_string())
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let id: i64 = row.get_named("id").ok()?;
                    let subject: String = row.get_named("subject").unwrap_or_default();
                    let body: String = row.get_named("body_md").unwrap_or_default();
                    let body = collapse_whitespace(&body);
                    let preview = if highlight_terms.is_empty() {
                        truncate_str(&body, 120)
                    } else {
                        extract_snippet(&body, highlight_terms, MAX_SNIPPET_CHARS)
                    };
                    Some(ResultEntry {
                        id,
                        doc_kind: DocKind::Message,
                        title: subject,
                        body_preview: preview,
                        full_body: Some(body),
                        score: row.get_named("score").ok(),
                        importance: row.get_named("importance").ok(),
                        ack_required: row.get_named::<i64>("ack_required").ok().map(|v| v != 0),
                        created_ts: row.get_named("created_ts").ok(),
                        thread_id: row.get_named("thread_id").ok(),
                        from_agent: row.get_named("from_name").ok(),
                        project_id: row.get_named("project_id").ok(),
                    })
                })
                .collect()
        })
}

fn query_agent_rows(conn: &SqliteConnection, sql: &str, params: &[Value]) -> Vec<ResultEntry> {
    conn.query_sync(sql, params)
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let id: i64 = row.get_named("id").ok()?;
                    let name: String = row.get_named("name").unwrap_or_default();
                    let desc: String = row.get_named("task_description").unwrap_or_default();
                    let desc = collapse_whitespace(&desc);
                    Some(ResultEntry {
                        id,
                        doc_kind: DocKind::Agent,
                        title: name,
                        body_preview: truncate_str(&desc, 120),
                        full_body: None,
                        score: row.get_named("score").ok(),
                        importance: None,
                        ack_required: None,
                        created_ts: None,
                        thread_id: None,
                        from_agent: None,
                        project_id: row.get_named("project_id").ok(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn query_project_rows(conn: &SqliteConnection, sql: &str, params: &[Value]) -> Vec<ResultEntry> {
    conn.query_sync(sql, params)
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let id: i64 = row.get_named("id").ok()?;
                    let slug: String = row.get_named("slug").unwrap_or_default();
                    let human_key: String = row.get_named("human_key").unwrap_or_default();
                    Some(ResultEntry {
                        id,
                        doc_kind: DocKind::Project,
                        title: slug,
                        body_preview: human_key,
                        full_body: None,
                        score: row.get_named("score").ok(),
                        importance: None,
                        ack_required: None,
                        created_ts: None,
                        thread_id: None,
                        from_agent: None,
                        project_id: Some(id),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Truncate a string to `max_chars`, adding ellipsis if needed.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let mut t = s[..max_chars.saturating_sub(1)].to_string();
        t.push('\u{2026}');
        t
    }
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = true; // trim leading whitespace
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn url_encode_component(s: &str) -> String {
    // Minimal percent-encoding for deterministic deeplink-style routes.
    // Encodes all bytes outside the unreserved set.
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len() + 8);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[(b >> 4) as usize]));
                out.push(char::from(HEX[(b & 0x0F) as usize]));
            }
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────
// Rendering helpers
// ──────────────────────────────────────────────────────────────────────

const FACET_ACTIVE_FG: PackedRgba = PackedRgba::rgba(0x5F, 0xAF, 0xFF, 0xFF); // Blue
const FACET_LABEL_FG: PackedRgba = PackedRgba::rgba(0x87, 0x87, 0x87, 0xFF); // Grey
const RESULT_CURSOR_FG: PackedRgba = PackedRgba::rgba(0xFF, 0xD7, 0x00, 0xFF); // Yellow
const ERROR_FG: PackedRgba = PackedRgba::rgba(0xFF, 0x5F, 0x5F, 0xFF); // Red
const ACTION_KEY_FG: PackedRgba = PackedRgba::rgba(0x87, 0xD7, 0x87, 0xFF); // Green

fn render_query_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &TextInput,
    screen: &SearchCockpitScreen,
) {
    let count = screen.results.len();
    let kind_label = screen.doc_kind_filter.label();
    let focus_label = if screen.focus == Focus::QueryBar {
        " [EDITING]"
    } else {
        ""
    };
    let thread_label = if screen.thread_filter.is_some() {
        " +thread"
    } else {
        ""
    };

    let title = format!("Search {kind_label} ({count} results){thread_label}{focus_label}");

    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    input.render(input_area, frame);

    // Optional hint line when the query bar has extra height.
    if inner.height >= 2 {
        let w = inner.width as usize;
        let (hint, style) = screen.last_error.as_ref().map_or_else(
            || {
                if screen.focus == Focus::QueryBar {
                    (
                        "Syntax: \"phrase\" term* AND/OR/NOT (no leading *)".to_string(),
                        Style::default().fg(FACET_LABEL_FG),
                    )
                } else {
                    (
                        format!("Route: {}", screen.route_string()),
                        Style::default().fg(FACET_LABEL_FG),
                    )
                }
            },
            |err| (format!("ERR: {err}"), Style::default().fg(ERROR_FG)),
        );

        let hint_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
        Paragraph::new(truncate_str(&hint, w))
            .style(style)
            .render(hint_area, frame);
    }
}

fn render_facet_rail(frame: &mut Frame<'_>, area: Rect, screen: &SearchCockpitScreen) {
    let block = Block::default()
        .title("Facets")
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let in_rail = screen.focus == Focus::FacetRail;
    let w = inner.width as usize;

    let facets: &[(FacetSlot, &str, &str)] = &[
        (FacetSlot::Scope, "Scope", screen.scope_mode.as_str()),
        (FacetSlot::DocKind, "Type", screen.doc_kind_filter.label()),
        (
            FacetSlot::Importance,
            "Imp.",
            screen.importance_filter.label(),
        ),
        (FacetSlot::AckStatus, "Ack", screen.ack_filter.label()),
        (FacetSlot::SortOrder, "Sort", screen.sort_direction.label()),
    ];

    for (i, &(slot, label, value)) in facets.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)] // max 4 facets
        let y = inner.y + (i as u16) * 2;
        if y >= inner.y + inner.height {
            break;
        }

        let is_active = in_rail && screen.active_facet == slot;
        let marker = if is_active { '>' } else { ' ' };

        let label_style = if is_active {
            Style::default().fg(FACET_ACTIVE_FG)
        } else {
            Style::default().fg(FACET_LABEL_FG)
        };

        // Label row
        let label_text = format!("{marker} {label}");
        let label_line = truncate_str(&label_text, w);
        let label_area = Rect::new(inner.x, y, inner.width, 1);
        Paragraph::new(label_line)
            .style(label_style)
            .render(label_area, frame);

        // Value row (indented)
        let value_y = y + 1;
        if value_y < inner.y + inner.height {
            let val_text = format!("  [{value}]");
            let val_line = truncate_str(&val_text, w);
            let val_area = Rect::new(inner.x, value_y, inner.width, 1);
            let val_style = if is_active {
                Style::default().fg(RESULT_CURSOR_FG)
            } else {
                Style::default()
            };
            Paragraph::new(val_line)
                .style(val_style)
                .render(val_area, frame);
        }
    }

    // Thread filter indicator
    if let Some(ref tid) = screen.thread_filter {
        let y = inner.y + 10;
        if y + 1 < inner.y + inner.height {
            let thread_text = format!("  Thread: {}", truncate_str(tid, w.saturating_sub(10)));
            let thread_area = Rect::new(inner.x, y, inner.width, 1);
            Paragraph::new(thread_text)
                .style(Style::default().fg(FACET_ACTIVE_FG))
                .render(thread_area, frame);
        }
    }

    // Help hint at bottom
    let help_y = inner.y + inner.height - 1;
    if help_y > inner.y + 11 {
        let hint = if in_rail {
            "Enter:toggle r:reset"
        } else {
            "f:facets"
        };
        let hint_area = Rect::new(inner.x, help_y, inner.width, 1);
        Paragraph::new(truncate_str(hint, w))
            .style(Style::default().fg(FACET_LABEL_FG))
            .render(hint_area, frame);
    }
}

fn created_time_hms(created_ts: Option<i64>) -> String {
    created_ts
        .map(|ts| {
            let iso = micros_to_iso(ts);
            if iso.len() >= 19 {
                iso[11..19].to_string()
            } else {
                iso
            }
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy)]
struct ResultListRenderCfg<'a> {
    width: usize,
    highlight_terms: &'a [QueryTerm],
    sort_direction: SortDirection,
    meta_style: Style,
    cursor_style: Style,
    snippet_style: Style,
    highlight_style: Style,
}

#[allow(clippy::too_many_lines)]
fn result_entry_line(entry: &ResultEntry, is_cursor: bool, cfg: &ResultListRenderCfg<'_>) -> Line {
    let marker = if is_cursor { '>' } else { ' ' };

    let kind_badge = match entry.doc_kind {
        DocKind::Message => "M",
        DocKind::Agent => "A",
        DocKind::Project => "P",
    };

    let imp_badge = match entry.importance.as_deref() {
        Some("urgent") => "!!",
        Some("high") => "!",
        _ => " ",
    };

    let time = created_time_hms(entry.created_ts);
    let proj = entry
        .project_id
        .map_or_else(|| "-".to_string(), |pid| format!("p#{pid}"));

    let score_col = if cfg.sort_direction == SortDirection::Relevance {
        entry
            .score
            .map_or_else(|| "      ".to_string(), |s| format!("{s:>6.2}"))
    } else {
        String::new()
    };

    let mut prefix = if cfg.sort_direction == SortDirection::Relevance {
        format!(
            "{marker} {kind_badge} {imp_badge:>2} {proj} #{:<5} {time:>8} {score_col} ",
            entry.id
        )
    } else {
        format!(
            "{marker} {kind_badge} {imp_badge:>2} {proj} #{:<5} {time:>8} ",
            entry.id
        )
    };

    // Ensure we don't overrun tiny viewports.
    prefix = truncate_str(&prefix, cfg.width);
    let remaining = cfg.width.saturating_sub(prefix.len());

    let sep_len = RESULTS_SNIPPET_SEP.len();
    let mut include_snippet = !entry.body_preview.is_empty();
    let (title_w, snippet_w) = if include_snippet
        && remaining >= RESULTS_MIN_TITLE_CHARS + sep_len + RESULTS_MIN_SNIPPET_CHARS
    {
        let mut snippet_w = (remaining / 2).min(RESULTS_MAX_SNIPPET_CHARS_IN_LIST);
        // Leave space for the title.
        snippet_w = snippet_w.min(remaining.saturating_sub(RESULTS_MIN_TITLE_CHARS + sep_len));
        let title_w = remaining.saturating_sub(sep_len + snippet_w);
        if title_w < RESULTS_MIN_TITLE_CHARS || snippet_w < RESULTS_MIN_SNIPPET_CHARS {
            include_snippet = false;
            (remaining, 0)
        } else {
            (title_w, snippet_w)
        }
    } else {
        include_snippet = false;
        (remaining, 0)
    };

    let title = truncate_str(&entry.title, title_w);
    let snippet = if include_snippet {
        truncate_str(&entry.body_preview, snippet_w)
    } else {
        String::new()
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let line_meta_style = if is_cursor {
        cfg.cursor_style
    } else {
        cfg.meta_style
    };
    spans.push(Span::styled(prefix, line_meta_style));
    spans.extend(highlight_spans(
        &title,
        cfg.highlight_terms,
        None,
        cfg.highlight_style,
    ));
    if include_snippet && !snippet.is_empty() && remaining > 0 {
        spans.push(Span::styled(RESULTS_SNIPPET_SEP, cfg.meta_style));
        spans.extend(highlight_spans(
            &snippet,
            cfg.highlight_terms,
            Some(cfg.snippet_style),
            cfg.highlight_style,
        ));
    }

    Line::from_spans(spans)
}

fn render_results(
    frame: &mut Frame<'_>,
    area: Rect,
    results: &[ResultEntry],
    cursor: usize,
    highlight_terms: &[QueryTerm],
    sort_direction: SortDirection,
) {
    let block = Block::default()
        .title("Results")
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let visible_h = inner.height as usize;

    if results.is_empty() {
        Paragraph::new("  No results found.").render(inner, frame);
        return;
    }

    let total = results.len();
    let cursor_clamped = cursor.min(total.saturating_sub(1));
    let (start, end) = viewport_range(total, visible_h, cursor_clamped);
    let viewport = &results[start..end];

    let w = inner.width as usize;
    let meta_style = Style::default().fg(FACET_LABEL_FG);
    let cursor_style = Style::default().fg(RESULT_CURSOR_FG);
    let snippet_style = Style::default().fg(FACET_LABEL_FG);
    let highlight_style = Style::default().fg(RESULT_CURSOR_FG).bold();

    let cfg = ResultListRenderCfg {
        width: w,
        highlight_terms,
        sort_direction,
        meta_style,
        cursor_style,
        snippet_style,
        highlight_style,
    };

    let mut lines: Vec<Line> = Vec::with_capacity(viewport.len());

    for (vi, entry) in viewport.iter().enumerate() {
        let abs_idx = start + vi;
        let is_cursor = abs_idx == cursor_clamped;
        lines.push(result_entry_line(entry, is_cursor, &cfg));
    }

    Paragraph::new(Text::from_lines(lines)).render(inner, frame);
}

#[allow(clippy::cast_possible_truncation)]
fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    entry: Option<&ResultEntry>,
    scroll: usize,
    highlight_terms: &[QueryTerm],
) {
    let block = Block::default()
        .title("Detail")
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let Some(entry) = entry else {
        Paragraph::new("  Select a result to view details.").render(inner, frame);
        return;
    };

    // Reserve 1 row for action bar at bottom.
    let action_bar_h: u16 = 1;
    let content_h = inner.height.saturating_sub(action_bar_h);
    let content_area = Rect::new(inner.x, inner.y, inner.width, content_h);
    let action_area = Rect::new(inner.x, inner.y + content_h, inner.width, action_bar_h);

    let label_style = Style::default().fg(FACET_LABEL_FG);
    let highlight_style = Style::default().fg(RESULT_CURSOR_FG).bold();

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(format!("Type:    {:?}", entry.doc_kind)));

    let mut title_spans: Vec<Span<'static>> = Vec::new();
    title_spans.push(Span::styled("Title:   ".to_string(), label_style));
    title_spans.extend(highlight_spans(
        &entry.title,
        highlight_terms,
        None,
        highlight_style,
    ));
    lines.push(Line::from_spans(title_spans));

    lines.push(Line::raw(format!("ID:      #{}", entry.id)));

    if let Some(ref agent) = entry.from_agent {
        lines.push(Line::raw(format!("From:    {agent}")));
    }
    if let Some(ref tid) = entry.thread_id {
        lines.push(Line::raw(format!("Thread:  {tid}")));
    }
    if let Some(ref imp) = entry.importance {
        lines.push(Line::raw(format!("Import.: {imp}")));
    }
    if let Some(ack) = entry.ack_required {
        lines.push(Line::raw(format!(
            "Ack:     {}",
            if ack { "required" } else { "no" }
        )));
    }
    if let Some(ts) = entry.created_ts {
        lines.push(Line::raw(format!("Time:    {}", micros_to_iso(ts))));
    }
    if let Some(pid) = entry.project_id {
        lines.push(Line::raw(format!("Project: #{pid}")));
    }
    if let Some(score) = entry.score {
        lines.push(Line::raw(format!("Score:   {score:.3}")));
    }

    // Markdown preview section (messages with full body) or plain preview fallback.
    lines.push(Line::raw(String::new()));
    if let Some(ref body) = entry.full_body {
        lines.push(Line::styled(
            "\u{2500}\u{2500}\u{2500} Body \u{2500}\u{2500}\u{2500}".to_string(),
            label_style,
        ));
        let theme = tui_markdown::MarkdownTheme::default();
        let rendered = tui_markdown::render_body(body, &theme);
        for line in rendered.lines() {
            lines.push(line.clone());
        }
    } else {
        lines.push(Line::styled(
            "\u{2500}\u{2500}\u{2500} Preview \u{2500}\u{2500}\u{2500}".to_string(),
            label_style,
        ));
        lines.push(Line::from_spans(highlight_spans(
            &entry.body_preview,
            highlight_terms,
            None,
            highlight_style,
        )));
    }

    // Apply scroll and render content.
    let skip = scroll.min(lines.len().saturating_sub(1));
    Paragraph::new(Text::from_lines(lines.into_iter().skip(skip))).render(content_area, frame);

    // Contextual action bar.
    render_action_bar(frame, action_area, entry);
}

/// Render a contextual action bar showing available deep-link keys.
fn render_action_bar(frame: &mut Frame<'_>, area: Rect, entry: &ResultEntry) {
    if area.width < 10 || area.height == 0 {
        return;
    }
    let key_style = Style::default().fg(ACTION_KEY_FG).bold();
    let label_style = Style::default().fg(FACET_LABEL_FG);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Enter always available
    spans.push(Span::styled("Enter".to_string(), key_style));
    spans.push(Span::styled(" Open  ".to_string(), label_style));

    if entry.thread_id.is_some() {
        spans.push(Span::styled("o".to_string(), key_style));
        spans.push(Span::styled(" Thread  ".to_string(), label_style));
    }
    if entry.from_agent.is_some() {
        spans.push(Span::styled("a".to_string(), key_style));
        spans.push(Span::styled(" Agent  ".to_string(), label_style));
    }
    if entry.created_ts.is_some() {
        spans.push(Span::styled("T".to_string(), key_style));
        spans.push(Span::styled(" Timeline  ".to_string(), label_style));
    }
    spans.push(Span::styled("J/K".to_string(), key_style));
    spans.push(Span::styled(" Scroll".to_string(), label_style));

    let line = Line::from_spans(spans);
    Paragraph::new(Text::from_lines(vec![line])).render(area, frame);
}

/// Compute a centered viewport range for scrolling.
fn viewport_range(total: usize, visible: usize, cursor: usize) -> (usize, usize) {
    if total <= visible {
        return (0, total);
    }
    let half = visible / 2;
    let start = if cursor <= half {
        0
    } else if cursor + half >= total {
        total.saturating_sub(visible)
    } else {
        cursor - half
    };
    let end = (start + visible).min(total);
    (start, end)
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_harness::buffer_to_text;

    #[test]
    fn screen_defaults() {
        let screen = SearchCockpitScreen::new();
        assert_eq!(screen.focus, Focus::ResultList);
        assert_eq!(screen.doc_kind_filter, DocKindFilter::Messages);
        assert_eq!(screen.importance_filter, ImportanceFilter::Any);
        assert_eq!(screen.ack_filter, AckFilter::Any);
        assert_eq!(screen.sort_direction, SortDirection::NewestFirst);
        assert!(screen.results.is_empty());
        assert!(screen.search_dirty);
        assert!(screen.thread_filter.is_none());
        assert!(screen.last_error.is_none());
    }

    #[test]
    fn doc_kind_filter_cycles() {
        let mut f = DocKindFilter::Messages;
        f = f.next();
        assert_eq!(f, DocKindFilter::Agents);
        f = f.next();
        assert_eq!(f, DocKindFilter::Projects);
        f = f.next();
        assert_eq!(f, DocKindFilter::All);
        f = f.next();
        assert_eq!(f, DocKindFilter::Messages);
    }

    #[test]
    fn doc_kind_prev_cycles() {
        let mut f = DocKindFilter::Messages;
        f = f.prev();
        assert_eq!(f, DocKindFilter::All);
        f = f.prev();
        assert_eq!(f, DocKindFilter::Projects);
    }

    #[test]
    fn importance_filter_cycles() {
        let mut f = ImportanceFilter::Any;
        f = f.next();
        assert_eq!(f, ImportanceFilter::Urgent);
        f = f.next();
        assert_eq!(f, ImportanceFilter::High);
        f = f.next();
        assert_eq!(f, ImportanceFilter::Normal);
        f = f.next();
        assert_eq!(f, ImportanceFilter::Any);
    }

    #[test]
    fn ack_filter_cycles() {
        let mut f = AckFilter::Any;
        f = f.next();
        assert_eq!(f, AckFilter::Required);
        f = f.next();
        assert_eq!(f, AckFilter::NotRequired);
        f = f.next();
        assert_eq!(f, AckFilter::Any);
    }

    #[test]
    fn sort_direction_cycles() {
        let mut d = SortDirection::NewestFirst;
        d = d.next();
        assert_eq!(d, SortDirection::OldestFirst);
        d = d.next();
        assert_eq!(d, SortDirection::Relevance);
        d = d.next();
        assert_eq!(d, SortDirection::NewestFirst);
    }

    #[test]
    fn facet_slot_cycles() {
        let mut s = FacetSlot::Scope;
        s = s.next();
        assert_eq!(s, FacetSlot::DocKind);
        s = s.next();
        assert_eq!(s, FacetSlot::Importance);
        s = s.next();
        assert_eq!(s, FacetSlot::AckStatus);
        s = s.next();
        assert_eq!(s, FacetSlot::SortOrder);
        s = s.next();
        assert_eq!(s, FacetSlot::Scope);
    }

    #[test]
    fn facet_slot_prev_cycles() {
        let mut s = FacetSlot::DocKind;
        s = s.prev();
        assert_eq!(s, FacetSlot::Scope);
        s = s.prev();
        assert_eq!(s, FacetSlot::SortOrder);
        s = s.prev();
        assert_eq!(s, FacetSlot::AckStatus);
    }

    #[test]
    fn viewport_range_small() {
        assert_eq!(viewport_range(5, 10, 0), (0, 5));
        assert_eq!(viewport_range(5, 10, 4), (0, 5));
    }

    #[test]
    fn viewport_range_centered() {
        let (start, end) = viewport_range(100, 20, 50);
        assert!(start <= 50);
        assert!(end > 50);
        assert_eq!(end - start, 20);
    }

    #[test]
    fn viewport_range_at_end() {
        let (start, end) = viewport_range(100, 20, 99);
        assert_eq!(end, 100);
        assert_eq!(start, 80);
    }

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_long() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result.chars().count(), 5); // 4 chars + 1 ellipsis char
        assert!(result.ends_with('\u{2026}'));
    }

    #[test]
    fn validate_query_syntax_rejects_unbalanced_quotes() {
        let err = validate_query_syntax("\"oops");
        assert!(err.is_some());
        assert!(err.unwrap().contains("Unbalanced quotes"));
    }

    #[test]
    fn validate_query_syntax_rejects_bare_boolean() {
        assert!(validate_query_syntax("AND").is_some());
        assert!(validate_query_syntax("or").is_some());
        assert!(validate_query_syntax("Not").is_some());
    }

    #[test]
    fn route_string_is_deterministic_and_encoded() {
        let mut screen = SearchCockpitScreen::new();
        screen.query_input.set_value("hello world");
        screen.doc_kind_filter = DocKindFilter::All;
        screen.importance_filter = ImportanceFilter::Urgent;
        screen.ack_filter = AckFilter::Required;
        screen.sort_direction = SortDirection::Relevance;
        screen.thread_filter = Some("t-1".to_string());
        assert_eq!(
            screen.route_string(),
            "/search?q=hello%20world&type=all&imp=urgent&ack=1&sort=relevance&thread=t-1"
        );
    }

    #[test]
    fn query_bar_renders_error_hint_line() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = SearchCockpitScreen::new();
        screen.query_input.set_value("\"oops");
        screen.last_error = validate_query_syntax(screen.query_input.value());

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 80, 10), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(text.contains("ERR:"), "expected ERR line, got:\n{text}");
        assert!(
            text.contains("Unbalanced quotes"),
            "expected validation error, got:\n{text}"
        );
    }

    #[test]
    fn reset_facets_clears_all() {
        let mut screen = SearchCockpitScreen::new();
        screen.doc_kind_filter = DocKindFilter::Agents;
        screen.importance_filter = ImportanceFilter::Urgent;
        screen.ack_filter = AckFilter::Required;
        screen.sort_direction = SortDirection::Relevance;
        screen.thread_filter = Some("t1".to_string());
        screen.search_dirty = false;

        screen.reset_facets();

        assert_eq!(screen.doc_kind_filter, DocKindFilter::Messages);
        assert_eq!(screen.importance_filter, ImportanceFilter::Any);
        assert_eq!(screen.ack_filter, AckFilter::Any);
        assert_eq!(screen.sort_direction, SortDirection::NewestFirst);
        assert!(screen.thread_filter.is_none());
        assert!(screen.search_dirty);
    }

    #[test]
    fn toggle_active_facet_doc_kind() {
        let mut screen = SearchCockpitScreen::new();
        screen.active_facet = FacetSlot::DocKind;
        screen.search_dirty = false;
        screen.toggle_active_facet();
        assert_eq!(screen.doc_kind_filter, DocKindFilter::Agents);
        assert!(screen.search_dirty);
    }

    #[test]
    fn toggle_active_facet_importance() {
        let mut screen = SearchCockpitScreen::new();
        screen.active_facet = FacetSlot::Importance;
        screen.toggle_active_facet();
        assert_eq!(screen.importance_filter, ImportanceFilter::Urgent);
    }

    #[test]
    fn screen_consumes_text_when_query_focused() {
        let mut screen = SearchCockpitScreen::new();
        assert!(!screen.consumes_text_input());
        screen.focus = Focus::QueryBar;
        assert!(screen.consumes_text_input());
        screen.focus = Focus::FacetRail;
        assert!(!screen.consumes_text_input());
    }

    #[test]
    fn screen_title_and_label() {
        let screen = SearchCockpitScreen::new();
        assert_eq!(screen.title(), "Search");
        assert_eq!(screen.tab_label(), "Find");
    }

    #[test]
    fn deep_link_thread_sets_filter() {
        let mut screen = SearchCockpitScreen::new();
        let handled = screen.receive_deep_link(&DeepLinkTarget::ThreadById("t-123".to_string()));
        assert!(handled);
        assert_eq!(screen.thread_filter.as_deref(), Some("t-123"));
        assert!(screen.search_dirty);
    }

    #[test]
    fn deep_link_other_ignored() {
        let mut screen = SearchCockpitScreen::new();
        assert!(!screen.receive_deep_link(&DeepLinkTarget::MessageById(1)));
    }

    #[test]
    fn build_query_includes_facets() {
        let mut screen = SearchCockpitScreen::new();
        screen.importance_filter = ImportanceFilter::High;
        screen.ack_filter = AckFilter::Required;
        screen.thread_filter = Some("t-1".to_string());
        let q = screen.build_query();
        assert_eq!(q.importance, vec![Importance::High]);
        assert_eq!(q.ack_required, Some(true));
        assert_eq!(q.thread_id.as_deref(), Some("t-1"));
    }

    #[test]
    fn screen_renders_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = SearchCockpitScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 40), &state);
    }

    #[test]
    fn screen_renders_narrow_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = SearchCockpitScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(50, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 50, 20), &state);
    }

    #[test]
    fn screen_renders_tiny_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = SearchCockpitScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(10, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 3), &state);
    }

    #[test]
    fn keybindings_nonempty() {
        let screen = SearchCockpitScreen::new();
        assert!(!screen.keybindings().is_empty());
    }

    #[test]
    fn importance_filter_string() {
        assert!(ImportanceFilter::Any.filter_string().is_none());
        assert_eq!(
            ImportanceFilter::Urgent.filter_string().as_deref(),
            Some("urgent")
        );
        assert_eq!(
            ImportanceFilter::High.filter_string().as_deref(),
            Some("high")
        );
    }

    #[test]
    fn ack_filter_value() {
        assert!(AckFilter::Any.filter_value().is_none());
        assert_eq!(AckFilter::Required.filter_value(), Some(true));
        assert_eq!(AckFilter::NotRequired.filter_value(), Some(false));
    }

    #[test]
    fn scope_mode_cycles_through_facet_toggle() {
        let mut screen = SearchCockpitScreen::new();
        screen.active_facet = FacetSlot::Scope;
        assert_eq!(screen.scope_mode, ScopeMode::Global);

        screen.toggle_active_facet();
        assert_eq!(screen.scope_mode, ScopeMode::Project);
        assert!(screen.search_dirty);

        screen.toggle_active_facet();
        assert_eq!(screen.scope_mode, ScopeMode::Product);

        screen.toggle_active_facet();
        assert_eq!(screen.scope_mode, ScopeMode::Global);
    }

    #[test]
    fn facet_slot_scope_cycles() {
        let mut s = FacetSlot::Scope;
        s = s.next();
        assert_eq!(s, FacetSlot::DocKind);
        s = s.prev();
        assert_eq!(s, FacetSlot::Scope);
        s = s.prev();
        assert_eq!(s, FacetSlot::SortOrder);
    }

    #[test]
    fn reset_facets_clears_scope() {
        let mut screen = SearchCockpitScreen::new();
        screen.scope_mode = ScopeMode::Product;
        screen.reset_facets();
        assert_eq!(screen.scope_mode, ScopeMode::Global);
    }

    #[test]
    fn route_string_includes_scope() {
        let mut screen = SearchCockpitScreen::new();
        screen.query_input.set_value("test");
        screen.scope_mode = ScopeMode::Project;
        let route = screen.route_string();
        assert!(route.contains("scope=project"), "route was: {route}");
    }

    #[test]
    fn route_string_omits_default_scope() {
        let mut screen = SearchCockpitScreen::new();
        screen.query_input.set_value("test");
        screen.scope_mode = ScopeMode::Global;
        let route = screen.route_string();
        assert!(!route.contains("scope="), "route was: {route}");
    }

    #[test]
    fn history_cursor_resets_on_enter() {
        let mut screen = SearchCockpitScreen::new();
        screen.history_cursor = Some(3);
        screen.focus = Focus::QueryBar;

        let enter = Event::Key(ftui::KeyEvent {
            code: KeyCode::Enter,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&enter, &state);

        assert!(screen.history_cursor.is_none());
        assert_eq!(screen.focus, Focus::ResultList);
    }

    #[test]
    fn history_cursor_resets_on_escape() {
        let mut screen = SearchCockpitScreen::new();
        screen.history_cursor = Some(1);
        screen.focus = Focus::QueryBar;

        let esc = Event::Key(ftui::KeyEvent {
            code: KeyCode::Escape,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&esc, &state);

        assert!(screen.history_cursor.is_none());
    }

    #[test]
    fn history_up_recalls_entry() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::QueryBar;
        screen.query_input.set_focused(true);
        screen.query_history = vec![
            QueryHistoryEntry {
                query_text: "first".to_string(),
                ..Default::default()
            },
            QueryHistoryEntry {
                query_text: "second".to_string(),
                ..Default::default()
            },
        ];

        let up = Event::Key(ftui::KeyEvent {
            code: KeyCode::Up,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&up, &state);

        assert_eq!(screen.history_cursor, Some(0));
        assert_eq!(screen.query_input.value(), "first");
    }

    #[test]
    fn history_down_clears_at_bottom() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::QueryBar;
        screen.query_input.set_focused(true);
        screen.history_cursor = Some(0);
        screen.query_history = vec![QueryHistoryEntry {
            query_text: "old query".to_string(),
            ..Default::default()
        }];
        screen.query_input.set_value("old query");

        let down = Event::Key(ftui::KeyEvent {
            code: KeyCode::Down,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&down, &state);

        assert!(screen.history_cursor.is_none());
        assert_eq!(screen.query_input.value(), "");
    }

    #[test]
    fn screen_defaults_include_scope() {
        let screen = SearchCockpitScreen::new();
        assert_eq!(screen.scope_mode, ScopeMode::Global);
        assert!(screen.saved_recipes.is_empty());
        assert!(screen.query_history.is_empty());
        assert!(screen.history_cursor.is_none());
        assert!(!screen.recipes_loaded);
    }

    #[test]
    fn extract_snippet_centers_on_match_and_adds_ellipses() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu needle nu xi omicron pi rho sigma tau upsilon phi chi psi omega";
        let terms = vec![QueryTerm {
            text: "needle".to_string(),
            kind: QueryTermKind::Word,
            negated: false,
        }];
        let snippet = extract_snippet(text, &terms, 40);
        assert!(snippet.contains("needle"));
        assert!(snippet.starts_with('\u{2026}'));
        assert!(snippet.ends_with('\u{2026}'));
    }

    #[test]
    fn highlight_spans_preserves_text_and_styles_matches() {
        let terms = vec![QueryTerm {
            text: "needle".to_string(),
            kind: QueryTermKind::Word,
            negated: false,
        }];
        let base = Style::default().fg(FACET_LABEL_FG);
        let highlight = Style::default().fg(RESULT_CURSOR_FG).bold();
        let spans = highlight_spans("xxNEEDLEyy", &terms, Some(base), highlight);

        let plain: String = spans.iter().map(Span::as_str).collect();
        assert_eq!(plain, "xxNEEDLEyy");
        assert!(
            spans
                .iter()
                .any(|s| s.as_str() == "NEEDLE" && s.style == Some(highlight))
        );
        assert!(
            spans
                .iter()
                .any(|s| s.as_str() == "xx" && s.style == Some(base))
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // br-3vwi.4.3: Markdown preview + contextual actions + deep-links
    // ──────────────────────────────────────────────────────────────────

    fn make_msg_entry() -> ResultEntry {
        ResultEntry {
            id: 42,
            doc_kind: DocKind::Message,
            title: "Test subject".to_string(),
            body_preview: "short preview".to_string(),
            full_body: Some("# Hello\n\nThis is **bold** markdown.".to_string()),
            score: Some(0.95),
            importance: Some("normal".to_string()),
            ack_required: Some(false),
            created_ts: Some(1_700_000_000_000_000),
            thread_id: Some("test-thread".to_string()),
            from_agent: Some("GoldFox".to_string()),
            project_id: Some(1),
        }
    }

    fn make_agent_entry() -> ResultEntry {
        ResultEntry {
            id: 10,
            doc_kind: DocKind::Agent,
            title: "GoldFox".to_string(),
            body_preview: "agent task description".to_string(),
            full_body: None,
            score: None,
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            project_id: Some(1),
        }
    }

    #[test]
    fn result_entry_full_body_populated_for_messages() {
        let entry = make_msg_entry();
        assert!(entry.full_body.is_some());
        assert!(entry.full_body.as_ref().unwrap().contains("**bold**"));
    }

    #[test]
    fn result_entry_full_body_none_for_agents() {
        let entry = make_agent_entry();
        assert!(entry.full_body.is_none());
    }

    #[test]
    fn render_detail_with_markdown_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 30, &mut pool);
        let entry = make_msg_entry();
        render_detail(&mut frame, Rect::new(0, 0, 80, 30), Some(&entry), 0, &[]);
        let text = buffer_to_text(&frame.buffer);
        // Should contain the body header
        assert!(
            text.contains("Body"),
            "detail should show Body header, got:\n{text}"
        );
        // Should contain action bar keys
        assert!(
            text.contains("Enter"),
            "detail should show Enter action, got:\n{text}"
        );
        assert!(
            text.contains("Thread"),
            "detail should show Thread action, got:\n{text}"
        );
    }

    #[test]
    fn render_detail_plain_preview_when_no_full_body() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 20, &mut pool);
        let entry = make_agent_entry();
        render_detail(&mut frame, Rect::new(0, 0, 80, 20), Some(&entry), 0, &[]);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Preview"),
            "agent detail should show Preview header, got:\n{text}"
        );
    }

    #[test]
    fn render_detail_no_entry_shows_prompt() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(60, 10, &mut pool);
        render_detail(&mut frame, Rect::new(0, 0, 60, 10), None, 0, &[]);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Select a result"),
            "should show selection prompt, got:\n{text}"
        );
    }

    #[test]
    fn action_bar_shows_thread_for_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 2, &mut pool);
        let entry = make_msg_entry();
        render_action_bar(&mut frame, Rect::new(0, 0, 80, 1), &entry);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Thread"),
            "message action bar should show Thread, got:\n{text}"
        );
        assert!(
            text.contains("Agent"),
            "message action bar should show Agent, got:\n{text}"
        );
        assert!(
            text.contains("Timeline"),
            "message action bar should show Timeline, got:\n{text}"
        );
    }

    #[test]
    fn action_bar_hides_thread_for_agent() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 2, &mut pool);
        let entry = make_agent_entry();
        render_action_bar(&mut frame, Rect::new(0, 0, 80, 1), &entry);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            !text.contains("Thread"),
            "agent action bar should not show Thread, got:\n{text}"
        );
        assert!(
            !text.contains("Agent"),
            "agent action bar should not show Agent, got:\n{text}"
        );
    }

    #[test]
    fn o_key_emits_thread_deep_link() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::ResultList;
        screen.results = vec![make_msg_entry()];
        screen.cursor = 0;

        let o = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('o'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let cmd = screen.update(&o, &state);

        // Should emit a DeepLink command (non-None)
        assert!(
            !matches!(cmd, Cmd::None),
            "o key should emit deep link for thread"
        );
    }

    #[test]
    fn a_key_emits_agent_deep_link() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::ResultList;
        screen.results = vec![make_msg_entry()];
        screen.cursor = 0;

        let a = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('a'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let cmd = screen.update(&a, &state);

        assert!(
            !matches!(cmd, Cmd::None),
            "a key should emit deep link for agent"
        );
    }

    #[test]
    fn shift_t_key_emits_timeline_deep_link() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::ResultList;
        screen.results = vec![make_msg_entry()];
        screen.cursor = 0;

        let t = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('T'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let cmd = screen.update(&t, &state);

        assert!(
            !matches!(cmd, Cmd::None),
            "T key should emit deep link for timeline"
        );
    }

    #[test]
    fn o_key_noop_when_no_thread_id() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::ResultList;
        screen.results = vec![make_agent_entry()];
        screen.cursor = 0;

        let o = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('o'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let cmd = screen.update(&o, &state);

        assert!(
            matches!(cmd, Cmd::None),
            "o key should be noop for agent (no thread_id)"
        );
    }

    #[test]
    fn keybindings_include_contextual_actions() {
        let screen = SearchCockpitScreen::new();
        let bindings = screen.keybindings();
        let actions: Vec<&str> = bindings.iter().map(|h| h.action).collect();
        assert!(
            actions.contains(&"Open thread"),
            "keybindings should include 'Open thread'"
        );
        assert!(
            actions.contains(&"Jump to agent"),
            "keybindings should include 'Jump to agent'"
        );
        assert!(
            actions.contains(&"Timeline at time"),
            "keybindings should include 'Timeline at time'"
        );
    }
}
