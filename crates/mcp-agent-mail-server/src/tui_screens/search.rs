//! Search Cockpit screen with query bar, facet rail, and results.
//!
//! Provides a unified search interface across messages, agents, and projects
//! using the global search planner and search service.  Facet toggles allow
//! composable filtering by document kind, importance, ack status, and more.

use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers, PackedRgba, Style};
use ftui_runtime::program::Cmd;
use ftui_widgets::input::TextInput;

use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::search_planner::DocKind;
#[cfg(test)]
use mcp_agent_mail_db::search_planner::{Importance, SearchQuery};
use mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection;
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max results to display.
const MAX_RESULTS: usize = 200;

/// Debounce delay in ticks (~100ms each, so 3 ticks = ~300ms).
const DEBOUNCE_TICKS: u8 = 3;

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

    #[cfg(test)]
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
    score: Option<f64>,
    importance: Option<String>,
    ack_required: Option<bool>,
    created_ts: Option<i64>,
    thread_id: Option<String>,
    from_agent: Option<String>,
    project_id: Option<i64>,
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
    DocKind,
    Importance,
    AckStatus,
    SortOrder,
}

impl FacetSlot {
    const fn next(self) -> Self {
        match self {
            Self::DocKind => Self::Importance,
            Self::Importance => Self::AckStatus,
            Self::AckStatus => Self::SortOrder,
            Self::SortOrder => Self::DocKind,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::DocKind => Self::SortOrder,
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
    doc_kind_filter: DocKindFilter,
    importance_filter: ImportanceFilter,
    ack_filter: AckFilter,
    sort_direction: SortDirection,
    thread_filter: Option<String>,

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
    debounce_remaining: u8,
    search_dirty: bool,
}

impl SearchCockpitScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            query_input: TextInput::new()
                .with_placeholder("Search across messages, agents, projects... (/ to focus)")
                .with_focused(false),
            doc_kind_filter: DocKindFilter::Messages,
            importance_filter: ImportanceFilter::Any,
            ack_filter: AckFilter::Any,
            sort_direction: SortDirection::NewestFirst,
            thread_filter: None,
            results: Vec::new(),
            cursor: 0,
            detail_scroll: 0,
            total_sql_rows: 0,
            focus: Focus::ResultList,
            active_facet: FacetSlot::DocKind,
            db_conn: None,
            db_conn_attempted: false,
            last_query: String::new(),
            debounce_remaining: 0,
            search_dirty: true,
        }
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
        self.ensure_db_conn(state);
        let Some(conn) = &self.db_conn else {
            return;
        };

        let raw = self.query_input.value().trim().to_string();
        self.last_query.clone_from(&raw);

        if self.doc_kind_filter == DocKindFilter::All {
            // Run all three kinds and merge
            let mut all_results = Vec::new();
            for kind in &[DocKind::Message, DocKind::Agent, DocKind::Project] {
                let results = self.run_kind_search(conn, *kind, &raw);
                all_results.extend(results);
            }
            // Sort by score descending
            all_results.sort_by(|a, b| {
                b.score
                    .unwrap_or(0.0)
                    .partial_cmp(&a.score.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            all_results.truncate(MAX_RESULTS);
            self.total_sql_rows = all_results.len();
            self.results = all_results;
        } else {
            let kind = self.doc_kind_filter.doc_kind().unwrap_or(DocKind::Message);
            let results = self.run_kind_search(conn, kind, &raw);
            self.total_sql_rows = results.len();
            self.results = results;
        }

        // Clamp cursor
        if self.results.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.results.len() - 1);
        }
        self.detail_scroll = 0;
        self.search_dirty = false;
    }

    /// Run a search for a single doc kind using sync queries.
    fn run_kind_search(
        &self,
        conn: &SqliteConnection,
        kind: DocKind,
        raw: &str,
    ) -> Vec<ResultEntry> {
        match kind {
            DocKind::Message => self.search_messages(conn, raw),
            DocKind::Agent => self.search_agents(conn, raw),
            DocKind::Project => self.search_projects(conn, raw),
        }
    }

    /// Search messages via FTS5 with LIKE fallback.
    fn search_messages(&self, conn: &SqliteConnection, raw: &str) -> Vec<ResultEntry> {
        let sanitized = sanitize_fts_query(raw);

        // Build WHERE conditions for facets
        let mut conditions = Vec::new();

        if let Some(ref imp) = self.importance_filter.filter_string() {
            let escaped = imp.replace('\'', "''");
            conditions.push(format!("m.importance = '{escaped}'"));
        }
        if let Some(ack) = self.ack_filter.filter_value() {
            conditions.push(format!("m.ack_required = {}", i32::from(ack)));
        }
        if let Some(ref tid) = self.thread_filter {
            let escaped = tid.replace('\'', "''");
            conditions.push(format!("m.thread_id = '{escaped}'"));
        }

        let extra_where = if conditions.is_empty() {
            String::new()
        } else {
            format!(" AND {}", conditions.join(" AND "))
        };
        let order = match self.sort_direction {
            SortDirection::NewestFirst => "m.created_ts DESC",
            SortDirection::OldestFirst => "m.created_ts ASC",
            SortDirection::Relevance => "rank",
        };

        // Try FTS first
        if !sanitized.is_empty() {
            let sql = format!(
                "SELECT m.id, m.subject, m.body_md, m.thread_id, m.importance, \
                 m.ack_required, m.created_ts, \
                 a.name AS from_name, m.project_id \
                 FROM fts_messages fts \
                 JOIN messages m ON m.id = fts.message_id \
                 LEFT JOIN agents a ON a.id = m.sender_id \
                 WHERE fts_messages MATCH '{sanitized}'{extra_where} \
                 GROUP BY m.id \
                 ORDER BY {order} \
                 LIMIT {MAX_RESULTS}"
            );

            let results = query_message_rows(conn, &sql);
            if !results.is_empty() {
                return results;
            }
        }

        // LIKE fallback or empty query (recent messages)
        let order_clause = match self.sort_direction {
            SortDirection::OldestFirst => "m.created_ts ASC",
            SortDirection::NewestFirst | SortDirection::Relevance => "m.created_ts DESC",
        };

        if raw.is_empty() {
            let sql = format!(
                "SELECT m.id, m.subject, m.body_md, m.thread_id, m.importance, \
                 m.ack_required, m.created_ts, \
                 a.name AS from_name, m.project_id \
                 FROM messages m \
                 LEFT JOIN agents a ON a.id = m.sender_id \
                 WHERE 1=1{extra_where} \
                 ORDER BY {order_clause} \
                 LIMIT {MAX_RESULTS}"
            );
            return query_message_rows(conn, &sql);
        }

        let escaped = raw.replace('\'', "''");
        let sql = format!(
            "SELECT m.id, m.subject, m.body_md, m.thread_id, m.importance, \
             m.ack_required, m.created_ts, \
             a.name AS from_name, m.project_id \
             FROM messages m \
             LEFT JOIN agents a ON a.id = m.sender_id \
             WHERE (m.subject LIKE '%{escaped}%' OR m.body_md LIKE '%{escaped}%'){extra_where} \
             ORDER BY {order_clause} \
             LIMIT {MAX_RESULTS}"
        );
        query_message_rows(conn, &sql)
    }

    /// Search agents.
    fn search_agents(&self, conn: &SqliteConnection, raw: &str) -> Vec<ResultEntry> {
        let _ = self; // future: may use self for per-project scoping
        if raw.is_empty() {
            let sql = "SELECT id, name, task_description, project_id \
                       FROM agents ORDER BY name LIMIT 100";
            return query_agent_rows(conn, sql);
        }
        let escaped = raw.replace('\'', "''");
        let sql = format!(
            "SELECT id, name, task_description, project_id \
             FROM agents \
             WHERE name LIKE '%{escaped}%' OR task_description LIKE '%{escaped}%' \
             ORDER BY name \
             LIMIT {MAX_RESULTS}"
        );
        query_agent_rows(conn, &sql)
    }

    /// Search projects.
    fn search_projects(&self, conn: &SqliteConnection, raw: &str) -> Vec<ResultEntry> {
        let _ = self; // future: may use self for filtering
        if raw.is_empty() {
            let sql = "SELECT id, slug, human_key FROM projects ORDER BY slug LIMIT 100";
            return query_project_rows(conn, sql);
        }
        let escaped = raw.replace('\'', "''");
        let sql = format!(
            "SELECT id, slug, human_key \
             FROM projects \
             WHERE slug LIKE '%{escaped}%' OR human_key LIKE '%{escaped}%' \
             ORDER BY slug \
             LIMIT {MAX_RESULTS}"
        );
        query_project_rows(conn, &sql)
    }

    /// Toggle the active facet's value.
    #[allow(clippy::missing_const_for_fn)] // mutates self through .next() chains
    fn toggle_active_facet(&mut self) {
        match self.active_facet {
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
        self.doc_kind_filter = DocKindFilter::Messages;
        self.importance_filter = ImportanceFilter::Any;
        self.ack_filter = AckFilter::Any;
        self.sort_direction = SortDirection::NewestFirst;
        self.thread_filter = None;
        self.search_dirty = true;
        self.debounce_remaining = 0;
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
                        }
                        KeyCode::Escape => {
                            self.focus = Focus::ResultList;
                            self.query_input.set_focused(false);
                        }
                        KeyCode::Tab => {
                            self.focus = Focus::FacetRail;
                            self.query_input.set_focused(false);
                        }
                        _ => {
                            let before = self.query_input.value().to_string();
                            self.query_input.handle_event(event);
                            if self.query_input.value() != before {
                                self.search_dirty = true;
                                self.debounce_remaining = DEBOUNCE_TICKS;
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
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 4 || area.width < 30 {
            return;
        }

        // Layout: query bar (3h) + body
        let query_h: u16 = 3;
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
            render_results(frame, results_area, &self.results, self.cursor);
            render_detail(
                frame,
                detail_area,
                self.results.get(self.cursor),
                self.detail_scroll,
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
            render_results(frame, results_area, &self.results, self.cursor);
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
                key: "Ctrl+C",
                action: "Clear all",
            },
            HelpEntry {
                key: "r",
                action: "Reset facets",
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

fn query_message_rows(conn: &SqliteConnection, sql: &str) -> Vec<ResultEntry> {
    conn.query_sync(sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let id: i64 = row.get_named("id").ok()?;
                    let subject: String = row.get_named("subject").unwrap_or_default();
                    let body: String = row.get_named("body_md").unwrap_or_default();
                    let preview = truncate_str(&body, 120);
                    Some(ResultEntry {
                        id,
                        doc_kind: DocKind::Message,
                        title: subject,
                        body_preview: preview,
                        score: None,
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
        .unwrap_or_default()
}

fn query_agent_rows(conn: &SqliteConnection, sql: &str) -> Vec<ResultEntry> {
    conn.query_sync(sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let id: i64 = row.get_named("id").ok()?;
                    let name: String = row.get_named("name").unwrap_or_default();
                    let desc: String = row.get_named("task_description").unwrap_or_default();
                    Some(ResultEntry {
                        id,
                        doc_kind: DocKind::Agent,
                        title: name,
                        body_preview: truncate_str(&desc, 120),
                        score: None,
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

fn query_project_rows(conn: &SqliteConnection, sql: &str) -> Vec<ResultEntry> {
    conn.query_sync(sql, &[])
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
                        score: None,
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

/// Sanitize FTS5 query to prevent syntax errors.
fn sanitize_fts_query(query: &str) -> String {
    let mut tokens = Vec::new();
    for word in query.split_whitespace() {
        let w = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
        if w.is_empty()
            || w.eq_ignore_ascii_case("AND")
            || w.eq_ignore_ascii_case("OR")
            || w.eq_ignore_ascii_case("NOT")
            || w.eq_ignore_ascii_case("NEAR")
        {
            continue;
        }
        let escaped = w.replace('"', "");
        tokens.push(format!("\"{escaped}\""));
    }
    tokens.join(" ")
}

/// Truncate a string to `max_chars`, adding ellipsis if needed.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let mut t = s[..max_chars.saturating_sub(1)].to_string();
        t.push('\u{2026}');
        t
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rendering helpers
// ──────────────────────────────────────────────────────────────────────

const FACET_ACTIVE_FG: PackedRgba = PackedRgba::rgba(0x5F, 0xAF, 0xFF, 0xFF); // Blue
const FACET_LABEL_FG: PackedRgba = PackedRgba::rgba(0x87, 0x87, 0x87, 0xFF); // Grey
const RESULT_CURSOR_FG: PackedRgba = PackedRgba::rgba(0xFF, 0xD7, 0x00, 0xFF); // Yellow

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

    if inner.height > 0 && inner.width > 0 {
        input.render(inner, frame);
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
        let y = inner.y + 8;
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
    if help_y > inner.y + 9 {
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

fn render_results(frame: &mut Frame<'_>, area: Rect, results: &[ResultEntry], cursor: usize) {
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
    let mut lines = Vec::with_capacity(viewport.len());

    for (vi, entry) in viewport.iter().enumerate() {
        let abs_idx = start + vi;
        let marker = if abs_idx == cursor_clamped { '>' } else { ' ' };

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

        let time = entry
            .created_ts
            .map(|ts| {
                let iso = micros_to_iso(ts);
                if iso.len() >= 19 {
                    iso[11..19].to_string()
                } else {
                    iso
                }
            })
            .unwrap_or_default();

        let prefix = format!(
            "{marker} {kind_badge} {imp_badge:>2} #{:<5} {time:>8} ",
            entry.id
        );
        let remaining = w.saturating_sub(prefix.len());
        let title = truncate_str(&entry.title, remaining);
        lines.push(format!("{prefix}{title}"));
    }

    let text = lines.join("\n");
    Paragraph::new(text).render(inner, frame);
}

#[allow(clippy::cast_possible_truncation)]
fn render_detail(frame: &mut Frame<'_>, area: Rect, entry: Option<&ResultEntry>, scroll: usize) {
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

    let mut lines = Vec::new();
    lines.push(format!("Type:    {:?}", entry.doc_kind));
    lines.push(format!("Title:   {}", entry.title));
    lines.push(format!("ID:      #{}", entry.id));

    if let Some(ref agent) = entry.from_agent {
        lines.push(format!("From:    {agent}"));
    }
    if let Some(ref tid) = entry.thread_id {
        lines.push(format!("Thread:  {tid}"));
    }
    if let Some(ref imp) = entry.importance {
        lines.push(format!("Import.: {imp}"));
    }
    if let Some(ack) = entry.ack_required {
        lines.push(format!("Ack:     {}", if ack { "required" } else { "no" }));
    }
    if let Some(ts) = entry.created_ts {
        lines.push(format!("Time:    {}", micros_to_iso(ts)));
    }
    if let Some(pid) = entry.project_id {
        lines.push(format!("Project: #{pid}"));
    }
    if let Some(score) = entry.score {
        lines.push(format!("Score:   {score:.3}"));
    }

    lines.push(String::new());
    lines.push("--- Preview ---".to_string());
    lines.push(entry.body_preview.clone());

    // Apply scroll
    let skip = scroll.min(lines.len().saturating_sub(1));
    let visible = &lines[skip..];
    let text = visible.join("\n");
    Paragraph::new(text).render(inner, frame);
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
        let mut s = FacetSlot::DocKind;
        s = s.next();
        assert_eq!(s, FacetSlot::Importance);
        s = s.next();
        assert_eq!(s, FacetSlot::AckStatus);
        s = s.next();
        assert_eq!(s, FacetSlot::SortOrder);
        s = s.next();
        assert_eq!(s, FacetSlot::DocKind);
    }

    #[test]
    fn facet_slot_prev_cycles() {
        let mut s = FacetSlot::DocKind;
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
    fn sanitize_fts_empty() {
        assert_eq!(sanitize_fts_query(""), "");
        assert_eq!(sanitize_fts_query("   "), "");
    }

    #[test]
    fn sanitize_fts_strips_operators() {
        assert_eq!(sanitize_fts_query("hello AND world"), "\"hello\" \"world\"");
        assert_eq!(sanitize_fts_query("NOT test"), "\"test\"");
    }

    #[test]
    fn sanitize_fts_quotes_tokens() {
        assert_eq!(sanitize_fts_query("hello world"), "\"hello\" \"world\"");
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
}
