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
use ftui::{
    Event, Frame, KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind, PackedRgba, Style,
};
use ftui_extras::charts::Sparkline;
use ftui_runtime::program::Cmd;
use ftui_widgets::StatefulWidget;
use ftui_widgets::input::TextInput;
use ftui_widgets::virtualized::{RenderItem, VirtualizedList, VirtualizedListState};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::search_planner::{
    DocKind, Importance, RankingMode, RecoverySuggestion, SearchQuery, ZeroResultGuidance,
    plan_search,
};
use mcp_agent_mail_db::search_recipes::{
    MAX_RECIPES, QueryHistoryEntry, ScopeMode, SearchRecipe, insert_history, insert_recipe,
    list_recent_history, list_recipes, touch_recipe,
};
use mcp_agent_mail_db::sqlmodel::Value;
use mcp_agent_mail_db::timestamps::{micros_to_iso, now_micros};
use mcp_agent_mail_db::{DbConn, QueryAssistance, parse_query_assistance};

use crate::tui_bridge::TuiSharedState;
use crate::tui_layout::{DockLayout, DockPosition};
use crate::tui_markdown;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max results to display.
const MAX_RESULTS: usize = 200;

/// Debounce delay in ticks for search-as-you-type.
const DEBOUNCE_TICKS: u8 = 1;
const SEARCH_DOCK_HIDE_HEIGHT_THRESHOLD: u16 = 8;
const SEARCH_STACKED_WIDTH_THRESHOLD: u16 = 60;
const SEARCH_STACKED_MIN_HEIGHT: u16 = 12;
const SEARCH_STACKED_DOCK_RATIO: f32 = 0.38;
const SEARCH_FACET_GAP_THRESHOLD: u16 = 56;
const SEARCH_SPLIT_GAP_THRESHOLD: u16 = 60;

/// Max chars for the message snippet shown in the detail pane.
const MAX_SNIPPET_CHARS: usize = 320;
/// Max markdown body lines to inspect when building searchable previews.
const SEARCHABLE_BODY_MAX_LINES: usize = 72;
/// Max markdown body bytes to inspect when building searchable previews.
const SEARCHABLE_BODY_MAX_CHARS: usize = 9_000;
/// Lower caps used when no highlight terms are active (keeps search snappy).
const SEARCHABLE_PREVIEW_MAX_LINES: usize = 24;
const SEARCHABLE_PREVIEW_MAX_CHARS: usize = 2_800;
const CONTEXT_NO_HIT_LINES: usize = 3;
const CONTEXT_HIT_RADIUS_LINES: usize = 2;

/// Hard cap on highlight terms to keep rendering predictable.
const MAX_HIGHLIGHT_TERMS: usize = 8;

/// Minimum title width required before we show a snippet in the results list.
#[allow(dead_code)]
const RESULTS_MIN_TITLE_CHARS: usize = 18;
/// Minimum snippet width required before we show it in the results list.
#[allow(dead_code)]
const RESULTS_MIN_SNIPPET_CHARS: usize = 18;
/// Max chars allocated to the snippet column in the results list.
#[allow(dead_code)]
const RESULTS_MAX_SNIPPET_CHARS_IN_LIST: usize = 60;
/// Separator between title and snippet in the results list.
#[allow(dead_code)]
const RESULTS_SNIPPET_SEP: &str = " | ";

fn fill_rect(frame: &mut Frame<'_>, area: Rect, bg: PackedRgba) {
    if area.is_empty() {
        return;
    }
    let fg = crate::tui_theme::TuiThemePalette::current().text_primary;
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.fg = fg;
                cell.bg = bg;
            }
        }
    }
}

fn render_splitter_handle(frame: &mut Frame<'_>, area: Rect, vertical: bool, active: bool) {
    if area.is_empty() {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();

    // Repaint the whole splitter gap first so prior layout artifacts never
    // remain visible as stray borders across list/detail content.
    let separator_color = crate::tui_theme::lerp_color(tp.panel_bg, tp.panel_border_dim, 0.58);
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.fg = separator_color;
                cell.bg = tp.panel_bg;
            }
        }
    }
    if active && ((vertical && area.height >= 5) || (!vertical && area.width >= 5)) {
        let x = if vertical {
            area.x.saturating_add(area.width / 2)
        } else {
            area.x.saturating_add(area.width.saturating_sub(1) / 2)
        };
        let y = if vertical {
            area.y.saturating_add(area.height.saturating_sub(1) / 2)
        } else {
            area.y.saturating_add(area.height / 2)
        };
        if let Some(cell) = frame.buffer.get_mut(x, y) {
            *cell = ftui::Cell::from_char('·');
            cell.fg = tp.selection_indicator;
            cell.bg = tp.panel_bg;
        }
    }
}

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
            Self::Required => "Required",
            Self::NotRequired => "Not Required",
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

/// Field scope for limiting FTS search to subject, body, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FieldScope {
    /// Search both subject and body (default FTS behavior).
    #[default]
    SubjectAndBody,
    /// Search subject field only.
    SubjectOnly,
    /// Search body field only.
    BodyOnly,
}

impl FieldScope {
    const fn label(self) -> &'static str {
        match self {
            Self::SubjectAndBody => "Subject+Body",
            Self::SubjectOnly => "Subject Only",
            Self::BodyOnly => "Body Only",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::SubjectAndBody => Self::SubjectOnly,
            Self::SubjectOnly => Self::BodyOnly,
            Self::BodyOnly => Self::SubjectAndBody,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::SubjectAndBody => Self::BodyOnly,
            Self::SubjectOnly => Self::SubjectAndBody,
            Self::BodyOnly => Self::SubjectOnly,
        }
    }

    /// Apply field scope to a query string for FTS5 column filtering.
    /// Returns the query wrapped with column prefix for SubjectOnly/BodyOnly.
    fn apply_to_query(self, query: &str) -> String {
        if query.is_empty() {
            return query.to_string();
        }
        match self {
            Self::SubjectAndBody => query.to_string(),
            Self::SubjectOnly => format!("subject:{query}"),
            Self::BodyOnly => format!("body_md:{query}"),
        }
    }
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

    const fn prev(self) -> Self {
        match self {
            Self::NewestFirst => Self::Relevance,
            Self::OldestFirst => Self::NewestFirst,
            Self::Relevance => Self::OldestFirst,
        }
    }
}

/// Search V3 mode selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SearchModeFilter {
    /// Engine picks the best mode based on query characteristics.
    #[default]
    Auto,
    /// Full-text lexical search (FTS5/Tantivy BM25).
    Lexical,
    /// Vector similarity search (embeddings).
    Semantic,
    /// Two-tier fusion: lexical + semantic reranking.
    Hybrid,
}

impl SearchModeFilter {
    const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Lexical => "Lexical",
            Self::Semantic => "Semantic",
            Self::Hybrid => "Hybrid",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Auto => Self::Lexical,
            Self::Lexical => Self::Semantic,
            Self::Semantic => Self::Hybrid,
            Self::Hybrid => Self::Auto,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Auto => Self::Hybrid,
            Self::Lexical => Self::Auto,
            Self::Semantic => Self::Lexical,
            Self::Hybrid => Self::Semantic,
        }
    }
}

/// Toggle for explain metadata in search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ExplainToggle {
    /// Do not include explain data (default).
    #[default]
    Off,
    /// Include reason codes and score factors.
    On,
}

impl ExplainToggle {
    const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::On => "On",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Off => Self::On,
            Self::On => Self::Off,
        }
    }

    const fn is_on(self) -> bool {
        matches!(self, Self::On)
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
    /// Context snippet centered around matched terms for list/detail previews.
    context_snippet: String,
    /// Approximate number of term matches in the searchable body text.
    match_count: usize,
    /// Full message body for markdown preview (messages only).
    full_body: Option<String>,
    /// Pre-rendered markdown body (messages only) to avoid per-frame markdown parsing.
    rendered_body: Option<Text<'static>>,
    score: Option<f64>,
    importance: Option<String>,
    ack_required: Option<bool>,
    created_ts: Option<i64>,
    thread_id: Option<String>,
    from_agent: Option<String>,
    project_id: Option<i64>,
    /// Explain reason codes (populated when explain=on).
    reason_codes: Vec<String>,
    /// Score factor summaries (populated when explain=on).
    score_factors: Vec<mcp_agent_mail_db::search_planner::ScoreFactorSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchDegradedDiagnostics {
    degraded: bool,
    fallback_mode: Option<String>,
    timeout_stage: Option<String>,
    budget_tier: Option<String>,
    budget_exhausted: Option<bool>,
    remediation_hint: Option<String>,
}

type DetailCacheKey = (i64, &'static str, &'static str);

fn explain_facet_value<'a>(
    explain: &'a mcp_agent_mail_db::search_planner::QueryExplain,
    key: &str,
) -> Option<&'a str> {
    explain.facets_applied.iter().find_map(|facet| {
        let (facet_key, facet_value) = facet.split_once(':')?;
        if facet_key.eq_ignore_ascii_case(key) {
            Some(facet_value)
        } else {
            None
        }
    })
}

fn derive_tui_degraded_diagnostics(
    explain: Option<&mcp_agent_mail_db::search_planner::QueryExplain>,
    selected_mode: SearchModeFilter,
) -> Option<SearchDegradedDiagnostics> {
    let mut diagnostics = SearchDegradedDiagnostics {
        degraded: false,
        fallback_mode: None,
        timeout_stage: None,
        budget_tier: None,
        budget_exhausted: None,
        remediation_hint: None,
    };

    // Current TUI path uses planner SQL for deterministic sync execution.
    // Non-auto mode selection degrades to lexical behavior and must be visible.
    if !matches!(
        selected_mode,
        SearchModeFilter::Auto | SearchModeFilter::Lexical
    ) {
        diagnostics.degraded = true;
        diagnostics.fallback_mode = Some("mode_degraded_to_lexical".to_string());
        diagnostics.remediation_hint = Some(
            "Semantic/Hybrid mode selection is currently degraded to lexical in this TUI path."
                .to_string(),
        );
    }

    if let Some(explain) = explain {
        if explain.used_like_fallback || explain.method.eq_ignore_ascii_case("like_fallback") {
            diagnostics.degraded = true;
            diagnostics
                .fallback_mode
                .get_or_insert_with(|| "like_fallback".to_string());
            diagnostics.remediation_hint.get_or_insert_with(|| {
                "FTS syntax fallback active; simplify operators or use quoted phrases.".to_string()
            });
        }

        if let Some(outcome) = explain_facet_value(explain, "rerank_outcome") {
            if let Some(tier) = outcome.strip_prefix("skipped_by_budget_governor_") {
                diagnostics.degraded = true;
                diagnostics
                    .fallback_mode
                    .get_or_insert_with(|| "hybrid_budget_governor".to_string());
                diagnostics.budget_tier = Some(tier.to_string());
                diagnostics.budget_exhausted = Some(tier.eq_ignore_ascii_case("critical"));
                diagnostics.remediation_hint.get_or_insert_with(|| {
                    "Budget pressure detected; lower limit or narrow filters.".to_string()
                });
            } else if outcome.to_ascii_lowercase().contains("timeout") {
                diagnostics.degraded = true;
                diagnostics
                    .fallback_mode
                    .get_or_insert_with(|| "rerank_timeout".to_string());
                diagnostics.timeout_stage = Some("rerank".to_string());
                diagnostics.remediation_hint.get_or_insert_with(|| {
                    "Rerank timed out; retry with tighter query scope.".to_string()
                });
            }
        }

        if let Some(stage) = explain_facet_value(explain, "timeout_stage") {
            diagnostics.degraded = true;
            diagnostics.timeout_stage = Some(stage.to_string());
            diagnostics.remediation_hint.get_or_insert_with(|| {
                "A search stage timed out; narrow query scope and retry.".to_string()
            });
        }
    }

    if diagnostics.degraded {
        Some(diagnostics)
    } else {
        None
    }
}

/// Wrapper for `VirtualizedList` rendering of search results.
#[derive(Debug, Clone)]
struct SearchResultRow {
    entry: ResultEntry,
    /// Lowercased positive needles cached once per result refresh.
    highlight_needles: Arc<Vec<String>>,
    /// Sort direction for displaying score or date.
    sort_direction: SortDirection,
}

impl RenderItem for SearchResultRow {
    #[allow(clippy::too_many_lines)]
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool, _skip_rows: u16) {
        use ftui::widgets::Widget;

        if area.height == 0 || area.width < 10 {
            return;
        }

        let w = area.width as usize;

        // Marker for selected row
        let marker = if selected {
            crate::tui_theme::SELECTION_PREFIX
        } else {
            crate::tui_theme::SELECTION_PREFIX_EMPTY
        };
        let tp = crate::tui_theme::TuiThemePalette::current();
        let cursor_style = Style::default()
            .fg(tp.selection_fg)
            .bg(tp.selection_bg)
            .bold();

        let meta_style = crate::tui_theme::text_meta(&tp);
        let accent_style = crate::tui_theme::text_accent(&tp);
        let warning_style = crate::tui_theme::text_warning(&tp);
        let primary_style = crate::tui_theme::text_primary(&tp);

        // Doc type badge with semantic color
        let (type_badge, type_style) = match self.entry.doc_kind {
            DocKind::Message => ("M", primary_style),
            DocKind::Agent => ("A", accent_style),
            DocKind::Project => ("P", meta_style),
            DocKind::Thread => ("T", meta_style),
        };

        // Importance badge with severity color
        let (imp_badge, imp_style) = match self.entry.importance.as_deref() {
            Some("urgent") => ("!!", crate::tui_theme::text_critical(&tp)),
            Some("high") => ("! ", warning_style),
            _ => ("  ", meta_style),
        };

        // Score or date prefix
        let meta_text = if self.sort_direction == SortDirection::Relevance {
            self.entry
                .score
                .map_or_else(|| "      ".to_string(), |s| format!("{s:>5.2} "))
        } else {
            self.entry.created_ts.map_or_else(
                || "        ".to_string(),
                |ts| {
                    let iso = mcp_agent_mail_db::timestamps::micros_to_iso(ts);
                    if iso.len() >= 16 {
                        format!("{} ", &iso[5..16])
                    } else {
                        format!("{iso:>11} ")
                    }
                },
            )
        };

        // Build prefix with styled spans
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::raw(marker.to_string()));
        spans.push(Span::styled(format!("[{type_badge}]"), type_style));
        spans.push(Span::styled(imp_badge.to_string(), imp_style));
        spans.push(Span::styled(meta_text, meta_style));

        if self.entry.match_count > 0 && self.entry.doc_kind == DocKind::Message {
            spans.push(Span::styled(
                format!("x{} ", self.entry.match_count),
                crate::tui_theme::text_meta(&tp),
            ));
        }

        let prefix_len = spans
            .iter()
            .map(|s| ftui::text::display_width(s.as_str()))
            .sum::<usize>();
        let remaining = w.saturating_sub(prefix_len);
        let title = truncate_display_width(&self.entry.title, remaining);

        // Title with optional highlighting
        let highlight_style = Style::default().fg(RESULT_CURSOR_FG()).bold();

        let highlight_enabled = selected && !self.highlight_needles.is_empty();
        if highlight_enabled {
            spans.extend(highlight_spans_with_needles(
                &title,
                &self.highlight_needles,
                Some(primary_style),
                highlight_style,
            ));
        } else {
            spans.push(Span::styled(title, primary_style));
        }

        let mut lines = vec![Line::from_spans(spans)];
        let snippet_source = if self.entry.context_snippet.is_empty() {
            self.entry.body_preview.as_str()
        } else {
            self.entry.context_snippet.as_str()
        };
        let show_context_line = area.height > 1
            && self.entry.doc_kind == DocKind::Message
            && !snippet_source.is_empty();
        if show_context_line {
            let max_context_lines = usize::from(area.height.saturating_sub(1)).clamp(1, 3);
            let mut context_segments = snippet_source
                .split(" ⟫ ")
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .peekable();
            let highlight_context = highlight_enabled;
            if context_segments.peek().is_none() {
                let context_prefix = "  ↳ ";
                let snippet_width = w.saturating_sub(ftui::text::display_width(context_prefix));
                let snippet = truncate_display_width(snippet_source, snippet_width);
                let mut context_spans = Vec::with_capacity(2);
                context_spans.push(Span::styled(context_prefix, meta_style));
                if highlight_context {
                    context_spans.extend(highlight_spans_with_needles(
                        &snippet,
                        &self.highlight_needles,
                        Some(crate::tui_theme::text_hint(&tp)),
                        highlight_style,
                    ));
                } else {
                    context_spans.push(Span::styled(snippet, crate::tui_theme::text_hint(&tp)));
                }
                lines.push(Line::from_spans(context_spans));
            } else {
                let mut rendered = 0usize;
                while rendered < max_context_lines {
                    let Some(segment) = context_segments.next() else {
                        break;
                    };
                    let context_prefix = if rendered == 0 { "  ↳ " } else { "    " };
                    let snippet_width = w.saturating_sub(ftui::text::display_width(context_prefix));
                    let snippet = truncate_display_width(segment, snippet_width);
                    let mut context_spans = Vec::with_capacity(2);
                    context_spans.push(Span::styled(context_prefix, meta_style));
                    if highlight_context {
                        context_spans.extend(highlight_spans_with_needles(
                            &snippet,
                            &self.highlight_needles,
                            Some(crate::tui_theme::text_hint(&tp)),
                            highlight_style,
                        ));
                    } else {
                        context_spans.push(Span::styled(snippet, crate::tui_theme::text_hint(&tp)));
                    }
                    lines.push(Line::from_spans(context_spans));
                    rendered += 1;
                }
                if context_segments.next().is_some() && lines.len() < usize::from(area.height) {
                    lines.push(Line::from_spans([Span::styled(
                        "    …",
                        crate::tui_theme::text_hint(&tp),
                    )]));
                }
            }
        }

        let mut para =
            ftui::widgets::paragraph::Paragraph::new(ftui::text::Text::from_lines(lines));
        if selected {
            para = para.style(cursor_style);
        } else {
            para = para.style(primary_style);
        }
        para.render(area, frame);
    }

    fn height(&self) -> u16 {
        if self.entry.doc_kind == DocKind::Message
            && (!self.entry.context_snippet.is_empty() || !self.entry.body_preview.is_empty())
        {
            3
        } else {
            1
        }
    }
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

fn build_highlight_needles(terms: &[QueryTerm]) -> Vec<String> {
    terms
        .iter()
        .filter(|t| !t.negated)
        .map(|t| t.text.to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .take(MAX_HIGHLIGHT_TERMS)
        .collect()
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
            if let Some(pos) = hay.find(&needle)
                && (best_pos.is_none() || pos < best_pos.unwrap_or(usize::MAX))
            {
                best_pos = Some(pos);
                best_len = needle.len();
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
    let needles = build_highlight_needles(terms);
    highlight_spans_with_needles(text, &needles, base_style, highlight_style)
}

fn highlight_spans_with_needles(
    text: &str,
    needles: &[String],
    base_style: Option<Style>,
    highlight_style: Style,
) -> Vec<Span<'static>> {
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
        for needle in needles {
            if let Some(rel) = hay[i..].find(needle.as_str()) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockDragState {
    Idle,
    Dragging,
}

/// Which facet is currently highlighted in the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FacetSlot {
    Scope,
    DocKind,
    Importance,
    AckStatus,
    SortOrder,
    FieldScope,
    SearchMode,
    Explain,
}

impl FacetSlot {
    const fn next(self) -> Self {
        match self {
            Self::Scope => Self::DocKind,
            Self::DocKind => Self::Importance,
            Self::Importance => Self::AckStatus,
            Self::AckStatus => Self::SortOrder,
            Self::SortOrder => Self::FieldScope,
            Self::FieldScope => Self::SearchMode,
            Self::SearchMode => Self::Explain,
            Self::Explain => Self::Scope,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Scope => Self::Explain,
            Self::DocKind => Self::Scope,
            Self::Importance => Self::DocKind,
            Self::AckStatus => Self::Importance,
            Self::SortOrder => Self::AckStatus,
            Self::FieldScope => Self::SortOrder,
            Self::SearchMode => Self::FieldScope,
            Self::Explain => Self::SearchMode,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// SearchCockpitScreen
// ──────────────────────────────────────────────────────────────────────

/// Unified search cockpit with query bar, facet rail, and results.
#[allow(clippy::struct_excessive_bools)]
pub struct SearchCockpitScreen {
    // Query input
    query_input: TextInput,

    // Facet state
    scope_mode: ScopeMode,
    doc_kind_filter: DocKindFilter,
    importance_filter: ImportanceFilter,
    ack_filter: AckFilter,
    sort_direction: SortDirection,
    field_scope: FieldScope,
    search_mode: SearchModeFilter,
    explain_toggle: ExplainToggle,
    thread_filter: Option<String>,
    highlight_terms: Vec<QueryTerm>,

    // Results
    results: Vec<ResultEntry>,
    result_rows: Vec<SearchResultRow>,
    cursor: usize,
    detail_scroll: usize,
    total_sql_rows: usize,

    // Focus
    focus: Focus,
    active_facet: FacetSlot,
    query_help_visible: bool,
    /// Query Lab panel visible (toggled by `L`). Hidden by default for
    /// progressive disclosure — power users can reveal debug state.
    query_lab_visible: bool,

    // Search state
    db_conn: Option<DbConn>,
    db_conn_attempted: bool,
    last_query: String,
    last_error: Option<String>,
    query_assistance: Option<QueryAssistance>,
    /// Zero-result recovery guidance (populated when results are empty).
    guidance: Option<ZeroResultGuidance>,
    /// Derived degraded-mode diagnostics for the most recent message query.
    last_diagnostics: Option<SearchDegradedDiagnostics>,
    /// Most recent search execution time in milliseconds.
    last_search_ms: Option<u32>,
    debounce_remaining: u8,
    search_dirty: bool,

    // Recipes and history
    saved_recipes: Vec<SearchRecipe>,
    query_history: Vec<QueryHistoryEntry>,
    history_cursor: Option<usize>,
    recipes_loaded: bool,

    /// Synthetic event for the focused search result (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,

    /// `VirtualizedList` state for efficient rendering of search results.
    list_state: RefCell<VirtualizedListState>,
    /// Resizable results/detail layout.
    dock: DockLayout,
    /// Current drag state while resizing split.
    dock_drag: DockDragState,
    /// Last rendered query bar area.
    last_query_area: Cell<Rect>,
    /// Last rendered full screen area.
    last_screen_area: Cell<Rect>,
    /// Last rendered facet area.
    last_facet_area: Cell<Rect>,
    /// Last rendered results area.
    last_results_area: Cell<Rect>,
    /// Last rendered detail area.
    last_detail_area: Cell<Rect>,
    /// Last split area (results + detail), used for border hit-testing.
    last_split_area: Cell<Rect>,
    /// Small animation phase for header/status flourish.
    ui_phase: u8,
    /// Cached markdown render for selected message bodies, keyed by message id.
    rendered_markdown_cache: RefCell<HashMap<(i64, &'static str), Arc<Text<'static>>>>,
    /// Cached detail panel text for selected entries, keyed by (id, kind, theme).
    rendered_detail_cache: RefCell<HashMap<DetailCacheKey, Arc<Text<'static>>>>,
    /// Last-seen data generation snapshot for dirty-state gating.
    last_data_gen: super::DataGeneration,
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
            field_scope: FieldScope::default(),
            search_mode: SearchModeFilter::default(),
            explain_toggle: ExplainToggle::default(),
            thread_filter: None,
            highlight_terms: Vec::new(),
            results: Vec::new(),
            result_rows: Vec::new(),
            cursor: 0,
            detail_scroll: 0,
            total_sql_rows: 0,
            focus: Focus::ResultList,
            active_facet: FacetSlot::DocKind,
            query_help_visible: false,
            query_lab_visible: false,
            db_conn: None,
            db_conn_attempted: false,
            last_query: String::new(),
            last_error: None,
            query_assistance: None,
            guidance: None,
            last_diagnostics: None,
            last_search_ms: None,
            debounce_remaining: 0,
            search_dirty: true,
            saved_recipes: Vec::new(),
            query_history: Vec::new(),
            history_cursor: None,
            recipes_loaded: false,
            focused_synthetic: None,
            list_state: RefCell::new(VirtualizedListState::default()),
            dock: DockLayout::right_40(),
            dock_drag: DockDragState::Idle,
            last_query_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_screen_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_facet_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_results_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_detail_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_split_area: Cell::new(Rect::new(0, 0, 0, 0)),
            ui_phase: 0,
            rendered_markdown_cache: RefCell::new(HashMap::new()),
            rendered_detail_cache: RefCell::new(HashMap::new()),
            last_data_gen: super::DataGeneration::stale(),
        }
    }

    /// Sync the `VirtualizedListState` with our cursor position.
    fn sync_list_state(&self) {
        let mut state = self.list_state.borrow_mut();
        state.select(Some(self.cursor));
    }

    fn rebuild_result_rows(&mut self) {
        let shared_needles = Arc::new(build_highlight_needles(&self.highlight_terms));
        self.result_rows = self
            .results
            .iter()
            .cloned()
            .map(|entry| SearchResultRow {
                entry,
                highlight_needles: Arc::clone(&shared_needles),
                sort_direction: self.sort_direction,
            })
            .collect();
    }

    /// Return cached markdown render for a message entry, generating lazily.
    fn cached_rendered_markdown(&self, entry: &ResultEntry) -> Option<Arc<Text<'static>>> {
        if entry.doc_kind != DocKind::Message {
            return None;
        }
        let body = entry
            .full_body
            .as_deref()
            .filter(|raw| !raw.trim().is_empty())?;
        let theme_key = crate::tui_theme::current_theme_env_value();
        let cache_key = (entry.id, theme_key);
        if let Some(existing) = self.rendered_markdown_cache.borrow().get(&cache_key) {
            return Some(Arc::clone(existing));
        }
        let theme = crate::tui_theme::markdown_theme();
        let rendered = Arc::new(tui_markdown::render_body(body, &theme));
        let mut cache = self.rendered_markdown_cache.borrow_mut();
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(cache_key, Arc::clone(&rendered));
        Some(rendered)
    }

    /// Return cached detail text for an entry, generating lazily.
    fn cached_rendered_detail(&self, entry: &ResultEntry) -> Arc<Text<'static>> {
        let theme_key = crate::tui_theme::current_theme_env_value();
        let cache_key = (entry.id, entry.doc_kind.as_str(), theme_key);
        if let Some(existing) = self.rendered_detail_cache.borrow().get(&cache_key) {
            return Arc::clone(existing);
        }

        let rendered_body = self.cached_rendered_markdown(entry);
        let tp = crate::tui_theme::TuiThemePalette::current();
        let rendered = Arc::new(compose_detail_text(
            entry,
            &self.highlight_terms,
            self.last_diagnostics.as_ref(),
            rendered_body.as_deref(),
            &tp,
        ));

        let mut cache = self.rendered_detail_cache.borrow_mut();
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(cache_key, Arc::clone(&rendered));
        rendered
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected search result.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self.results.get(self.cursor).and_then(|entry| {
            match entry.doc_kind {
                DocKind::Message | DocKind::Thread => {
                    Some(crate::tui_events::MailEvent::message_sent(
                        entry.id,
                        entry.from_agent.as_deref().unwrap_or(""),
                        vec![], // to-agents not stored in search results
                        &entry.title,
                        entry.thread_id.as_deref().unwrap_or(""),
                        "", // project slug not directly available
                    ))
                }
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
        let db_url = &state.config_snapshot().raw_database_url;
        let cfg = DbPoolConfig {
            database_url: db_url.clone(),
            ..Default::default()
        };
        if let Ok(path) = cfg.sqlite_path() {
            self.db_conn = mcp_agent_mail_db::open_sqlite_file_with_recovery(&path).ok();
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
        let started = Instant::now();
        let raw = self.query_input.value().trim().to_string();
        self.last_query.clone_from(&raw);
        self.last_diagnostics = None;
        self.last_error = validate_query_syntax(&raw);
        if self.last_error.is_some() {
            self.query_assistance = None;
            self.highlight_terms.clear();
            self.results.clear();
            self.result_rows.clear();
            self.rendered_markdown_cache.borrow_mut().clear();
            self.rendered_detail_cache.borrow_mut().clear();
            self.total_sql_rows = 0;
            self.guidance = None;
            self.cursor = 0;
            self.detail_scroll = 0;
            self.search_dirty = false;
            self.last_search_ms = None;
            return;
        }

        self.query_assistance = if raw.is_empty() {
            None
        } else {
            let assistance = parse_query_assistance(&raw);
            if assistance.applied_filter_hints.is_empty() && assistance.did_you_mean.is_empty() {
                None
            } else {
                Some(assistance)
            }
        };

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
        self.rendered_markdown_cache.borrow_mut().clear();
        self.rendered_detail_cache.borrow_mut().clear();

        // Generate zero-result guidance from TUI facet state
        self.guidance = if self.results.is_empty() && !raw.is_empty() {
            Some(self.build_guidance())
        } else {
            None
        };

        // Clamp cursor
        if self.results.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.results.len() - 1);
        }
        self.detail_scroll = 0;
        self.rebuild_result_rows();
        self.search_dirty = false;
        let elapsed_ms = started.elapsed().as_millis();
        self.last_search_ms = Some(u32::try_from(elapsed_ms).unwrap_or(u32::MAX));
        self.record_history();
    }

    /// Build zero-result recovery guidance from current TUI facet state.
    fn build_guidance(&self) -> ZeroResultGuidance {
        let mut suggestions = Vec::new();

        if self.importance_filter != ImportanceFilter::Any {
            suggestions.push(RecoverySuggestion {
                kind: "drop_importance_filter".to_string(),
                label: "Remove importance filter".to_string(),
                detail: Some(format!(
                    "Currently filtering to \"{}\". Try removing the importance constraint.",
                    self.importance_filter.label()
                )),
            });
        }
        if self.ack_filter != AckFilter::Any {
            suggestions.push(RecoverySuggestion {
                kind: "drop_ack_filter".to_string(),
                label: "Remove ack-required filter".to_string(),
                detail: None,
            });
        }
        if self.thread_filter.is_some() {
            suggestions.push(RecoverySuggestion {
                kind: "drop_thread_filter".to_string(),
                label: "Remove thread filter".to_string(),
                detail: Some("Search across all threads instead of a single thread.".to_string()),
            });
        }
        if let Some(ref assist) = self.query_assistance {
            for hint in &assist.did_you_mean {
                suggestions.push(RecoverySuggestion {
                    kind: "fix_typo".to_string(),
                    label: format!("Did you mean \"{}:{}\"?", hint.suggested_field, hint.value),
                    detail: Some(format!(
                        "\"{}\" is not a recognized field. Try \"{}\" instead.",
                        hint.token, hint.suggested_field
                    )),
                });
            }
        }
        if suggestions.is_empty() {
            suggestions.push(RecoverySuggestion {
                kind: "simplify_query".to_string(),
                label: "Simplify search terms".to_string(),
                detail: Some("Try fewer or broader keywords.".to_string()),
            });
        }

        let count = suggestions.len();
        ZeroResultGuidance {
            summary: format!(
                "No results found. {count} suggestion{s} available to broaden your search.",
                count = count,
                s = if count == 1 { "" } else { "s" }
            ),
            suggestions,
        }
    }

    /// Run a search for a single doc kind using sync queries.
    fn run_kind_search(&mut self, conn: &DbConn, kind: DocKind, raw: &str) -> Vec<ResultEntry> {
        match kind {
            DocKind::Message | DocKind::Thread => self.search_messages(conn, raw),
            DocKind::Agent => Self::search_agents(conn, raw),
            DocKind::Project => Self::search_projects(conn, raw),
        }
    }

    /// Search messages using the global planner for non-empty queries.
    fn search_messages(&mut self, conn: &DbConn, raw: &str) -> Vec<ResultEntry> {
        if raw.is_empty() {
            return self.search_messages_recent(conn);
        }

        // Apply field scope to constrain search to subject/body/both
        let scoped_query = self.field_scope.apply_to_query(raw);

        let mut query = SearchQuery {
            text: scoped_query,
            doc_kind: DocKind::Message,
            limit: Some(MAX_RESULTS),
            explain: self.explain_toggle.is_on(),
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
        self.last_diagnostics =
            derive_tui_degraded_diagnostics(Some(&plan.explain()), self.search_mode);
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
    fn search_messages_recent(&mut self, conn: &DbConn) -> Vec<ResultEntry> {
        self.last_diagnostics = derive_tui_degraded_diagnostics(None, self.search_mode);
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
    fn search_agents(conn: &DbConn, raw: &str) -> Vec<ResultEntry> {
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
    fn search_projects(conn: &DbConn, raw: &str) -> Vec<ResultEntry> {
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
            FacetSlot::FieldScope => self.field_scope = self.field_scope.next(),
            FacetSlot::SearchMode => self.search_mode = self.search_mode.next(),
            FacetSlot::Explain => self.explain_toggle = self.explain_toggle.next(),
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
        self.field_scope = FieldScope::default();
        self.search_mode = SearchModeFilter::default();
        self.explain_toggle = ExplainToggle::default();
        self.thread_filter = None;
        self.search_dirty = true;
        self.debounce_remaining = 0;
    }

    fn set_cursor_from_results_click(&mut self, y: u16) {
        if self.results.is_empty() {
            return;
        }
        let area = self.last_results_area.get();
        let list_height = area.height.saturating_sub(2) as usize;
        if list_height == 0 {
            return;
        }
        let inner_top = area.y.saturating_add(1);
        if y < inner_top {
            return;
        }
        let row = usize::from(y.saturating_sub(inner_top));
        let (start, end) = viewport_range(self.results.len(), list_height, self.cursor);
        let idx = start.saturating_add(row);
        if idx < end {
            self.cursor = idx;
            self.detail_scroll = 0;
        }
    }

    fn facet_slot_from_click(&self, y: u16) -> Option<FacetSlot> {
        let area = self.last_facet_area.get();
        if area.height <= 2 {
            return None;
        }
        let inner_top = area.y.saturating_add(1);
        if y < inner_top {
            return None;
        }
        let row = usize::from(y.saturating_sub(inner_top));
        match row / 2 {
            0 => Some(FacetSlot::Scope),
            1 => Some(FacetSlot::DocKind),
            2 => Some(FacetSlot::Importance),
            3 => Some(FacetSlot::AckStatus),
            4 => Some(FacetSlot::SortOrder),
            5 => Some(FacetSlot::FieldScope),
            _ => None,
        }
    }

    fn set_active_facet_from_click(&mut self, y: u16) -> bool {
        if let Some(slot) = self.facet_slot_from_click(y) {
            self.active_facet = slot;
            true
        } else {
            false
        }
    }

    const fn detail_visible(&self) -> bool {
        let area = self.last_detail_area.get();
        area.width > 0 && area.height > 0
    }

    fn detail_max_scroll(&self) -> usize {
        let Some(entry) = self.results.get(self.cursor) else {
            return 0;
        };
        let area = self.last_detail_area.get();
        // Border (2) + action bar (1) are fixed; only content body scrolls.
        // Fallback viewport for pre-render calls (unit tests or early key events).
        let visible = if area.height <= 3 {
            8
        } else {
            usize::from(area.height.saturating_sub(3))
        };
        let width = if area.width == 0 {
            80
        } else {
            area.width.saturating_sub(2).max(1)
        };
        let detail_text = self.cached_rendered_detail(entry);
        let total = estimate_wrapped_text_lines(detail_text.as_ref(), usize::from(width));
        total.saturating_sub(visible)
    }

    fn scroll_detail_by(&mut self, delta: isize) {
        let max = self.detail_max_scroll();
        if delta.is_negative() {
            self.detail_scroll = self
                .detail_scroll
                .saturating_sub(delta.unsigned_abs())
                .min(max);
        } else {
            #[allow(clippy::cast_sign_loss)]
            let add = delta as usize;
            self.detail_scroll = self.detail_scroll.saturating_add(add).min(max);
        }
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
        if let Some(ref conn) = self.db_conn
            && let Ok(id) = insert_recipe(conn, &recipe)
        {
            let mut saved = recipe;
            saved.id = Some(id);
            self.saved_recipes.insert(0, saved);
            // Evict oldest non-pinned recipes when over the cap.
            while self.saved_recipes.len() > MAX_RECIPES {
                if let Some(pos) = self.saved_recipes.iter().rposition(|r| !r.pinned) {
                    self.saved_recipes.remove(pos);
                } else {
                    break; // all remaining are pinned
                }
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

    fn assistance_hint_line(&self) -> Option<String> {
        let assistance = self.query_assistance.as_ref()?;
        let mut parts = Vec::new();
        if !assistance.applied_filter_hints.is_empty() {
            let applied = assistance
                .applied_filter_hints
                .iter()
                .map(|hint| format!("{}={}", hint.field, hint.value))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("Filters: {applied}"));
        }
        if !assistance.did_you_mean.is_empty() {
            let suggestions = assistance
                .did_you_mean
                .iter()
                .map(|hint| format!("{} -> {}", hint.token, hint.suggested_field))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("Did you mean: {suggestions}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" | "))
        }
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
        DocKind::Thread => 3,
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
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        match event {
            Event::Mouse(mouse) => {
                if self.query_help_visible {
                    let popup = query_help_popup_rect(
                        self.last_screen_area.get(),
                        self.last_query_area.get(),
                    );
                    let inside = popup.is_some_and(|rect| point_in_rect(rect, mouse.x, mouse.y));
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && !inside {
                        self.query_help_visible = false;
                    }
                    // Query help popup traps pointer input while visible.
                    return Cmd::None;
                }
                let split_area = self.last_split_area.get();
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if self.detail_visible()
                            && self.dock.hit_test_border(split_area, mouse.x, mouse.y)
                        {
                            self.dock_drag = DockDragState::Dragging;
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_query_area.get(), mouse.x, mouse.y) {
                            self.focus = Focus::QueryBar;
                            self.query_input.set_focused(true);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_facet_area.get(), mouse.x, mouse.y) {
                            self.focus = Focus::FacetRail;
                            self.query_input.set_focused(false);
                            let prev = self.active_facet;
                            if self.set_active_facet_from_click(mouse.y)
                                && self.active_facet == prev
                            {
                                self.toggle_active_facet();
                            }
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_results_area.get(), mouse.x, mouse.y) {
                            self.focus = Focus::ResultList;
                            self.query_input.set_focused(false);
                            self.set_cursor_from_results_click(mouse.y);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_detail_area.get(), mouse.x, mouse.y) {
                            self.focus = Focus::ResultList;
                            self.query_input.set_focused(false);
                            return Cmd::None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if self.dock_drag == DockDragState::Dragging {
                            self.dock.drag_to(split_area, mouse.x, mouse.y);
                            return Cmd::None;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        self.dock_drag = DockDragState::Idle;
                    }
                    MouseEventKind::ScrollDown => {
                        if point_in_rect(self.last_query_area.get(), mouse.x, mouse.y) {
                            self.sort_direction = self.sort_direction.next();
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_facet_area.get(), mouse.x, mouse.y) {
                            self.active_facet = self.active_facet.next();
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_detail_area.get(), mouse.x, mouse.y) {
                            self.scroll_detail_by(1);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_results_area.get(), mouse.x, mouse.y)
                            && !self.results.is_empty()
                        {
                            self.cursor = (self.cursor + 1).min(self.results.len() - 1);
                            self.detail_scroll = 0;
                            return Cmd::None;
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if point_in_rect(self.last_query_area.get(), mouse.x, mouse.y) {
                            self.sort_direction = self.sort_direction.prev();
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_facet_area.get(), mouse.x, mouse.y) {
                            self.active_facet = self.active_facet.prev();
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_detail_area.get(), mouse.x, mouse.y) {
                            self.scroll_detail_by(-1);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_results_area.get(), mouse.x, mouse.y) {
                            self.cursor = self.cursor.saturating_sub(1);
                            self.detail_scroll = 0;
                            return Cmd::None;
                        }
                    }
                    _ => {}
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match self.focus {
                Focus::QueryBar => match key.code {
                    // `?` is reserved for query syntax help while query bar is focused.
                    KeyCode::Char('?') => {
                        self.query_help_visible = true;
                    }
                    // If the query-help popup is open, any key dismisses it.
                    _ if self.query_help_visible => {
                        self.query_help_visible = false;
                        return Cmd::None;
                    }
                    KeyCode::Enter => {
                        self.search_dirty = true;
                        self.debounce_remaining = 0;
                        self.focus = Focus::ResultList;
                        self.query_input.set_focused(false);
                        self.history_cursor = None;
                        self.query_help_visible = false;
                    }
                    KeyCode::Escape => {
                        self.focus = Focus::ResultList;
                        self.query_input.set_focused(false);
                        self.history_cursor = None;
                        self.query_help_visible = false;
                    }
                    KeyCode::Tab => {
                        self.focus = Focus::FacetRail;
                        self.query_input.set_focused(false);
                        self.query_help_visible = false;
                    }
                    KeyCode::Up => {
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
                        match self.active_facet {
                            FacetSlot::Scope => self.scope_mode = self.scope_mode.next(),
                            FacetSlot::DocKind => {
                                self.doc_kind_filter = self.doc_kind_filter.prev();
                            }
                            FacetSlot::Importance => {
                                self.importance_filter = self.importance_filter.next();
                            }
                            FacetSlot::AckStatus => self.ack_filter = self.ack_filter.next(),
                            FacetSlot::SortOrder => {
                                self.sort_direction = self.sort_direction.next();
                            }
                            FacetSlot::FieldScope => self.field_scope = self.field_scope.prev(),
                            FacetSlot::SearchMode => self.search_mode = self.search_mode.prev(),
                            FacetSlot::Explain => self.explain_toggle = self.explain_toggle.next(),
                        }
                        self.search_dirty = true;
                        self.debounce_remaining = 0;
                    }
                    KeyCode::Char('r') => self.reset_facets(),
                    KeyCode::Char('L') => self.query_lab_visible = !self.query_lab_visible,
                    KeyCode::Char('I') => self.dock.toggle_visible(),
                    KeyCode::Char(']') => self.dock.grow_dock(),
                    KeyCode::Char('[') => self.dock.shrink_dock(),
                    KeyCode::Char('}') => self.dock.cycle_position(),
                    KeyCode::Char('{') => self.dock.cycle_position_prev(),
                    _ => {}
                },
                Focus::ResultList => match key.code {
                    KeyCode::Char('/') => {
                        self.focus = Focus::QueryBar;
                        self.query_input.set_focused(true);
                    }
                    KeyCode::Tab | KeyCode::Char('f') => self.focus = Focus::FacetRail,
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
                    KeyCode::Char('J') => self.scroll_detail_by(1),
                    KeyCode::Char('K') => self.scroll_detail_by(-1),
                    KeyCode::Char('I') => self.dock.toggle_visible(),
                    KeyCode::Char(']') => self.dock.grow_dock(),
                    KeyCode::Char('[') => self.dock.shrink_dock(),
                    KeyCode::Char('}') => self.dock.cycle_position(),
                    KeyCode::Char('{') => self.dock.cycle_position_prev(),
                    KeyCode::Enter => {
                        if let Some(entry) = self.results.get(self.cursor) {
                            return Cmd::msg(match entry.doc_kind {
                                DocKind::Message => {
                                    MailScreenMsg::DeepLink(DeepLinkTarget::MessageById(entry.id))
                                }
                                DocKind::Agent => MailScreenMsg::DeepLink(
                                    DeepLinkTarget::AgentByName(entry.title.clone()),
                                ),
                                DocKind::Project => MailScreenMsg::DeepLink(
                                    DeepLinkTarget::ProjectBySlug(entry.title.clone()),
                                ),
                                DocKind::Thread => MailScreenMsg::DeepLink(
                                    entry
                                        .thread_id
                                        .as_ref()
                                        .map_or(DeepLinkTarget::MessageById(entry.id), |tid| {
                                            DeepLinkTarget::ThreadById(tid.clone())
                                        }),
                                ),
                            });
                        }
                    }
                    KeyCode::Char('t') => {
                        self.doc_kind_filter = self.doc_kind_filter.next();
                        self.search_dirty = true;
                        self.debounce_remaining = 0;
                    }
                    KeyCode::Char('i') => {
                        self.importance_filter = self.importance_filter.next();
                        self.search_dirty = true;
                        self.debounce_remaining = 0;
                    }
                    KeyCode::Char('o') => {
                        if let Some(entry) = self.results.get(self.cursor)
                            && let Some(ref tid) = entry.thread_id
                        {
                            return Cmd::msg(MailScreenMsg::DeepLink(DeepLinkTarget::ThreadById(
                                tid.clone(),
                            )));
                        }
                    }
                    KeyCode::Char('a') => {
                        if let Some(entry) = self.results.get(self.cursor)
                            && let Some(ref agent) = entry.from_agent
                        {
                            return Cmd::msg(MailScreenMsg::DeepLink(DeepLinkTarget::AgentByName(
                                agent.clone(),
                            )));
                        }
                    }
                    KeyCode::Char('T') => {
                        if let Some(entry) = self.results.get(self.cursor)
                            && let Some(ts) = entry.created_ts
                        {
                            return Cmd::msg(MailScreenMsg::DeepLink(
                                DeepLinkTarget::TimelineAtTime(ts),
                            ));
                        }
                    }
                    KeyCode::Char('L') => {
                        self.query_lab_visible = !self.query_lab_visible;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(Modifiers::CTRL) => {
                        self.query_input.clear();
                        self.reset_facets();
                        self.execute_search(state);
                    }
                    _ => {}
                },
            },
            _ => {}
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        // ── Dirty-state gated data ingestion ────────────────────────
        let current_gen = state.data_generation();
        let _dirty = super::dirty_since(&self.last_data_gen, &current_gen);

        self.ui_phase = (tick_count % 16) as u8;
        if self.search_dirty {
            if self.debounce_remaining > 0 {
                self.debounce_remaining -= 1;
            } else {
                self.execute_search(state);
            }
        }
        self.sync_focused_event();

        self.last_data_gen = current_gen;
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    #[allow(clippy::too_many_lines)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 4 || area.width < 30 {
            let tp = crate::tui_theme::TuiThemePalette::current();
            Block::bordered()
                .title("Search")
                .border_type(BorderType::Rounded)
                .border_style(crate::tui_theme::text_meta(&tp))
                .render(area, frame);
            return;
        }

        // Always paint the whole area before pane rendering.
        let tp = crate::tui_theme::TuiThemePalette::current();
        fill_rect(frame, area, tp.bg_deep);

        // Layout: query bar (3-4h) + body
        let query_h: u16 = if area.height >= 20 {
            6
        } else if area.height >= 16 {
            5
        } else if area.height >= 6 {
            4
        } else {
            3
        };
        let body_h = area.height.saturating_sub(query_h);

        let query_area = Rect::new(area.x, area.y, area.width, query_h);
        let body_area = Rect::new(area.x, area.y + query_h, area.width, body_h);
        self.last_query_area.set(query_area);
        self.last_screen_area.set(area);

        // Body: facet rail (left) + dock-split content area (results/detail)
        let min_remaining_for_results: u16 = 26;
        let max_facet_w = body_area.width.saturating_sub(min_remaining_for_results);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let computed_facet_w = ((f32::from(body_area.width) * 0.2).round() as u16).clamp(12, 28);
        let facet_w_raw: u16 = if body_area.width <= min_remaining_for_results {
            0
        } else {
            computed_facet_w.min(max_facet_w).min(body_area.width)
        };
        let facet_w = if facet_w_raw < 12 { 0 } else { facet_w_raw };
        let facet_gap = u16::from(facet_w > 0 && body_area.width >= SEARCH_FACET_GAP_THRESHOLD);
        let consumed_w = facet_w.saturating_add(facet_gap).min(body_area.width);
        let facet_area = Rect::new(body_area.x, body_area.y, facet_w, body_area.height);
        let split_area = Rect::new(
            body_area.x.saturating_add(consumed_w),
            body_area.y,
            body_area.width.saturating_sub(consumed_w),
            body_area.height,
        );

        let mut dock = self.dock;
        let mut stacked_fallback = false;
        if split_area.height < SEARCH_DOCK_HIDE_HEIGHT_THRESHOLD {
            dock.visible = false;
        } else if split_area.width < SEARCH_STACKED_WIDTH_THRESHOLD {
            if split_area.height >= SEARCH_STACKED_MIN_HEIGHT {
                stacked_fallback = true;
                dock.visible = true;
                dock.position = DockPosition::Bottom;
                dock.set_ratio(SEARCH_STACKED_DOCK_RATIO);
            } else {
                dock.visible = false;
            }
        }

        let layout_label = if dock.visible {
            if stacked_fallback {
                format!("Stacked {}", dock.state_label())
            } else {
                dock.state_label()
            }
        } else {
            "List only".to_string()
        };
        let telemetry = runtime_telemetry_line(state, self.ui_phase);
        render_query_bar(
            frame,
            query_area,
            &self.query_input,
            self,
            matches!(self.focus, Focus::QueryBar),
            &layout_label,
            self.ui_phase,
            &telemetry,
        );

        self.last_facet_area.set(facet_area);
        if facet_area.width > 0 {
            render_facet_rail(
                frame,
                facet_area,
                self,
                matches!(self.focus, Focus::FacetRail),
            );
            if facet_gap > 0 {
                render_splitter_handle(
                    frame,
                    Rect::new(
                        body_area.x.saturating_add(facet_w),
                        body_area.y,
                        facet_gap,
                        body_area.height,
                    ),
                    true,
                    false,
                );
            }
        }

        self.last_split_area.set(split_area);
        let split = dock.split(split_area);
        let mut primary_area = split.primary;
        let mut detail_area = split.dock;
        if let Some(mut dock_area) = detail_area {
            let split_extent = if dock.position.is_horizontal() {
                split_area.height
            } else {
                split_area.width
            };
            let split_gap = u16::from(split_extent >= SEARCH_SPLIT_GAP_THRESHOLD);
            if split_gap > 0 {
                let splitter_area = match dock.position {
                    DockPosition::Right => {
                        dock_area.x = dock_area.x.saturating_add(split_gap);
                        dock_area.width = dock_area.width.saturating_sub(split_gap);
                        Rect::new(
                            split.primary.x.saturating_add(split.primary.width),
                            split_area.y,
                            split_gap,
                            split_area.height,
                        )
                    }
                    DockPosition::Left => {
                        primary_area.x = primary_area.x.saturating_add(split_gap);
                        primary_area.width = primary_area.width.saturating_sub(split_gap);
                        Rect::new(
                            dock_area.x.saturating_add(dock_area.width),
                            split_area.y,
                            split_gap,
                            split_area.height,
                        )
                    }
                    DockPosition::Bottom => {
                        dock_area.y = dock_area.y.saturating_add(split_gap);
                        dock_area.height = dock_area.height.saturating_sub(split_gap);
                        Rect::new(
                            split_area.x,
                            split.primary.y.saturating_add(split.primary.height),
                            split_area.width,
                            split_gap,
                        )
                    }
                    DockPosition::Top => {
                        primary_area.y = primary_area.y.saturating_add(split_gap);
                        primary_area.height = primary_area.height.saturating_sub(split_gap);
                        Rect::new(
                            split_area.x,
                            dock_area.y.saturating_add(dock_area.height),
                            split_area.width,
                            split_gap,
                        )
                    }
                };
                render_splitter_handle(
                    frame,
                    splitter_area,
                    !dock.position.is_horizontal(),
                    self.dock_drag == DockDragState::Dragging,
                );
            }
            detail_area = if dock_area.width > 0 && dock_area.height > 0 {
                Some(dock_area)
            } else {
                None
            };
        }

        let results_area =
            if facet_area.width == 0 && primary_area.width >= 24 && primary_area.height >= 3 {
                let hint_area = Rect::new(primary_area.x, primary_area.y, primary_area.width, 1);
                render_collapsed_facet_hint(frame, hint_area, self);
                Rect::new(
                    primary_area.x,
                    primary_area.y.saturating_add(1),
                    primary_area.width,
                    primary_area.height.saturating_sub(1),
                )
            } else {
                primary_area
            };
        self.last_results_area.set(results_area);
        self.last_detail_area
            .set(detail_area.unwrap_or(Rect::new(0, 0, 0, 0)));

        self.sync_list_state();
        render_results(
            frame,
            results_area,
            &self.result_rows,
            &mut self.list_state.borrow_mut(),
            self.cursor,
            &self.highlight_terms,
            self.sort_direction,
            matches!(self.focus, Focus::ResultList),
            self.guidance.as_ref(),
        );
        if let Some(detail_area) = detail_area {
            let selected_entry = self.results.get(self.cursor);
            let rendered_body_override =
                selected_entry.and_then(|entry| self.cached_rendered_markdown(entry));
            let rendered_detail_override =
                selected_entry.map(|entry| self.cached_rendered_detail(entry));
            render_detail(
                frame,
                detail_area,
                selected_entry,
                self.detail_scroll,
                &self.highlight_terms,
                self.last_diagnostics.as_ref(),
                rendered_body_override.as_deref(),
                rendered_detail_override.as_deref(),
                matches!(self.focus, Focus::ResultList),
            );
        }

        // Render query help popup LAST so it appears on top of body content
        if self.query_help_visible {
            render_query_help_popup(frame, area, query_area);
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
                key: "I [ ] { }",
                action: "Toggle/resize/reposition split",
            },
            HelpEntry {
                key: "Mouse",
                action: "Click/select, wheel facets/sort, drag split",
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
                key: "?",
                action: "Query syntax help (query bar)",
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
            HelpEntry {
                key: "L",
                action: "Toggle query lab",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Full-text search across messages. Supports AND, OR, NOT operators.")
    }

    fn consumes_text_input(&self) -> bool {
        matches!(self.focus, Focus::QueryBar)
    }

    fn copyable_content(&self) -> Option<String> {
        let entry = self.results.get(self.cursor)?;
        Some(entry.full_body.as_ref().map_or_else(
            || {
                if entry.body_preview.is_empty() {
                    entry.title.clone()
                } else {
                    format!("{}\n\n{}", entry.title, entry.body_preview)
                }
            },
            |body| format!("{}\n\n{}", entry.title, body),
        ))
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
            DeepLinkTarget::SearchFocused(query) => {
                self.focus = Focus::QueryBar;
                self.query_input.set_focused(true);
                if !query.is_empty() {
                    self.query_input.set_value(query);
                    self.search_dirty = true;
                    self.debounce_remaining = 0;
                }
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
    conn: &DbConn,
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
                    let body_md: String = row.get_named("body_md").unwrap_or_default();
                    // Keep row extraction lightweight: defer rich markdown rendering to detail view.
                    let subject_text = collapse_whitespace(&subject);
                    let has_highlight_terms = highlight_terms
                        .iter()
                        .any(|term| !term.negated && term.text.len() >= 2);
                    let (line_cap, char_cap) = if has_highlight_terms {
                        (SEARCHABLE_BODY_MAX_LINES, SEARCHABLE_BODY_MAX_CHARS)
                    } else {
                        (SEARCHABLE_PREVIEW_MAX_LINES, SEARCHABLE_PREVIEW_MAX_CHARS)
                    };
                    let body_lines =
                        markdown_to_searchable_lines_with_caps(&body_md, line_cap, char_cap);
                    let subject_match_count = if has_highlight_terms {
                        count_term_matches(&subject_text, highlight_terms)
                    } else {
                        0
                    };
                    let body_match_count = if has_highlight_terms {
                        count_term_matches_in_lines(&body_lines, highlight_terms)
                    } else {
                        0
                    };
                    let match_count = subject_match_count.saturating_add(body_match_count);
                    let body_preview_source = if body_lines.is_empty() {
                        String::new()
                    } else {
                        body_lines
                            .iter()
                            .take(8)
                            .map(|line| line.text.clone())
                            .collect::<Vec<_>>()
                            .join(" ⟫ ")
                    };

                    let mut preview = if !has_highlight_terms {
                        truncate_str(&body_preview_source, 120)
                    } else if body_match_count > 0 {
                        extract_context_snippet_from_lines(
                            &body_lines,
                            highlight_terms,
                            MAX_SNIPPET_CHARS,
                        )
                    } else if subject_match_count > 0 {
                        let subject_snippet =
                            extract_snippet(&subject_text, highlight_terms, MAX_SNIPPET_CHARS);
                        format!("subject: {subject_snippet}")
                    } else {
                        extract_snippet(&body_preview_source, highlight_terms, MAX_SNIPPET_CHARS)
                    };
                    if preview.is_empty() {
                        preview = truncate_str(&subject_text, 120);
                    }
                    let body_preview = if body_preview_source.is_empty() {
                        truncate_str(&subject_text, 320)
                    } else {
                        truncate_str(&body_preview_source, 320)
                    };

                    Some(ResultEntry {
                        id,
                        doc_kind: DocKind::Message,
                        title: subject,
                        body_preview,
                        context_snippet: preview,
                        match_count,
                        full_body: Some(body_md),
                        rendered_body: None,
                        score: row.get_named("score").ok(),
                        importance: row.get_named("importance").ok(),
                        ack_required: row.get_named::<i64>("ack_required").ok().map(|v| v != 0),
                        created_ts: row.get_named("created_ts").ok(),
                        thread_id: row.get_named("thread_id").ok(),
                        from_agent: row.get_named("from_name").ok(),
                        project_id: row.get_named("project_id").ok(),
                        reason_codes: Vec::new(),
                        score_factors: Vec::new(),
                    })
                })
                .collect()
        })
}

fn query_agent_rows(conn: &DbConn, sql: &str, params: &[Value]) -> Vec<ResultEntry> {
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
                        context_snippet: String::new(),
                        match_count: 0,
                        full_body: None,
                        rendered_body: None,
                        score: row.get_named("score").ok(),
                        importance: None,
                        ack_required: None,
                        created_ts: None,
                        thread_id: None,
                        from_agent: None,
                        project_id: row.get_named("project_id").ok(),
                        reason_codes: Vec::new(),
                        score_factors: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn query_project_rows(conn: &DbConn, sql: &str, params: &[Value]) -> Vec<ResultEntry> {
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
                        context_snippet: String::new(),
                        match_count: 0,
                        full_body: None,
                        rendered_body: None,
                        score: row.get_named("score").ok(),
                        importance: None,
                        ack_required: None,
                        created_ts: None,
                        thread_id: None,
                        from_agent: None,
                        project_id: Some(id),
                        reason_codes: Vec::new(),
                        score_factors: Vec::new(),
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
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        t.push('\u{2026}');
        t
    }
}

fn truncate_display_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if ftui::text::display_width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    let budget = max_width - 1;
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let ch_s = ch.encode_utf8(&mut buf);
        let ch_w = ftui::text::display_width(ch_s);
        if used.saturating_add(ch_w) > budget {
            break;
        }
        out.push(ch);
        used = used.saturating_add(ch_w);
    }
    out.push('…');
    while ftui::text::display_width(out.as_str()) > max_width {
        let _ = out.pop();
        if out.pop().is_none() {
            return "…".to_string();
        }
        out.push('…');
    }
    out
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

#[derive(Debug, Clone)]
struct SearchableLine {
    line_no: usize,
    text: String,
}

#[cfg(test)]
fn markdown_to_searchable_lines(markdown: &str) -> Vec<SearchableLine> {
    markdown_to_searchable_lines_with_caps(
        markdown,
        SEARCHABLE_BODY_MAX_LINES,
        SEARCHABLE_BODY_MAX_CHARS,
    )
}

fn markdown_to_searchable_lines_with_caps(
    markdown: &str,
    max_lines: usize,
    max_chars: usize,
) -> Vec<SearchableLine> {
    if markdown.trim().is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut scanned_chars = 0usize;
    for (line_idx, line) in markdown.lines().enumerate() {
        if lines.len() >= max_lines || scanned_chars >= max_chars {
            break;
        }
        scanned_chars = scanned_chars.saturating_add(line.len().saturating_add(1));
        let bounded_line = if line.len() > max_chars {
            let stop = clamp_to_char_boundary(line, max_chars);
            &line[..stop]
        } else {
            line
        };
        // Remove common markdown decoration while preserving semantic content.
        let stripped = bounded_line
            .trim_start_matches(['#', '>', '-', '*', '+', '|', '`'])
            .trim()
            .trim_matches('|')
            .trim();
        if stripped.is_empty() {
            continue;
        }
        // Ignore GFM table separator lines like |---|:---:|.
        if stripped
            .chars()
            .all(|c| matches!(c, '-' | ':' | '|') || c.is_whitespace())
        {
            continue;
        }
        let collapsed = collapse_whitespace(stripped);
        if !collapsed.is_empty() {
            lines.push(SearchableLine {
                line_no: line_idx.saturating_add(1),
                text: collapsed,
            });
        }
    }
    lines
}

fn count_term_matches_in_lines(lines: &[SearchableLine], terms: &[QueryTerm]) -> usize {
    lines
        .iter()
        .map(|line| count_term_matches(&line.text, terms))
        .sum()
}

fn extract_context_snippet_from_lines(
    lines: &[SearchableLine],
    terms: &[QueryTerm],
    max_chars: usize,
) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let needles = build_highlight_needles(terms);
    let match_line_idx = if needles.is_empty() {
        None
    } else {
        let mut best_idx: Option<usize> = None;
        let mut best_score = 0usize;
        for (idx, line) in lines.iter().enumerate() {
            let hay = line.text.to_ascii_lowercase();
            let score = needles
                .iter()
                .map(|needle| hay.matches(needle).count())
                .sum::<usize>();
            if score > best_score || (score > 0 && score == best_score) {
                best_score = score;
                best_idx = Some(idx);
            }
        }
        best_idx
    };

    let snippet = match_line_idx.map_or_else(
        || {
            lines
                .iter()
                .take(CONTEXT_NO_HIT_LINES)
                .map(|line| format!("L{}: {}", line.line_no, line.text))
                .collect::<Vec<_>>()
                .join(" ⟫ ")
        },
        |hit_idx| {
            let start = hit_idx.saturating_sub(CONTEXT_HIT_RADIUS_LINES);
            let end = (hit_idx + CONTEXT_HIT_RADIUS_LINES + 1).min(lines.len());
            let segments = lines[start..end]
                .iter()
                .filter(|line| !line.text.is_empty())
                .map(|line| format!("L{}: {}", line.line_no, line.text))
                .collect::<Vec<_>>();
            if segments.is_empty() {
                String::new()
            } else {
                let mut joined = segments.join(" ⟫ ");
                if start > 0 {
                    joined = format!("… {joined}");
                }
                if end < lines.len() {
                    joined.push_str(" ⟫ …");
                }
                joined
            }
        },
    );

    truncate_str(snippet.trim(), max_chars)
}

fn count_term_matches(text: &str, terms: &[QueryTerm]) -> usize {
    if text.is_empty() || terms.is_empty() {
        return 0;
    }
    let hay = text.to_ascii_lowercase();
    let mut total = 0usize;
    for term in terms.iter().filter(|term| !term.negated) {
        if term.text.len() < 2 {
            continue;
        }
        let needle = term.text.to_ascii_lowercase();
        if needle.is_empty() {
            continue;
        }
        let mut offset = 0usize;
        while offset < hay.len() {
            let Some(pos) = hay[offset..].find(&needle) else {
                break;
            };
            total = total.saturating_add(1);
            offset = offset.saturating_add(pos.saturating_add(needle.len().max(1)));
        }
    }
    total
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

#[allow(non_snake_case)]
fn FACET_ACTIVE_FG() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().status_accent
}
#[allow(non_snake_case)]
fn FACET_LABEL_FG() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().text_muted
}
#[allow(non_snake_case)]
fn RESULT_CURSOR_FG() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().selection_indicator
}
#[allow(non_snake_case)]
fn ERROR_FG() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().severity_error
}
#[allow(non_snake_case)]
fn ACTION_KEY_FG() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().severity_ok
}
#[allow(non_snake_case)]
fn QUERY_HELP_BG() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().bg_deep
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_query_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &TextInput,
    screen: &SearchCockpitScreen,
    focused: bool,
    layout_label: &str,
    ui_phase: u8,
    telemetry: &str,
) {
    let count = screen.results.len();
    let kind_label = screen.doc_kind_filter.label();
    let active_filters = usize::from(screen.thread_filter.is_some())
        + usize::from(screen.importance_filter != ImportanceFilter::Any)
        + usize::from(screen.ack_filter != AckFilter::Any)
        + usize::from(screen.doc_kind_filter != DocKindFilter::Messages)
        + usize::from(screen.scope_mode != ScopeMode::Global)
        + usize::from(screen.field_scope != FieldScope::default());
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

    let spinner = spinner_glyph(ui_phase);
    let title = format!(
        "{spinner} Search {kind_label} ({count} results, {active_filters} filters){thread_label} [{layout_label}]{focus_label}"
    );

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);
    let content_inner = if inner.width > 2 {
        Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        )
    } else {
        inner
    };

    let input_area = Rect::new(content_inner.x, content_inner.y, content_inner.width, 1);
    input.render(input_area, frame);

    // Optional hint line when the query bar has extra height.
    if content_inner.height >= 2 {
        let w = content_inner.width as usize;
        let (hint, style) = screen.last_error.as_ref().map_or_else(
            || {
                screen
                    .last_diagnostics
                    .as_ref()
                    .filter(|diag| diag.degraded)
                    .map_or_else(
                        || {
                            screen.assistance_hint_line().map_or_else(
                                || {
                                    if screen.focus == Focus::QueryBar {
                                        (
                                            "Syntax: \"phrase\" term* AND/OR/NOT | field:value | Mouse wheel sort + drag split"
                                                .to_string(),
                                            Style::default().fg(FACET_LABEL_FG()),
                                        )
                                    } else {
                                        (
                                            format!("Route: {}", screen.route_string()),
                                            Style::default().fg(FACET_LABEL_FG()),
                                        )
                                    }
                                },
                                |line| (line, Style::default().fg(FACET_LABEL_FG())),
                            )
                        },
                        |diag| {
                            let mut parts = Vec::new();
                            if let Some(mode) = &diag.fallback_mode {
                                parts.push(format!("fallback={mode}"));
                            }
                            if let Some(stage) = &diag.timeout_stage {
                                parts.push(format!("timeout_stage={stage}"));
                            }
                            if let Some(tier) = &diag.budget_tier {
                                parts.push(format!("budget_tier={tier}"));
                            }
                            if let Some(exhausted) = diag.budget_exhausted {
                                parts.push(format!("budget_exhausted={exhausted}"));
                            }
                            if let Some(hint) = &diag.remediation_hint {
                                parts.push(format!("hint={hint}"));
                            }
                            (
                                format!("DEGRADED: {}", parts.join(" | ")),
                                crate::tui_theme::text_warning(&tp),
                            )
                        },
                    )
            },
            |err| (format!("ERR: {err}"), Style::default().fg(ERROR_FG())),
        );

        let hint_area = Rect::new(content_inner.x, content_inner.y + 1, content_inner.width, 1);
        Paragraph::new(truncate_display_width(&hint, w))
            .style(style)
            .render(hint_area, frame);
    }

    // Chips line: show when query is active or focus is on query bar
    // (progressive disclosure — hidden when browsing results with no query)
    let has_active_query = !screen.query_input.value().trim().is_empty();
    let in_query = matches!(screen.focus, Focus::QueryBar);
    if content_inner.height >= 3 && (has_active_query || in_query) {
        let meter = pulse_meter(ui_phase, 8);
        let state_label = if screen.search_dirty {
            if screen.debounce_remaining > 0 {
                format!("pending:{}t", screen.debounce_remaining)
            } else {
                "running".to_string()
            }
        } else {
            "ready".to_string()
        };
        let latency_label = screen
            .last_search_ms
            .map_or_else(|| "n/a".to_string(), |ms| format!("{ms}ms"));
        let chips = format!(
            "{}  state:{}  scope:{}  type:{}  sort:{}  terms:{}  sql:{}  latency:{}",
            meter,
            state_label,
            screen.scope_mode.label(),
            screen.doc_kind_filter.label(),
            screen.sort_direction.label(),
            screen.highlight_terms.len(),
            screen.total_sql_rows,
            latency_label,
        );
        let chips_area = Rect::new(content_inner.x, content_inner.y + 2, content_inner.width, 1);
        Paragraph::new(truncate_display_width(&chips, content_inner.width as usize))
            .style(Style::default().fg(RESULT_CURSOR_FG()))
            .render(chips_area, frame);
    }

    if content_inner.height >= 4 && (has_active_query || in_query) {
        let telemetry_area =
            Rect::new(content_inner.x, content_inner.y + 3, content_inner.width, 1);
        Paragraph::new(truncate_display_width(
            telemetry,
            content_inner.width as usize,
        ))
        .style(Style::default().fg(FACET_ACTIVE_FG()))
        .render(telemetry_area, frame);
    }
}

fn render_collapsed_facet_hint(frame: &mut Frame<'_>, area: Rect, screen: &SearchCockpitScreen) {
    if area.width < 16 || area.height == 0 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let hint = format!(
        " Facets hidden | type:{} imp:{} sort:{} mode:{} | [f] focus facets",
        screen.doc_kind_filter.label(),
        screen.importance_filter.label(),
        screen.sort_direction.label(),
        screen.search_mode.label(),
    );
    Paragraph::new(truncate_display_width(&hint, area.width as usize))
        .style(crate::tui_theme::text_hint(&tp))
        .render(area, frame);
}

fn query_help_popup_rect(area: Rect, query_area: Rect) -> Option<Rect> {
    if area.width < 28 || area.height < 6 {
        return None;
    }
    let width = area.width.saturating_sub(2).min(60);
    let height = 12_u16.min(area.height.saturating_sub(2));
    if width < 24 || height < 5 {
        return None;
    }
    let x = area.x + 1;
    // Prefer below query bar, but flip above when there is not enough room.
    let below_y = query_area.y.saturating_add(query_area.height);
    let area_bottom = area.y.saturating_add(area.height);
    let y = if below_y.saturating_add(height) <= area_bottom {
        below_y
    } else if query_area.y >= area.y.saturating_add(height) {
        query_area.y.saturating_sub(height)
    } else {
        area_bottom.saturating_sub(height)
    };
    Some(Rect::new(x, y.max(area.y), width, height))
}

fn render_query_help_popup(frame: &mut Frame<'_>, area: Rect, query_area: Rect) {
    let Some(popup_area) = query_help_popup_rect(area, query_area) else {
        return;
    };
    let tp = crate::tui_theme::TuiThemePalette::current();

    let block = Block::bordered()
        .title("Query Syntax Help")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().fg(FACET_LABEL_FG()).bg(QUERY_HELP_BG()));
    let inner = block.inner(popup_area);
    block.render(popup_area, frame);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);

    let text = "AND/OR: error AND deploy\n\
Quotes: \"build failed\"\n\
Prefix: deploy*\n\
NOT: error NOT test\n\
Column: subject:deploy\n\
Esc/any key: close";

    Paragraph::new(text)
        .style(Style::default().fg(FACET_LABEL_FG()).bg(QUERY_HELP_BG()))
        .render(inner, frame);
}

#[allow(clippy::too_many_lines)]
fn render_facet_rail(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &SearchCockpitScreen,
    focused: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title("Facets")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let in_rail = screen.focus == Focus::FacetRail;
    let w = inner.width as usize;

    let facets: &[(FacetSlot, &str, &str)] = &[
        (FacetSlot::Scope, "Scope", screen.scope_mode.label()),
        (
            FacetSlot::DocKind,
            "Doc Type",
            screen.doc_kind_filter.label(),
        ),
        (
            FacetSlot::SearchMode,
            "Search Mode",
            screen.search_mode.label(),
        ),
        (
            FacetSlot::Importance,
            "Importance",
            screen.importance_filter.label(),
        ),
        (
            FacetSlot::AckStatus,
            "Ack Required",
            screen.ack_filter.label(),
        ),
        (
            FacetSlot::SortOrder,
            "Sort Order",
            screen.sort_direction.label(),
        ),
        (
            FacetSlot::FieldScope,
            "Search Field",
            screen.field_scope.label(),
        ),
        (FacetSlot::Explain, "Explain", screen.explain_toggle.label()),
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
            Style::default().fg(FACET_ACTIVE_FG()).bg(tp.bg_overlay)
        } else {
            Style::default().fg(FACET_LABEL_FG())
        };

        // Label row
        let label_text = format!("{marker} {label}");
        let label_line = truncate_display_width(&label_text, w);
        let label_area = Rect::new(inner.x, y, inner.width, 1);
        Paragraph::new(label_line)
            .style(label_style)
            .render(label_area, frame);

        // Value row (indented)
        let value_y = y + 1;
        if value_y < inner.y + inner.height {
            let val_text = format!("  [{value}]");
            let val_line = truncate_display_width(&val_text, w);
            let val_area = Rect::new(inner.x, value_y, inner.width, 1);
            let val_style = if is_active {
                Style::default().fg(RESULT_CURSOR_FG()).bg(tp.bg_overlay)
            } else {
                Style::default()
            };
            Paragraph::new(val_line)
                .style(val_style)
                .render(val_area, frame);
        }
    }

    // Thread filter indicator (after 8 facets x 2 rows each = 16)
    if let Some(ref tid) = screen.thread_filter {
        let y = inner.y + 16;
        if y + 1 < inner.y + inner.height {
            let thread_text = format!(
                "  Thread: {}",
                truncate_display_width(tid, w.saturating_sub(10))
            );
            let thread_area = Rect::new(inner.x, y, inner.width, 1);
            Paragraph::new(thread_text)
                .style(Style::default().fg(FACET_ACTIVE_FG()))
                .render(thread_area, frame);
        }
    }

    if screen.query_lab_visible {
        render_query_lab(frame, inner, screen);
    }

    // Help hint at bottom
    let help_y = inner.y + inner.height - 1;
    if help_y > inner.y + 17 {
        let hint = if in_rail {
            "Enter:toggle  wheel:facet  r:reset  L:lab"
        } else {
            "f:facets  L:query lab  mouse:click/wheel"
        };
        let hint_area = Rect::new(inner.x, help_y, inner.width, 1);
        Paragraph::new(truncate_display_width(hint, w))
            .style(Style::default().fg(FACET_LABEL_FG()))
            .render(hint_area, frame);
    }
}

fn render_query_lab(frame: &mut Frame<'_>, inner: Rect, screen: &SearchCockpitScreen) {
    if inner.height < 19 || inner.width < 14 {
        return;
    }

    let top = inner.y.saturating_add(18);
    let available_h = inner
        .y
        .saturating_add(inner.height)
        .saturating_sub(top)
        .saturating_sub(1);
    if available_h < 4 {
        return;
    }
    let lab_area = Rect::new(inner.x, top, inner.width, available_h);

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title("Query Lab")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border_dim));
    let lab_inner = block.inner(lab_area);
    block.render(lab_area, frame);
    if lab_inner.height == 0 || lab_inner.width == 0 {
        return;
    }
    fill_rect(frame, lab_inner, tp.panel_bg);

    let mut rows: Vec<String> = Vec::new();
    let q = screen.query_input.value().trim();
    rows.push(format!(
        "q: {}",
        if q.is_empty() {
            "<empty>".to_string()
        } else {
            truncate_str(q, 36)
        }
    ));
    rows.push(format!(
        "route: {}",
        truncate_str(&screen.route_string(), 36)
    ));
    let meter = pulse_meter(screen.ui_phase, 6);
    rows.push(format!(
        "{meter} terms:{} matches:{}",
        screen.highlight_terms.len(),
        screen.results.len()
    ));
    rows.push(format!(
        "focus:{} sort:{}",
        match screen.focus {
            Focus::QueryBar => "query",
            Focus::FacetRail => "facets",
            Focus::ResultList => "results",
        },
        screen.sort_direction.label()
    ));
    if let Some(diag) = screen
        .last_diagnostics
        .as_ref()
        .filter(|diag| diag.degraded)
    {
        let mut parts = Vec::new();
        if let Some(mode) = &diag.fallback_mode {
            parts.push(format!("fallback={mode}"));
        }
        if let Some(stage) = &diag.timeout_stage {
            parts.push(format!("timeout={stage}"));
        }
        if let Some(tier) = &diag.budget_tier {
            parts.push(format!("budget={tier}"));
        }
        rows.push(format!("degraded: {}", parts.join(" | ")));
    }

    for (idx, row) in rows.into_iter().enumerate() {
        let y = lab_inner.y + u16::try_from(idx).unwrap_or(0);
        if y >= lab_inner.y + lab_inner.height {
            break;
        }
        let line_area = Rect::new(lab_inner.x, y, lab_inner.width, 1);
        let style = if idx == 0 {
            Style::default().fg(RESULT_CURSOR_FG())
        } else {
            Style::default().fg(FACET_LABEL_FG())
        };
        Paragraph::new(truncate_display_width(&row, lab_inner.width as usize))
            .style(style)
            .render(line_area, frame);
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
#[allow(clippy::too_many_lines)]
fn result_entry_line(
    entry: &ResultEntry,
    is_cursor: bool,
    cfg: &ResultListRenderCfg<'_>,
) -> Line<'static> {
    let marker = if is_cursor { '>' } else { ' ' };

    let kind_badge = match entry.doc_kind {
        DocKind::Message => "M",
        DocKind::Agent => "A",
        DocKind::Project => "P",
        DocKind::Thread => "T",
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
    prefix = truncate_display_width(&prefix, cfg.width);
    let remaining = cfg
        .width
        .saturating_sub(ftui::text::display_width(prefix.as_str()));

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

    let title = truncate_display_width(&entry.title, title_w);
    let snippet_source = if entry.context_snippet.is_empty() {
        entry.body_preview.as_str()
    } else {
        entry.context_snippet.as_str()
    };
    let snippet = if include_snippet {
        truncate_display_width(snippet_source, snippet_w)
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

/// Render search results using `VirtualizedList` for O(1) scroll performance.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_results(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[SearchResultRow],
    list_state: &mut VirtualizedListState,
    cursor: usize,
    highlight_terms: &[QueryTerm],
    sort_direction: SortDirection,
    focused: bool,
    guidance: Option<&ZeroResultGuidance>,
) {
    // Show match count and search posture in header.
    let title = if rows.is_empty() {
        "Results".to_string()
    } else {
        let count = rows.len();
        let plural = if count == 1 { "match" } else { "matches" };
        let active = cursor.min(count.saturating_sub(1)) + 1;
        format!(
            "Results (active {active}/{count} • {count} {plural} • {} • {} terms)",
            sort_direction.label(),
            highlight_terms.len()
        )
    };
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);
    let content = if inner.width > 2 {
        Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        )
    } else {
        inner
    };
    if content.height == 0 || content.width == 0 {
        return;
    }

    if rows.is_empty() {
        if let Some(guide) = guidance {
            render_guidance(frame, content, guide, &tp);
        } else {
            Paragraph::new("  No results found.")
                .style(crate::tui_theme::text_hint(&tp))
                .render(content, frame);
        }
        return;
    }

    let mut list_area = content;
    if content.height >= 6 {
        let summary_area = Rect::new(content.x, content.y, content.width, 1);
        let radar_area = Rect::new(content.x, content.y.saturating_add(1), content.width, 1);
        let context_area = Rect::new(content.x, content.y.saturating_add(2), content.width, 1);
        list_area = Rect::new(
            content.x,
            content.y.saturating_add(3),
            content.width,
            content.height.saturating_sub(3),
        );

        let active = cursor.min(rows.len().saturating_sub(1));
        let active_matches = rows[active].entry.match_count;
        let total_matches: usize = rows.iter().map(|row| row.entry.match_count).sum();
        let summary = format!(
            "active:{}  row_matches:{}  total_matches:{}  visible_rows:{}",
            active + 1,
            active_matches,
            total_matches,
            list_area.height,
        );
        Paragraph::new(truncate_display_width(
            &summary,
            summary_area.width as usize,
        ))
        .style(crate::tui_theme::text_hint(&tp))
        .render(summary_area, frame);

        let density = rows
            .iter()
            .take(80)
            .map(|row| {
                if row.entry.match_count == 0 {
                    0.25
                } else {
                    f64::from(u32::try_from(row.entry.match_count).unwrap_or(u32::MAX))
                }
            })
            .collect::<Vec<_>>();
        if !density.is_empty() && radar_area.width > 0 && radar_area.height > 0 {
            Sparkline::new(&density)
                .style(Style::default().fg(tp.status_accent))
                .render(radar_area, frame);
        }

        let entry = &rows[active].entry;
        let snippet_source = if entry.context_snippet.is_empty() {
            entry.body_preview.as_str()
        } else {
            entry.context_snippet.as_str()
        };
        let snippet = if snippet_source.is_empty() {
            "(no contextual snippet)".to_string()
        } else if snippet_source.contains(" ⟫ ") {
            snippet_source
                .split(" ⟫ ")
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .take(2)
                .map(|segment| {
                    truncate_display_width(segment, context_area.width.saturating_sub(10) as usize)
                })
                .collect::<Vec<_>>()
                .join("  |  ")
        } else {
            truncate_display_width(
                snippet_source,
                context_area.width.saturating_sub(10) as usize,
            )
        };
        let mut context_spans = vec![Span::styled(
            "context: ".to_string(),
            crate::tui_theme::text_meta(&tp),
        )];
        context_spans.extend(highlight_spans(
            &snippet,
            highlight_terms,
            Some(crate::tui_theme::text_hint(&tp)),
            Style::default().fg(tp.selection_indicator).bold(),
        ));
        Paragraph::new(Text::from_line(Line::from_spans(context_spans)))
            .render(context_area, frame);
    } else if content.height >= 5 {
        let summary_area = Rect::new(content.x, content.y, content.width, 1);
        let radar_area = Rect::new(content.x, content.y.saturating_add(1), content.width, 1);
        list_area = Rect::new(
            content.x,
            content.y.saturating_add(2),
            content.width,
            content.height.saturating_sub(2),
        );

        let active = cursor.min(rows.len().saturating_sub(1));
        let active_matches = rows[active].entry.match_count;
        let total_matches: usize = rows.iter().map(|row| row.entry.match_count).sum();
        let summary = format!(
            "active:{}  row_matches:{}  total_matches:{}  visible_rows:{}",
            active + 1,
            active_matches,
            total_matches,
            list_area.height,
        );
        Paragraph::new(truncate_display_width(
            &summary,
            summary_area.width as usize,
        ))
        .style(crate::tui_theme::text_hint(&tp))
        .render(summary_area, frame);

        let density = rows
            .iter()
            .take(80)
            .map(|row| {
                if row.entry.match_count == 0 {
                    0.25
                } else {
                    f64::from(u32::try_from(row.entry.match_count).unwrap_or(u32::MAX))
                }
            })
            .collect::<Vec<_>>();
        if !density.is_empty() && radar_area.width > 0 && radar_area.height > 0 {
            Sparkline::new(&density)
                .style(Style::default().fg(tp.status_accent))
                .render(radar_area, frame);
        }
    }

    // Render using VirtualizedList for efficient scrolling
    let list = VirtualizedList::new(rows)
        .style(crate::tui_theme::text_primary(&tp))
        .highlight_style(
            Style::default()
                .fg(tp.selection_fg)
                .bg(tp.selection_bg)
                .bold(),
        )
        .show_scrollbar(rows.len() > usize::from(list_area.height));

    list.render(list_area, frame, list_state);
}

/// Render zero-result recovery guidance inside the results pane.
fn render_guidance(
    frame: &mut Frame<'_>,
    area: Rect,
    guidance: &ZeroResultGuidance,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    let label_style = crate::tui_theme::text_meta(tp);
    let accent_style = crate::tui_theme::text_accent(tp);
    let value_style = crate::tui_theme::text_primary(tp);

    let mut lines = Vec::new();
    lines.push(Line::styled(format!("  {}", guidance.summary), label_style));
    lines.push(Line::raw(String::new()));

    for (i, suggestion) in guidance.suggestions.iter().enumerate() {
        lines.push(Line::from_spans([
            Span::styled(format!("  {}. ", i + 1), label_style),
            Span::styled(suggestion.label.clone(), accent_style),
        ]));
        if let Some(ref detail) = suggestion.detail {
            lines.push(Line::styled(format!("     {detail}"), value_style));
        }
    }

    let text = Text::from_iter(lines);
    Paragraph::new(text)
        .style(crate::tui_theme::text_primary(tp))
        .render(area, frame);
}

#[allow(clippy::too_many_lines)]
fn compose_detail_text(
    entry: &ResultEntry,
    highlight_terms: &[QueryTerm],
    diagnostics: Option<&SearchDegradedDiagnostics>,
    rendered_body_override: Option<&Text<'static>>,
    tp: &crate::tui_theme::TuiThemePalette,
) -> Text<'static> {
    let label_style = Style::default().fg(FACET_LABEL_FG());
    let highlight_style = Style::default().fg(RESULT_CURSOR_FG()).bold();
    let value_style = crate::tui_theme::text_primary(tp);
    let accent_style = crate::tui_theme::text_accent(tp);

    // Helper: build a label+value line with consistent styling.
    let styled_field = |label: &str, value: String| -> Line {
        Line::from_spans([
            Span::styled(label.to_string(), label_style),
            Span::styled(value, value_style),
        ])
    };

    let mut lines: Vec<Line> = Vec::new();

    let type_label = match entry.doc_kind {
        DocKind::Message => "Message",
        DocKind::Agent => "Agent",
        DocKind::Project => "Project",
        DocKind::Thread => "Thread",
    };
    lines.push(styled_field("Type:       ", type_label.to_string()));

    let mut title_spans: Vec<Span<'static>> = Vec::new();
    title_spans.push(Span::styled("Title:      ".to_string(), label_style));
    title_spans.extend(highlight_spans(
        &entry.title,
        highlight_terms,
        Some(value_style),
        highlight_style,
    ));
    lines.push(Line::from_spans(title_spans));
    lines.push(styled_field("ID:         ", format!("#{}", entry.id)));

    if let Some(ref agent) = entry.from_agent {
        lines.push(Line::from_spans([
            Span::styled("From:       ".to_string(), label_style),
            Span::styled(agent.clone(), accent_style),
        ]));
    }
    if let Some(ref tid) = entry.thread_id {
        lines.push(styled_field("Thread:     ", tid.clone()));
    }
    if let Some(ref imp) = entry.importance {
        let imp_style = match imp.as_str() {
            "urgent" => crate::tui_theme::text_critical(tp),
            "high" => crate::tui_theme::text_warning(tp),
            _ => value_style,
        };
        lines.push(Line::from_spans([
            Span::styled("Importance: ".to_string(), label_style),
            Span::styled(imp.clone(), imp_style),
        ]));
    }
    if let Some(ack) = entry.ack_required {
        let (ack_text, ack_style) = if ack {
            ("required", accent_style)
        } else {
            ("no", value_style)
        };
        lines.push(Line::from_spans([
            Span::styled("Ack:        ".to_string(), label_style),
            Span::styled(ack_text.to_string(), ack_style),
        ]));
    }
    if let Some(ts) = entry.created_ts {
        lines.push(styled_field("Time:       ", micros_to_iso(ts)));
    }
    if let Some(pid) = entry.project_id {
        lines.push(styled_field("Project:    ", format!("#{pid}")));
    }
    if let Some(score) = entry.score {
        lines.push(styled_field("Score:      ", format!("{score:.3}")));
    }

    if !entry.context_snippet.is_empty() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::styled("Context".to_string(), label_style.bold()));
        let match_label = if entry.match_count == 0 {
            "no matched terms".to_string()
        } else if entry.match_count == 1 {
            "1 match".to_string()
        } else {
            format!("{} matches", entry.match_count)
        };
        lines.push(styled_field("Matches:    ", match_label));
        let context_segments = entry
            .context_snippet
            .split(" ⟫ ")
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if context_segments.is_empty() {
            let mut snippet_spans: Vec<Span<'static>> = Vec::new();
            snippet_spans.push(Span::styled("Context:    ".to_string(), label_style));
            snippet_spans.extend(highlight_spans(
                &entry.context_snippet,
                highlight_terms,
                Some(value_style),
                highlight_style,
            ));
            lines.push(Line::from_spans(snippet_spans));
        } else {
            for (idx, segment) in context_segments.iter().enumerate() {
                let mut snippet_spans: Vec<Span<'static>> = Vec::new();
                let label = if idx == 0 {
                    "Context:    "
                } else {
                    "            "
                };
                snippet_spans.push(Span::styled(label.to_string(), label_style));
                snippet_spans.extend(highlight_spans(
                    segment,
                    highlight_terms,
                    Some(value_style),
                    highlight_style,
                ));
                lines.push(Line::from_spans(snippet_spans));
            }
        }
    }

    if !entry.reason_codes.is_empty() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::styled("Explain".to_string(), label_style.bold()));
        lines.push(styled_field("Reasons:    ", entry.reason_codes.join(", ")));
        for sf in &entry.score_factors {
            let sign = if sf.contribution >= 0.0 { "+" } else { "" };
            lines.push(Line::from_spans([
                Span::styled("  ".to_string(), label_style),
                Span::styled(
                    format!("{}{:.3}", sign, sf.contribution),
                    if sf.contribution >= 0.0 {
                        accent_style
                    } else {
                        value_style
                    },
                ),
                Span::styled(format!(" {} ", sf.key), label_style),
                Span::styled(sf.summary.clone(), value_style),
            ]));
        }
    }

    if let Some(diag) = diagnostics.filter(|diag| diag.degraded) {
        lines.push(Line::raw(String::new()));
        lines.push(Line::styled(
            "Degraded Mode".to_string(),
            label_style.bold(),
        ));
        if let Some(mode) = &diag.fallback_mode {
            lines.push(styled_field("Fallback:   ", mode.clone()));
        }
        if let Some(stage) = &diag.timeout_stage {
            lines.push(styled_field("Timeout:    ", stage.clone()));
        }
        if let Some(tier) = &diag.budget_tier {
            lines.push(styled_field("Budget:     ", format!("tier={tier}")));
        }
        if let Some(exhausted) = diag.budget_exhausted {
            lines.push(styled_field(
                "BudgetExh:  ",
                if exhausted { "true" } else { "false" }.to_string(),
            ));
        }
        if let Some(hint) = &diag.remediation_hint {
            lines.push(styled_field("Hint:       ", hint.clone()));
        }
    }

    lines.push(Line::raw(String::new()));
    if let Some(rendered) = rendered_body_override.or(entry.rendered_body.as_ref()) {
        lines.push(Line::styled("Body".to_string(), label_style.bold()));
        lines.extend(rendered.lines().iter().cloned());
    } else if let Some(ref body) = entry.full_body {
        lines.push(Line::styled("Body".to_string(), label_style.bold()));
        let theme = crate::tui_theme::markdown_theme();
        let rendered = tui_markdown::render_body(body, &theme);
        lines.extend(rendered.lines().iter().cloned());
    } else {
        lines.push(Line::styled("Preview".to_string(), label_style.bold()));
        let theme = crate::tui_theme::markdown_theme();
        let rendered = tui_markdown::render_body(&entry.body_preview, &theme);
        lines.extend(rendered.lines().iter().cloned());
    }

    Text::from_lines(lines)
}

fn estimate_wrapped_text_lines(text: &Text, width: usize) -> usize {
    let wrap_width = width.max(1);
    text.lines()
        .iter()
        .map(|line| {
            let len = line.width();
            if len == 0 {
                1
            } else {
                len.div_ceil(wrap_width)
            }
        })
        .sum::<usize>()
        .max(1)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    entry: Option<&ResultEntry>,
    scroll: usize,
    highlight_terms: &[QueryTerm],
    diagnostics: Option<&SearchDegradedDiagnostics>,
    rendered_body_override: Option<&Text<'static>>,
    rendered_detail_override: Option<&Text<'static>>,
    focused: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title("Detail")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);

    let (content_inner, scrollbar_area) = if inner.width > 6 {
        (
            Rect::new(
                inner.x,
                inner.y,
                inner.width.saturating_sub(1),
                inner.height,
            ),
            Some(Rect::new(
                inner.x + inner.width.saturating_sub(1),
                inner.y,
                1,
                inner.height,
            )),
        )
    } else {
        (inner, None)
    };

    let hint_area = if content_inner.width > 2 && content_inner.height > 2 {
        Rect::new(
            content_inner.x.saturating_add(1),
            content_inner.y.saturating_add(1),
            content_inner.width.saturating_sub(2),
            content_inner.height.saturating_sub(1),
        )
    } else if content_inner.width > 2 {
        Rect::new(
            content_inner.x.saturating_add(1),
            content_inner.y,
            content_inner.width.saturating_sub(2),
            content_inner.height,
        )
    } else {
        content_inner
    };

    let Some(entry) = entry else {
        Paragraph::new("Select a result to view details.")
            .style(crate::tui_theme::text_hint(&tp))
            .render(hint_area, frame);
        return;
    };

    // Reserve 1 row for action bar at bottom.
    let action_bar_h: u16 = 1;
    let content_h = content_inner.height.saturating_sub(action_bar_h);
    let mut content_area = Rect::new(
        content_inner.x,
        content_inner.y,
        content_inner.width,
        content_h,
    );
    if content_area.width > 2 {
        content_area = Rect::new(
            content_area.x.saturating_add(1),
            content_area.y,
            content_area.width.saturating_sub(2),
            content_area.height,
        );
    }
    if content_area.height > 1 {
        content_area = Rect::new(
            content_area.x,
            content_area.y.saturating_add(1),
            content_area.width,
            content_area.height.saturating_sub(1),
        );
    }
    let action_area = Rect::new(
        content_inner.x,
        content_inner.y + content_h,
        content_inner.width,
        action_bar_h,
    );
    let detail_text = rendered_detail_override.cloned().unwrap_or_else(|| {
        compose_detail_text(
            entry,
            highlight_terms,
            diagnostics,
            rendered_body_override,
            &tp,
        )
    });

    if content_h == 0 {
        render_action_bar(frame, action_area, entry);
        return;
    }

    let visible = usize::from(content_h);
    let line_count = detail_text.lines().len().max(1);
    let total_estimated = if line_count <= visible {
        line_count
    } else {
        estimate_wrapped_text_lines(&detail_text, usize::from(content_area.width.max(1)))
    };
    let max_scroll = total_estimated.saturating_sub(visible);
    let clamped_scroll = scroll.min(max_scroll);
    let scroll_rows = u16::try_from(clamped_scroll).unwrap_or(u16::MAX);

    Paragraph::new(detail_text)
        .style(crate::tui_theme::text_primary(&tp))
        .wrap(ftui::text::WrapMode::Word)
        .scroll((scroll_rows, 0))
        .render(content_area, frame);

    if let Some(bar_area) = scrollbar_area {
        render_vertical_scrollbar(
            frame,
            bar_area,
            clamped_scroll,
            visible,
            total_estimated,
            focused,
        );
    }

    // Contextual action bar.
    render_action_bar(frame, action_area, entry);
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_vertical_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    scroll: usize,
    visible: usize,
    total: usize,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    let track_style = crate::tui_theme::text_disabled(&tp);
    let thumb_style = Style::default()
        .fg(if focused {
            tp.selection_indicator
        } else {
            tp.status_accent
        })
        .bold();

    let rows = usize::from(area.height);
    let mut lines = Vec::with_capacity(rows);
    if total <= visible || rows == 0 {
        lines.extend((0..rows).map(|_| Line::styled("\u{2502}", track_style)));
    } else {
        let thumb_len = ((visible as f32 / total as f32) * rows as f32)
            .ceil()
            .max(1.0) as usize;
        let max_start = rows.saturating_sub(thumb_len);
        let denom = total.saturating_sub(visible).max(1) as f32;
        let thumb_start = ((scroll as f32 / denom) * max_start as f32).round() as usize;
        for row in 0..rows {
            if row >= thumb_start && row < thumb_start + thumb_len {
                lines.push(Line::styled("\u{2588}", thumb_style));
            } else {
                lines.push(Line::styled("\u{2502}", track_style));
            }
        }
    }
    Paragraph::new(Text::from_lines(lines)).render(area, frame);
}

/// Render a contextual action bar showing available deep-link keys.
fn render_action_bar(frame: &mut Frame<'_>, area: Rect, entry: &ResultEntry) {
    if area.width < 10 || area.height == 0 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    fill_rect(frame, area, tp.panel_bg);
    let key_style = Style::default().fg(ACTION_KEY_FG()).bold();
    let label_style = Style::default().fg(FACET_LABEL_FG());

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
#[allow(dead_code)]
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

const fn spinner_glyph(phase: u8) -> &'static str {
    match phase % 8 {
        0 | 4 => "\u{25d0}",
        1 | 5 => "\u{25d3}",
        2 | 6 => "\u{25d1}",
        _ => "\u{25d2}",
    }
}

fn pulse_meter(phase: u8, width: usize) -> String {
    const BARS: [char; 8] = [
        '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];
    let w = width.max(4);
    let mut out = String::with_capacity(w);
    for idx in 0..w {
        let pos = (usize::from(phase) + idx) % BARS.len();
        out.push(BARS[pos]);
    }
    out
}

fn runtime_telemetry_line(state: &TuiSharedState, ui_phase: u8) -> String {
    let counters = state.request_counters();
    let err = counters.status_4xx.saturating_add(counters.status_5xx);
    let spark_raw = state.sparkline_snapshot();
    let spark = crate::tui_screens::dashboard::render_sparkline(&spark_raw, 12);
    let meter = pulse_meter(ui_phase, 6);
    let sparkline = if spark.is_empty() {
        "......".to_string()
    } else {
        spark
    };
    let prefix = format!(
        "{meter} req:{} ok:{} err:{} avg:{}ms",
        counters.total,
        counters.status_2xx,
        err,
        state.avg_latency_ms()
    );
    format!("{prefix} spark:{sparkline}")
}

/// Returns `true` if the point `(x, y)` is inside the rectangle.
const fn point_in_rect(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
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
    fn derive_tui_diagnostics_like_fallback() {
        let explain = mcp_agent_mail_db::search_planner::QueryExplain {
            method: "like_fallback".to_string(),
            normalized_query: Some("x".to_string()),
            used_like_fallback: true,
            facet_count: 0,
            facets_applied: Vec::new(),
            sql: "SELECT 1".to_string(),
            scope_policy: "unrestricted".to_string(),
            denied_count: 0,
            redacted_count: 0,
        };
        let diagnostics = derive_tui_degraded_diagnostics(Some(&explain), SearchModeFilter::Auto)
            .expect("diagnostics");
        assert!(diagnostics.degraded);
        assert_eq!(diagnostics.fallback_mode.as_deref(), Some("like_fallback"));
    }

    #[test]
    fn derive_tui_diagnostics_marks_semantic_mode_degraded() {
        let diagnostics =
            derive_tui_degraded_diagnostics(None, SearchModeFilter::Semantic).expect("diagnostics");
        assert!(diagnostics.degraded);
        assert_eq!(
            diagnostics.fallback_mode.as_deref(),
            Some("mode_degraded_to_lexical")
        );
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
    fn sort_direction_prev_cycles() {
        let mut d = SortDirection::NewestFirst;
        d = d.prev();
        assert_eq!(d, SortDirection::Relevance);
        d = d.prev();
        assert_eq!(d, SortDirection::OldestFirst);
        d = d.prev();
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
        assert_eq!(s, FacetSlot::FieldScope);
        s = s.next();
        assert_eq!(s, FacetSlot::SearchMode);
        s = s.next();
        assert_eq!(s, FacetSlot::Explain);
        s = s.next();
        assert_eq!(s, FacetSlot::Scope);
    }

    #[test]
    fn facet_slot_prev_cycles() {
        let mut s = FacetSlot::DocKind;
        s = s.prev();
        assert_eq!(s, FacetSlot::Scope);
        s = s.prev();
        assert_eq!(s, FacetSlot::Explain);
        s = s.prev();
        assert_eq!(s, FacetSlot::SearchMode);
        s = s.prev();
        assert_eq!(s, FacetSlot::FieldScope);
        s = s.prev();
        assert_eq!(s, FacetSlot::SortOrder);
        s = s.prev();
        assert_eq!(s, FacetSlot::AckStatus);
    }

    #[test]
    fn set_active_facet_from_click_ignores_non_facet_rows() {
        let mut screen = SearchCockpitScreen::new();
        screen.last_facet_area.set(Rect::new(0, 0, 20, 24));
        screen.active_facet = FacetSlot::DocKind;

        assert!(screen.set_active_facet_from_click(1));
        assert_eq!(screen.active_facet, FacetSlot::Scope);

        let changed = screen.set_active_facet_from_click(17);
        assert!(!changed);
        assert_eq!(screen.active_facet, FacetSlot::Scope);
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

        // Frame must be tall enough (>=20) so query bar gets height 6, leaving
        // 2 inner rows after Block::bordered() padding for the hint line.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 100, 24), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(text.contains("ERR:"), "expected ERR line, got:\n{text}");
        assert!(
            text.contains("Unbalanced quotes"),
            "expected validation error, got:\n{text}"
        );
    }

    #[test]
    fn query_bar_renders_query_assistance_hint_line() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = SearchCockpitScreen::new();
        screen.query_input.set_value("form:BlueLake thread:br-123");
        screen.query_assistance = Some(parse_query_assistance(screen.query_input.value()));

        // Frame must be tall enough (>=20) so query bar gets height 6, leaving
        // 2 inner rows after Block::bordered() padding for the hint line.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 100, 24), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Did you mean:"),
            "expected assistance hint, got:\n{text}"
        );
        assert!(
            text.contains("thread=br-123"),
            "expected applied filter hint, got:\n{text}"
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
    fn ctrl_c_clears_query_and_refreshes_results_immediately() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::ResultList;
        screen.query_input.set_value("urgent");
        screen.results.push(ResultEntry {
            id: 1,
            doc_kind: DocKind::Message,
            title: "stale result".to_string(),
            body_preview: String::new(),
            context_snippet: String::new(),
            match_count: 0,
            full_body: None,
            rendered_body: None,
            score: None,
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            project_id: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
        });
        screen.search_dirty = true;

        let clear = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('c'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::CTRL,
        });
        let _ = screen.update(&clear, &state);

        assert_eq!(screen.query_input.value(), "");
        assert!(screen.results.is_empty());
        assert!(!screen.search_dirty);
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
    fn narrow_tall_layout_keeps_detail_visible_with_stacked_fallback() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = SearchCockpitScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(50, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 50, 20), &state);

        let detail = screen.last_detail_area.get();
        let results = screen.last_results_area.get();
        assert!(
            detail.width > 0 && detail.height > 0,
            "stacked fallback should keep detail pane visible at 50x20"
        );
        assert_eq!(detail.width, results.width);
        assert!(detail.y > results.y);
    }

    #[test]
    fn narrow_short_layout_hides_detail_when_too_short_for_stack() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = SearchCockpitScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(50, 14, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 50, 14), &state);

        assert_eq!(screen.last_detail_area.get(), Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn facet_rail_collapses_when_width_too_small_for_useful_rail() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = SearchCockpitScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(34, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 34, 20), &state);

        assert_eq!(screen.last_facet_area.get().width, 0);
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
        assert_eq!(s, FacetSlot::Explain);
    }

    #[test]
    fn field_scope_cycles_correctly() {
        let mut fs = FieldScope::SubjectAndBody;
        fs = fs.next();
        assert_eq!(fs, FieldScope::SubjectOnly);
        fs = fs.next();
        assert_eq!(fs, FieldScope::BodyOnly);
        fs = fs.next();
        assert_eq!(fs, FieldScope::SubjectAndBody);
    }

    #[test]
    fn field_scope_prev_cycles_correctly() {
        let mut fs = FieldScope::SubjectAndBody;
        fs = fs.prev();
        assert_eq!(fs, FieldScope::BodyOnly);
        fs = fs.prev();
        assert_eq!(fs, FieldScope::SubjectOnly);
        fs = fs.prev();
        assert_eq!(fs, FieldScope::SubjectAndBody);
    }

    #[test]
    fn search_mode_filter_cycles() {
        let mut m = SearchModeFilter::Auto;
        m = m.next();
        assert_eq!(m, SearchModeFilter::Lexical);
        m = m.next();
        assert_eq!(m, SearchModeFilter::Semantic);
        m = m.next();
        assert_eq!(m, SearchModeFilter::Hybrid);
        m = m.next();
        assert_eq!(m, SearchModeFilter::Auto);
    }

    #[test]
    fn search_mode_filter_prev_cycles() {
        let mut m = SearchModeFilter::Auto;
        m = m.prev();
        assert_eq!(m, SearchModeFilter::Hybrid);
        m = m.prev();
        assert_eq!(m, SearchModeFilter::Semantic);
        m = m.prev();
        assert_eq!(m, SearchModeFilter::Lexical);
        m = m.prev();
        assert_eq!(m, SearchModeFilter::Auto);
    }

    #[test]
    fn explain_toggle_cycles() {
        let mut e = ExplainToggle::Off;
        assert!(!e.is_on());
        e = e.next();
        assert_eq!(e, ExplainToggle::On);
        assert!(e.is_on());
        e = e.next();
        assert_eq!(e, ExplainToggle::Off);
    }

    #[test]
    fn field_scope_subject_only_produces_fts5_column_filter() {
        let scope = FieldScope::SubjectOnly;
        let query = scope.apply_to_query("test query");
        assert_eq!(query, "subject:test query");
    }

    #[test]
    fn field_scope_body_only_produces_fts5_column_filter() {
        let scope = FieldScope::BodyOnly;
        let query = scope.apply_to_query("test query");
        assert_eq!(query, "body_md:test query");
    }

    #[test]
    fn field_scope_subject_and_body_preserves_query() {
        let scope = FieldScope::SubjectAndBody;
        let query = scope.apply_to_query("test query");
        assert_eq!(query, "test query");
    }

    #[test]
    fn field_scope_empty_query_returns_empty() {
        assert_eq!(FieldScope::SubjectOnly.apply_to_query(""), "");
        assert_eq!(FieldScope::BodyOnly.apply_to_query(""), "");
        assert_eq!(FieldScope::SubjectAndBody.apply_to_query(""), "");
    }

    #[test]
    fn toggle_active_facet_field_scope() {
        let mut screen = SearchCockpitScreen::new();
        screen.active_facet = FacetSlot::FieldScope;
        screen.search_dirty = false;
        screen.toggle_active_facet();
        assert_eq!(screen.field_scope, FieldScope::SubjectOnly);
        assert!(screen.search_dirty);
    }

    #[test]
    fn reset_facets_clears_field_scope() {
        let mut screen = SearchCockpitScreen::new();
        screen.field_scope = FieldScope::BodyOnly;
        screen.reset_facets();
        assert_eq!(screen.field_scope, FieldScope::SubjectAndBody);
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
    fn question_mark_opens_query_help_when_query_bar_focused() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::QueryBar;
        screen.query_input.set_focused(true);
        screen.query_input.set_value("deploy");

        let question = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('?'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&question, &state);

        assert!(screen.query_help_visible);
        assert_eq!(screen.query_input.value(), "deploy");
    }

    #[test]
    fn question_mark_does_not_open_query_help_outside_query_bar() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::ResultList;

        let question = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('?'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&question, &state);

        assert!(!screen.query_help_visible);
    }

    #[test]
    fn query_help_popup_dismisses_on_any_key() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::QueryBar;
        screen.query_input.set_focused(true);
        screen.query_input.set_value("deploy");
        screen.query_help_visible = true;

        let key = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('x'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&key, &state);

        assert!(!screen.query_help_visible);
        assert_eq!(screen.query_input.value(), "deploy");
    }

    #[test]
    fn query_help_popup_escape_only_dismisses_popup() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::QueryBar;
        screen.query_input.set_focused(true);
        screen.query_help_visible = true;

        let esc = Event::Key(ftui::KeyEvent {
            code: KeyCode::Escape,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::empty(),
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&esc, &state);

        assert!(!screen.query_help_visible);
        assert_eq!(screen.focus, Focus::QueryBar);
    }

    #[test]
    fn query_help_popup_renders_when_visible() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::QueryBar;
        screen.query_input.set_focused(true);
        screen.query_help_visible = true;

        // Tall frame so the help popup has enough inner rows (after
        // Block::bordered() padding) to display all syntax examples.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 50, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 50), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Query Syntax Help"),
            "expected popup title, got:\n{text}"
        );
        assert!(
            text.contains("subject:deploy"),
            "expected column example, got:\n{text}"
        );
    }

    #[test]
    fn query_help_popup_rect_flips_above_query_when_needed() {
        let area = Rect::new(0, 0, 80, 12);
        let query_area = Rect::new(0, 8, 80, 4);
        let popup = query_help_popup_rect(area, query_area).expect("popup rect");
        assert!(popup.y < query_area.y);
    }

    #[test]
    fn query_help_popup_mouse_outside_dismisses_and_traps() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = SearchCockpitScreen::new();
        screen.query_help_visible = true;
        screen.last_screen_area.set(Rect::new(0, 0, 100, 30));
        screen.last_query_area.set(Rect::new(0, 0, 100, 6));

        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: 0,
            y: 29,
            modifiers: Modifiers::empty(),
        });
        let _ = screen.update(&click, &state);
        assert!(!screen.query_help_visible);
    }

    #[test]
    fn query_help_popup_mouse_inside_keeps_popup_visible() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = SearchCockpitScreen::new();
        screen.query_help_visible = true;
        screen.last_screen_area.set(Rect::new(0, 0, 100, 30));
        screen.last_query_area.set(Rect::new(0, 0, 100, 6));
        let popup =
            query_help_popup_rect(screen.last_screen_area.get(), screen.last_query_area.get())
                .expect("popup rect");

        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: popup.x.saturating_add(1),
            y: popup.y.saturating_add(1),
            modifiers: Modifiers::empty(),
        });
        let _ = screen.update(&click, &state);
        assert!(screen.query_help_visible);
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
    fn extract_context_snippet_includes_line_numbers_and_hit_context() {
        let lines = vec![
            SearchableLine {
                line_no: 3,
                text: "alpha context".to_string(),
            },
            SearchableLine {
                line_no: 4,
                text: "target needle match".to_string(),
            },
            SearchableLine {
                line_no: 5,
                text: "omega context".to_string(),
            },
        ];
        let terms = vec![QueryTerm {
            text: "needle".to_string(),
            kind: QueryTermKind::Word,
            negated: false,
        }];

        let snippet = extract_context_snippet_from_lines(&lines, &terms, 200);
        assert!(snippet.contains("L4:"));
        assert!(snippet.contains("needle"));
        assert!(snippet.contains(" ⟫ "));
    }

    #[test]
    fn extract_context_snippet_prefers_strongest_hit_line() {
        let lines = vec![
            SearchableLine {
                line_no: 10,
                text: "alpha appears once".to_string(),
            },
            SearchableLine {
                line_no: 11,
                text: "noise section".to_string(),
            },
            SearchableLine {
                line_no: 12,
                text: "alpha and beta both appear here".to_string(),
            },
        ];
        let terms = vec![
            QueryTerm {
                text: "alpha".to_string(),
                kind: QueryTermKind::Word,
                negated: false,
            },
            QueryTerm {
                text: "beta".to_string(),
                kind: QueryTermKind::Word,
                negated: false,
            },
        ];

        let snippet = extract_context_snippet_from_lines(&lines, &terms, 200);
        assert!(
            snippet.contains("L12:"),
            "snippet should center strongest hit: {snippet}"
        );
        assert!(
            snippet.contains("beta"),
            "snippet should include strongest-hit terms: {snippet}"
        );
    }

    #[test]
    fn markdown_to_searchable_lines_respects_scan_caps() {
        use std::fmt::Write as _;

        let mut markdown = String::new();
        for idx in 0..200 {
            let _ = writeln!(markdown, "line {idx}");
        }
        let lines = markdown_to_searchable_lines(&markdown);
        assert!(lines.len() <= SEARCHABLE_BODY_MAX_LINES);
        assert!(lines.first().is_some_and(|line| line.line_no > 0));
    }

    #[test]
    fn highlight_spans_preserves_text_and_styles_matches() {
        let terms = vec![QueryTerm {
            text: "needle".to_string(),
            kind: QueryTermKind::Word,
            negated: false,
        }];
        let base = Style::default().fg(FACET_LABEL_FG());
        let highlight = Style::default().fg(RESULT_CURSOR_FG()).bold();
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
        let full_body = "# Hello\n\nThis is **bold** markdown.".to_string();
        let rendered_body =
            crate::tui_markdown::render_body(&full_body, &crate::tui_theme::markdown_theme());
        ResultEntry {
            id: 42,
            doc_kind: DocKind::Message,
            title: "Test subject".to_string(),
            body_preview: "short preview".to_string(),
            context_snippet: "short preview".to_string(),
            match_count: 1,
            full_body: Some(full_body),
            rendered_body: Some(rendered_body),
            score: Some(0.95),
            importance: Some("normal".to_string()),
            ack_required: Some(false),
            created_ts: Some(1_700_000_000_000_000),
            thread_id: Some("test-thread".to_string()),
            from_agent: Some("GoldFox".to_string()),
            project_id: Some(1),
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
        }
    }

    fn make_agent_entry() -> ResultEntry {
        ResultEntry {
            id: 10,
            doc_kind: DocKind::Agent,
            title: "GoldFox".to_string(),
            body_preview: "agent task description".to_string(),
            context_snippet: String::new(),
            match_count: 0,
            full_body: None,
            rendered_body: None,
            score: None,
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            project_id: Some(1),
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
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
        render_detail(
            &mut frame,
            Rect::new(0, 0, 80, 30),
            Some(&entry),
            0,
            &[],
            None,
            None,
            None,
            true,
        );
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
        render_detail(
            &mut frame,
            Rect::new(0, 0, 80, 20),
            Some(&entry),
            0,
            &[],
            None,
            None,
            None,
            true,
        );
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
        render_detail(
            &mut frame,
            Rect::new(0, 0, 60, 10),
            None,
            0,
            &[],
            None,
            None,
            None,
            true,
        );
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

    // ── truncate_str UTF-8 safety ────────────────────────────────────

    #[test]
    fn truncate_str_ascii_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_ascii_exact_boundary() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_ascii_truncated() {
        let r = truncate_str("hello world", 6);
        assert_eq!(r.chars().count(), 6);
        assert!(r.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_str_zero_returns_empty() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn truncate_str_3byte_arrow_no_panic() {
        let s = "foo → bar → baz → qux";
        let r = truncate_str(s, 8);
        assert_eq!(r.chars().count(), 8);
        assert!(r.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_str_cjk_no_panic() {
        let s = "日本語テスト文字列";
        let r = truncate_str(s, 5);
        assert_eq!(r.chars().count(), 5);
        assert!(r.starts_with("日本語テ"));
    }

    #[test]
    fn truncate_str_emoji_no_panic() {
        let s = "🔥🚀💡🎯🏆😊";
        let r = truncate_str(s, 4);
        assert_eq!(r.chars().count(), 4);
        assert!(r.starts_with("🔥🚀💡"));
    }

    #[test]
    fn truncate_str_mixed_multibyte() {
        let s = "hello→world🔥test";
        for max in 1..=s.chars().count() {
            let r = truncate_str(s, max);
            assert!(
                r.chars().count() <= max,
                "max={max} got {}",
                r.chars().count()
            );
        }
    }

    #[test]
    fn truncate_str_empty_input() {
        assert_eq!(truncate_str("", 5), "");
    }

    #[test]
    fn query_lab_hidden_by_default() {
        let screen = SearchCockpitScreen::new();
        assert!(
            !screen.query_lab_visible,
            "query lab should be hidden by default"
        );
    }

    #[test]
    fn l_key_toggles_query_lab() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::ResultList;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // Press L to show query lab
        let l_key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('L')));
        screen.update(&l_key, &state);
        assert!(screen.query_lab_visible, "L should toggle query lab on");

        // Press L again to hide
        screen.update(&l_key, &state);
        assert!(!screen.query_lab_visible, "L should toggle query lab off");
    }

    #[test]
    fn l_key_works_in_facet_rail() {
        let mut screen = SearchCockpitScreen::new();
        screen.focus = Focus::FacetRail;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let l_key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('L')));
        screen.update(&l_key, &state);
        assert!(screen.query_lab_visible);
    }

    // ──────────────────────────────────────────────────────────────────
    // br-1xt0m.1.10.2: Explicit labels — no abbreviations or cryptic symbols
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn facet_labels_are_explicit_no_abbreviations() {
        // FieldScope labels should be self-descriptive
        assert_eq!(FieldScope::SubjectAndBody.label(), "Subject+Body");
        assert_eq!(FieldScope::SubjectOnly.label(), "Subject Only");
        assert_eq!(FieldScope::BodyOnly.label(), "Body Only");
    }

    #[test]
    fn ack_filter_labels_are_explicit() {
        assert_eq!(AckFilter::Any.label(), "Any");
        assert_eq!(AckFilter::Required.label(), "Required");
        assert_eq!(AckFilter::NotRequired.label(), "Not Required");
    }

    #[test]
    fn scope_mode_label_is_capitalized() {
        use mcp_agent_mail_db::search_recipes::ScopeMode;
        assert_eq!(ScopeMode::Global.label(), "Global");
        assert_eq!(ScopeMode::Project.label(), "Project");
        assert_eq!(ScopeMode::Product.label(), "Product");
    }

    // ──────────────────────────────────────────────────────────────────
    // br-1xt0m.1.10.3: Result/Inspector hierarchy and highlight strategy
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn result_row_has_styled_type_badge() {
        let entry = ResultEntry {
            id: 1,
            doc_kind: DocKind::Message,
            title: "Test".to_string(),
            body_preview: String::new(),
            context_snippet: String::new(),
            match_count: 0,
            full_body: None,
            rendered_body: None,
            score: None,
            importance: Some("urgent".to_string()),
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            project_id: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
        };
        let row = SearchResultRow {
            entry,
            highlight_needles: Arc::new(Vec::new()),
            sort_direction: SortDirection::NewestFirst,
        };
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(60, 1, &mut pool);
        row.render(Rect::new(0, 0, 60, 1), &mut frame, false, 0);
        let text = buffer_to_text(&frame.buffer);
        // Should contain type badge [M] and importance !!
        assert!(text.contains("[M]"), "row text: {text}");
        assert!(text.contains("!!"), "row text: {text}");
    }

    #[test]
    fn detail_inspector_shows_explicit_type_label() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(60, 25, &mut pool);
        let entry = make_msg_entry();
        render_detail(
            &mut frame,
            Rect::new(0, 0, 60, 25),
            Some(&entry),
            0,
            &[],
            None,
            None,
            None,
            true,
        );
        let text = buffer_to_text(&frame.buffer);
        // Should show "Message" not "Message" from Debug format
        assert!(text.contains("Message"), "detail text: {text}");
        // Should show full "Importance" label, not "Import."
        assert!(text.contains("Importance:"), "detail text: {text}");
    }

    #[test]
    fn detail_inspector_highlights_title() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(60, 25, &mut pool);
        let entry = make_msg_entry();
        let terms = vec![QueryTerm {
            text: "test".to_string(),
            kind: QueryTermKind::Word,
            negated: false,
        }];
        render_detail(
            &mut frame,
            Rect::new(0, 0, 60, 25),
            Some(&entry),
            0,
            &terms,
            None,
            None,
            None,
            true,
        );
        let text = buffer_to_text(&frame.buffer);
        assert!(text.contains("Title:"), "detail text: {text}");
        assert!(text.contains("Test subject"), "detail text: {text}");
    }

    // ── Screen logic, density heuristics, and failure paths (br-1xt0m.1.13.8) ──

    #[test]
    fn facet_slot_next_round_trips() {
        let start = FacetSlot::Scope;
        let mut slot = start;
        for _ in 0..8 {
            slot = slot.next();
        }
        assert_eq!(slot, start, "8 next() calls should round-trip");
    }

    #[test]
    fn facet_slot_prev_round_trips() {
        let start = FacetSlot::Scope;
        let mut slot = start;
        for _ in 0..8 {
            slot = slot.prev();
        }
        assert_eq!(slot, start, "8 prev() calls should round-trip");
    }

    #[test]
    fn facet_slot_next_prev_inverse() {
        for slot in [
            FacetSlot::Scope,
            FacetSlot::DocKind,
            FacetSlot::Importance,
            FacetSlot::AckStatus,
            FacetSlot::SortOrder,
            FacetSlot::FieldScope,
            FacetSlot::SearchMode,
            FacetSlot::Explain,
        ] {
            assert_eq!(slot.next().prev(), slot, "next().prev() for {slot:?}");
            assert_eq!(slot.prev().next(), slot, "prev().next() for {slot:?}");
        }
    }

    #[test]
    fn focus_starts_at_result_list() {
        let screen = SearchCockpitScreen::new();
        assert_eq!(screen.focus, Focus::ResultList);
    }

    #[test]
    fn dock_drag_starts_idle() {
        let screen = SearchCockpitScreen::new();
        assert_eq!(screen.dock_drag, DockDragState::Idle);
    }

    #[test]
    fn query_help_and_lab_hidden_by_default() {
        let screen = SearchCockpitScreen::new();
        assert!(!screen.query_help_visible);
        assert!(!screen.query_lab_visible);
    }
}
