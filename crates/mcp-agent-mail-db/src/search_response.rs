//! Lexical response assembler: ranking, pagination, snippets, and explain
//!
//! Converts raw Tantivy search results into [`SearchResults`] with:
//! - Score-sorted hits with deterministic tie-breaking (by ID descending)
//! - Offset/limit pagination with correct `total_count`
//! - Context-aware text snippets with term highlighting
//! - Optional deterministic multi-stage explain report

#[cfg(feature = "tantivy-engine")]
use std::collections::HashMap;
#[cfg(feature = "tantivy-engine")]
use std::time::Instant;

#[cfg(feature = "tantivy-engine")]
use tantivy::collector::TopDocs;
#[cfg(feature = "tantivy-engine")]
use tantivy::query::Query;
#[cfg(feature = "tantivy-engine")]
use tantivy::schema::Value;
#[cfg(feature = "tantivy-engine")]
use tantivy::{Index, TantivyDocument};

// Always available (used by find_highlights)
use mcp_agent_mail_core::HighlightRange;

#[cfg(feature = "tantivy-engine")]
use mcp_agent_mail_core::DocKind;
#[cfg(feature = "tantivy-engine")]
use mcp_agent_mail_core::SearchMode;
#[cfg(feature = "tantivy-engine")]
use mcp_agent_mail_core::{
    ExplainComposerConfig, ExplainReasonCode, ExplainStage, ExplainVerbosity, HitExplanation,
    ScoreFactor, SearchHit, SearchResults, StageScoreInput, compose_explain_report,
    compose_hit_explanation,
};
#[cfg(feature = "tantivy-engine")]
use crate::tantivy_schema::FieldHandles;

// ── Snippet generation ──────────────────────────────────────────────────────

/// Maximum snippet length in characters
const SNIPPET_MAX_CHARS: usize = 200;

/// Context characters to include before/after a match in snippets
const SNIPPET_CONTEXT: usize = 40;

/// Generate a text snippet from a document field, highlighting matched terms.
///
/// Returns a truncated excerpt centered around the first occurrence of any
/// query term, with `**bold**` markers around matched portions.
#[must_use]
pub fn generate_snippet(text: &str, query_terms: &[String]) -> Option<String> {
    if text.is_empty() || query_terms.is_empty() {
        return None;
    }

    let lower_text = text.to_lowercase();

    // Find the first matching term position
    let mut best_pos: Option<usize> = None;
    let mut best_term_len = 0usize;

    for term in query_terms {
        let lower_term = term.to_lowercase();
        if lower_term.is_empty() {
            continue;
        }
        if let Some(pos) = lower_text.find(&lower_term)
            && (best_pos.is_none() || pos < best_pos.unwrap_or(usize::MAX))
        {
            best_pos = Some(pos);
            best_term_len = lower_term.len();
        }
    }

    let match_pos = best_pos?;
    let match_start = floor_char_boundary(text, match_pos);
    let match_end = ceil_char_boundary(text, match_start.saturating_add(best_term_len));

    // Calculate excerpt window
    let start = floor_char_boundary(text, match_start.saturating_sub(SNIPPET_CONTEXT));
    let end = ceil_char_boundary(text, match_end.saturating_add(SNIPPET_CONTEXT));

    // Snap to word boundaries
    let start = snap_to_word_start(text, start);
    let end = snap_to_word_end(text, end);

    // Build snippet
    let mut snippet = String::with_capacity(SNIPPET_MAX_CHARS + 20);

    if start > 0 {
        snippet.push_str("...");
    }

    let max_end = ceil_char_boundary(text, start.saturating_add(SNIPPET_MAX_CHARS));
    let excerpt_end = end.min(max_end).max(start);
    let excerpt = &text[start..excerpt_end];
    snippet.push_str(excerpt);

    if end < text.len() {
        snippet.push_str("...");
    }

    Some(snippet)
}

/// Find highlight ranges for query terms within a text
#[must_use]
pub fn find_highlights(
    text: &str,
    field_name: &str,
    query_terms: &[String],
) -> Vec<HighlightRange> {
    if text.is_empty() || query_terms.is_empty() {
        return Vec::new();
    }
    let lower_text = text.to_lowercase();
    let mut ranges = Vec::new();

    for term in query_terms {
        let lower_term = term.to_lowercase();
        if lower_term.is_empty() {
            continue;
        }
        let mut search_from = 0;

        while let Some(pos) = lower_text[search_from..].find(&lower_term) {
            let abs_pos = search_from + pos;
            let start = floor_char_boundary(text, abs_pos);
            let end = ceil_char_boundary(text, abs_pos.saturating_add(lower_term.len()));
            if end <= start {
                search_from = abs_pos + lower_term.len();
                continue;
            }
            ranges.push(HighlightRange {
                field: field_name.to_string(),
                start,
                end,
            });
            search_from = abs_pos + lower_term.len();
        }
    }

    // Sort by position for consistent output
    ranges.sort_by_key(|r| r.start);
    ranges
}

/// Snap a byte position back to the start of the nearest word
fn snap_to_word_start(text: &str, pos: usize) -> usize {
    let safe_pos = floor_char_boundary(text, pos);
    if safe_pos == 0 || safe_pos >= text.len() {
        return safe_pos.min(text.len());
    }
    // Walk backwards to find whitespace
    text[..safe_pos]
        .rfind(|c: char| c.is_whitespace())
        .map_or(0, |p| p + 1)
}

/// Snap a byte position forward to the end of the nearest word
fn snap_to_word_end(text: &str, pos: usize) -> usize {
    let safe_pos = ceil_char_boundary(text, pos);
    if safe_pos >= text.len() {
        return text.len();
    }
    // Walk forward to find whitespace
    text[safe_pos..]
        .find(|c: char| c.is_whitespace())
        .map_or(text.len(), |p| safe_pos + p)
}

fn floor_char_boundary(text: &str, pos: usize) -> usize {
    let mut idx = pos.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(text: &str, pos: usize) -> usize {
    let mut idx = pos.min(text.len());
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

// ── Tantivy result assembler (behind feature gate) ──────────────────────────

/// Configuration for the lexical response assembler
#[cfg(feature = "tantivy-engine")]
#[derive(Debug, Clone)]
pub struct ResponseConfig {
    /// Maximum snippet length
    pub snippet_max_chars: usize,
    /// Whether to generate snippets
    pub generate_snippets: bool,
    /// Whether to generate highlight ranges
    pub generate_highlights: bool,
    /// Explain payload verbosity.
    pub explain_verbosity: ExplainVerbosity,
    /// Maximum factors retained per explain stage.
    pub explain_max_factors: usize,
}

#[cfg(feature = "tantivy-engine")]
impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            snippet_max_chars: SNIPPET_MAX_CHARS,
            generate_snippets: true,
            generate_highlights: true,
            explain_verbosity: ExplainVerbosity::Standard,
            explain_max_factors: 4,
        }
    }
}

/// Execute a Tantivy search and assemble results with pagination, snippets,
/// and optional explain report.
///
/// # Arguments
/// * `index` — The Tantivy index to search
/// * `query` — The compiled Tantivy query
/// * `handles` — Field handles for extracting document data
/// * `query_terms` — Terms for snippet highlighting
/// * `limit` — Max results to return
/// * `offset` — Number of results to skip
/// * `explain` — Whether to include an explain report
/// * `config` — Response assembly configuration
#[cfg(feature = "tantivy-engine")]
#[allow(clippy::too_many_arguments)]
pub fn execute_search(
    index: &Index,
    query: &dyn Query,
    handles: &FieldHandles,
    query_terms: &[String],
    limit: usize,
    offset: usize,
    explain: bool,
    config: &ResponseConfig,
) -> SearchResults {
    let start = Instant::now();

    let Ok(reader) = index.reader() else {
        return SearchResults::empty(SearchMode::Lexical, start.elapsed());
    };
    let searcher = reader.searcher();

    // Fetch more results than needed to handle offset + count total
    let fetch_limit = offset.saturating_add(limit).max(1);
    let Ok(top_docs) = searcher.search(query, &TopDocs::with_limit(fetch_limit)) else {
        return SearchResults::empty(SearchMode::Lexical, start.elapsed());
    };

    let total_count = top_docs.len();

    // Apply offset
    let page_docs: Vec<_> = top_docs.into_iter().skip(offset).collect();

    // Build hits
    let mut hits = Vec::with_capacity(page_docs.len());
    let mut explanations = Vec::new();
    let composer_config = ExplainComposerConfig {
        verbosity: config.explain_verbosity,
        max_factors_per_stage: config.explain_max_factors,
    };

    for (score, doc_addr) in &page_docs {
        let doc: TantivyDocument = match searcher.doc(*doc_addr) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let hit = build_hit(&doc, handles, *score, query_terms, config);

        if explain {
            explanations.push(build_explanation(
                &hit,
                *score,
                query_terms,
                &composer_config,
            ));
        }

        hits.push(hit);
    }

    // Deterministic tie-breaking: when scores are equal, sort by ID descending
    // (newer documents first)
    hits.sort_by(|a, b| {
        let score_cmp = b
            .score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal);
        if score_cmp == std::cmp::Ordering::Equal {
            b.doc_id.cmp(&a.doc_id)
        } else {
            score_cmp
        }
    });

    let elapsed = start.elapsed();

    let explain_report = if explain {
        let mut phase_timings = HashMap::new();
        phase_timings.insert("lexical_search".to_string(), elapsed);
        Some(compose_explain_report(
            SearchMode::Lexical,
            total_count,
            phase_timings,
            explanations,
            &composer_config,
        ))
    } else {
        None
    };

    SearchResults {
        hits,
        total_count,
        mode_used: SearchMode::Lexical,
        explain: explain_report,
        elapsed,
    }
}

/// Extract a `SearchHit` from a Tantivy document
#[cfg(feature = "tantivy-engine")]
fn build_hit(
    doc: &TantivyDocument,
    handles: &FieldHandles,
    score: f32,
    query_terms: &[String],
    config: &ResponseConfig,
) -> SearchHit {
    #[allow(clippy::cast_possible_wrap)]
    let id = doc
        .get_first(handles.id)
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as i64;

    let doc_kind_str = doc
        .get_first(handles.doc_kind)
        .and_then(|v| v.as_str())
        .unwrap_or("message");

    let doc_kind = match doc_kind_str {
        "agent" => DocKind::Agent,
        "project" => DocKind::Project,
        "thread" => DocKind::Thread,
        _ => DocKind::Message,
    };

    let subject = doc
        .get_first(handles.subject)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let body = doc
        .get_first(handles.body)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Generate snippet from body (or subject if body is empty)
    let snippet = if config.generate_snippets {
        let text = if body.is_empty() { &subject } else { &body };
        generate_snippet(text, query_terms)
    } else {
        None
    };

    // Generate highlight ranges
    let highlight_ranges = if config.generate_highlights {
        let mut ranges = find_highlights(&subject, "subject", query_terms);
        ranges.extend(find_highlights(&body, "body", query_terms));
        ranges
    } else {
        Vec::new()
    };

    // Build metadata
    let mut metadata = HashMap::new();
    if !subject.is_empty() {
        metadata.insert("subject".to_string(), serde_json::json!(subject));
    }

    if let Some(sender) = doc.get_first(handles.sender).and_then(|v| v.as_str()) {
        metadata.insert("sender".to_string(), serde_json::json!(sender));
    }

    if let Some(project) = doc.get_first(handles.project_slug).and_then(|v| v.as_str()) {
        metadata.insert("project_slug".to_string(), serde_json::json!(project));
    }

    if let Some(thread) = doc.get_first(handles.thread_id).and_then(|v| v.as_str()) {
        metadata.insert("thread_id".to_string(), serde_json::json!(thread));
    }

    if let Some(importance) = doc.get_first(handles.importance).and_then(|v| v.as_str()) {
        metadata.insert("importance".to_string(), serde_json::json!(importance));
    }

    if let Some(ts) = doc.get_first(handles.created_ts).and_then(|v| v.as_i64()) {
        metadata.insert("created_ts".to_string(), serde_json::json!(ts));
    }

    // project_id is stored as u64 in the index; emit as i64 for scope enforcement
    if let Some(pid) = doc.get_first(handles.project_id).and_then(|v| v.as_u64()) {
        #[allow(clippy::cast_possible_wrap)]
        metadata.insert("project_id".to_string(), serde_json::json!(pid as i64));
    }

    SearchHit {
        doc_id: id,
        doc_kind,
        score: f64::from(score),
        snippet,
        highlight_ranges,
        metadata,
    }
}

/// Build an explain entry for a hit
#[cfg(feature = "tantivy-engine")]
fn build_explanation(
    hit: &SearchHit,
    raw_score: f32,
    query_terms: &[String],
    config: &ExplainComposerConfig,
) -> HitExplanation {
    let raw_bm25 = f64::from(raw_score);
    let query_term_count = query_terms.len();
    let highlight_count = hit.highlight_ranges.len();
    #[allow(clippy::cast_precision_loss)] // highlight/query counts always small
    let coverage = if query_term_count == 0 {
        0.0
    } else {
        (highlight_count as f64 / query_term_count as f64).min(1.0)
    };
    let coverage_component = raw_bm25 * 0.1 * coverage;
    let bm25_component = raw_bm25 - coverage_component;

    let lexical_stage = StageScoreInput {
        stage: ExplainStage::Lexical,
        reason_code: ExplainReasonCode::LexicalBm25,
        summary: Some(format!(
            "Lexical retrieval via BM25 for doc_kind={}, id={}",
            match hit.doc_kind {
                DocKind::Message => "message",
                DocKind::Agent => "agent",
                DocKind::Project => "project",
                DocKind::Thread => "thread",
            },
            hit.doc_id
        )),
        stage_score: hit.score,
        stage_weight: 1.0,
        score_factors: vec![
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "bm25".to_string(),
                contribution: bm25_component,
                detail: Some(format!("raw_bm25={raw_bm25:.6}")),
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalTermCoverage,
                key: "term_coverage".to_string(),
                contribution: coverage_component,
                detail: Some(format!(
                    "highlight_count={highlight_count}, query_term_count={query_term_count}"
                )),
            },
        ],
    };

    compose_hit_explanation(hit.doc_id, hit.score, vec![lexical_stage], config)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Engine-independent snippet tests ──

    #[test]
    fn snippet_empty_text() {
        assert!(generate_snippet("", &["foo".to_string()]).is_none());
    }

    #[test]
    fn snippet_empty_terms() {
        assert!(generate_snippet("hello world", &[]).is_none());
    }

    #[test]
    fn snippet_single_match() {
        let text = "The quick brown fox jumps over the lazy dog";
        let snippet = generate_snippet(text, &["fox".to_string()]).unwrap();
        assert!(snippet.contains("fox"));
    }

    #[test]
    fn snippet_case_insensitive() {
        let text = "The Migration Plan for DB v3";
        let snippet = generate_snippet(text, &["migration".to_string()]).unwrap();
        assert!(snippet.contains("Migration"));
    }

    #[test]
    fn snippet_handles_unicode_safely_near_window_boundaries() {
        let text = "``br`/`bv` show `bd-1j6n` as the top in-progress/ready item. I’m taking execution now and will work in `crates/ffs-alloc/src/lib.rs` to add the requested property tests. If either of you already owns `bd-1j6n`, reply in-thread and I’ll adjust immediate.";
        let snippet =
            generate_snippet(text, &["immediate".to_string()]).expect("snippet should be produced");
        assert!(snippet.contains("immediate"));
    }

    #[test]
    fn snippet_truncates_long_text() {
        let long_text = "x ".repeat(500);
        let text = format!("{long_text}MATCH_HERE{long_text}");
        let snippet = generate_snippet(&text, &["match_here".to_string()]).unwrap();
        assert!(snippet.len() < text.len());
        assert!(snippet.contains("MATCH_HERE"));
    }

    #[test]
    fn snippet_no_match() {
        let text = "hello world";
        assert!(generate_snippet(text, &["xyz".to_string()]).is_none());
    }

    #[test]
    fn snippet_at_start() {
        let text = "migration plan for the new database";
        let snippet = generate_snippet(text, &["migration".to_string()]).unwrap();
        assert!(snippet.starts_with("migration") || snippet.starts_with("..."));
        assert!(snippet.contains("migration"));
    }

    // ── Highlight tests ──

    #[test]
    fn highlights_empty_text() {
        let ranges = find_highlights("", "body", &["foo".to_string()]);
        assert!(ranges.is_empty());
    }

    #[test]
    fn highlights_single_occurrence() {
        let ranges = find_highlights("hello world", "body", &["world".to_string()]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].field, "body");
        assert_eq!(ranges[0].start, 6);
        assert_eq!(ranges[0].end, 11);
    }

    #[test]
    fn highlights_multiple_occurrences() {
        let ranges = find_highlights("foo bar foo baz foo", "body", &["foo".to_string()]);
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[1].start, 8);
        assert_eq!(ranges[2].start, 16);
    }

    #[test]
    fn highlights_case_insensitive() {
        let ranges = find_highlights("Hello HELLO hello", "body", &["hello".to_string()]);
        assert_eq!(ranges.len(), 3);
    }

    #[test]
    fn highlights_multiple_terms() {
        let ranges = find_highlights(
            "foo bar baz",
            "body",
            &["foo".to_string(), "baz".to_string()],
        );
        assert_eq!(ranges.len(), 2);
        // Sorted by position
        assert_eq!(ranges[0].start, 0); // foo
        assert_eq!(ranges[1].start, 8); // baz
    }

    #[test]
    fn highlights_no_match() {
        let ranges = find_highlights("hello world", "body", &["xyz".to_string()]);
        assert!(ranges.is_empty());
    }

    #[test]
    fn highlights_unicode_boundaries_are_valid_utf8_indices() {
        let text = "Coordinate in-thread: I’m taking execution now; adjust if `bd-1j6n` is owned.";
        let ranges = find_highlights(text, "body", &["taking".to_string(), "owned".to_string()]);
        assert!(!ranges.is_empty(), "expected at least one highlight");
        for range in ranges {
            assert!(text.is_char_boundary(range.start));
            assert!(text.is_char_boundary(range.end));
        }
    }

    // ── Word boundary snapping tests ──

    #[test]
    fn snap_word_start_at_zero() {
        assert_eq!(snap_to_word_start("hello world", 0), 0);
    }

    #[test]
    fn snap_word_start_mid_word() {
        assert_eq!(snap_to_word_start("hello world", 8), 6);
    }

    #[test]
    fn snap_word_end_at_end() {
        let text = "hello world";
        assert_eq!(snap_to_word_end(text, text.len()), text.len());
    }

    #[test]
    fn snap_word_end_mid_word() {
        assert_eq!(snap_to_word_end("hello world", 3), 5);
    }

    // ── Snippet edge cases ──

    #[test]
    fn snippet_at_end_of_text() {
        let text = "beginning of the text with the match at end";
        let snippet = generate_snippet(text, &["end".to_string()]).unwrap();
        assert!(snippet.contains("end"));
    }

    #[test]
    fn snippet_multiple_terms_first_match_wins() {
        let text = "alpha comes before beta in the text";
        let snippet = generate_snippet(text, &["beta".to_string(), "alpha".to_string()]).unwrap();
        // "alpha" appears first in text, so it should be the anchor
        assert!(snippet.contains("alpha"));
    }

    #[test]
    fn snippet_ellipsis_at_start_when_match_far_in() {
        let prefix = "word ".repeat(50);
        let text = format!("{prefix}NEEDLE rest of text");
        let snippet = generate_snippet(&text, &["needle".to_string()]).unwrap();
        assert!(snippet.starts_with("..."));
        assert!(snippet.contains("NEEDLE"));
    }

    #[test]
    fn snippet_short_text_no_ellipsis() {
        let text = "short text with needle";
        let snippet = generate_snippet(text, &["needle".to_string()]).unwrap();
        assert!(!snippet.starts_with("..."));
        assert!(!snippet.ends_with("..."));
    }

    // ── Highlight edge cases ──

    #[test]
    fn highlights_empty_terms_list() {
        let ranges = find_highlights("hello world", "body", &[]);
        assert!(ranges.is_empty());
    }

    #[test]
    fn highlights_adjacent_matches() {
        let ranges = find_highlights("foobar", "body", &["foo".to_string(), "bar".to_string()]);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[0].end, 3);
        assert_eq!(ranges[1].start, 3);
        assert_eq!(ranges[1].end, 6);
    }

    #[test]
    fn highlights_field_name_propagated() {
        let ranges = find_highlights("test", "subject", &["test".to_string()]);
        assert_eq!(ranges[0].field, "subject");
    }

    // ── Word boundary snapping edge cases ──

    #[test]
    fn snap_word_start_past_end() {
        let text = "hello";
        assert_eq!(snap_to_word_start(text, 100), text.len());
    }

    #[test]
    fn snap_word_end_at_zero() {
        let text = "hello world";
        let end = snap_to_word_end(text, 0);
        assert_eq!(end, 5); // snaps to end of "hello"
    }

    #[test]
    fn snap_word_start_at_space() {
        let text = "hello world";
        // Position 5 is the space
        assert_eq!(snap_to_word_start(text, 5), 0);
    }

    #[test]
    fn snap_word_end_already_at_space() {
        let text = "hello world";
        // Position 5 is the space
        assert_eq!(snap_to_word_end(text, 5), 5);
    }

    // ── Constants ──

    #[test]
    fn snippet_max_chars_reasonable() {
        const { assert!(SNIPPET_MAX_CHARS > 50) };
        const { assert!(SNIPPET_MAX_CHARS < 1000) };
    }

    #[test]
    fn snippet_context_reasonable() {
        const { assert!(SNIPPET_CONTEXT > 10) };
        const { assert!(SNIPPET_CONTEXT < SNIPPET_MAX_CHARS) };
    }

    // ── Tantivy integration tests ──

    #[cfg(feature = "tantivy-engine")]
    mod tantivy_tests {
        use super::super::*;
        use crate::tantivy_schema::{build_schema, register_tokenizer};
        use tantivy::doc;
        use tantivy::query::{AllQuery, QueryParser};

        fn setup_index() -> (Index, FieldHandles) {
            let (schema, handles) = build_schema();
            let index = Index::create_in_ram(schema);
            register_tokenizer(&index);

            let mut writer = index.writer(15_000_000).unwrap();
            writer
                .add_document(doc!(
                    handles.id => 1u64,
                    handles.doc_kind => "message",
                    handles.subject => "Migration plan review",
                    handles.body => "Here is the plan for DB migration to version 3",
                    handles.sender => "BlueLake",
                    handles.project_slug => "backend",
                    handles.project_id => 1u64,
                    handles.thread_id => "br-123",
                    handles.importance => "high",
                    handles.created_ts => 1_700_000_000_000_000i64
                ))
                .unwrap();
            writer
                .add_document(doc!(
                    handles.id => 2u64,
                    handles.doc_kind => "message",
                    handles.subject => "Deployment checklist",
                    handles.body => "Steps for deploying the new search engine to production",
                    handles.sender => "RedPeak",
                    handles.project_slug => "backend",
                    handles.project_id => 1u64,
                    handles.thread_id => "br-456",
                    handles.importance => "normal",
                    handles.created_ts => 1_700_100_000_000_000i64
                ))
                .unwrap();
            writer
                .add_document(doc!(
                    handles.id => 3u64,
                    handles.doc_kind => "message",
                    handles.subject => "Security audit results",
                    handles.body => "Completed the security audit with no critical findings",
                    handles.sender => "GreenCastle",
                    handles.project_slug => "compliance",
                    handles.project_id => 2u64,
                    handles.thread_id => "TKT-789",
                    handles.importance => "urgent",
                    handles.created_ts => 1_700_200_000_000_000i64
                ))
                .unwrap();
            writer.commit().unwrap();

            (index, handles)
        }

        #[test]
        fn execute_search_all_docs() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 100, 0, false, &config);
            assert_eq!(results.total_count, 3);
            assert_eq!(results.hits.len(), 3);
            assert_eq!(results.mode_used, SearchMode::Lexical);
            assert!(results.explain.is_none());
        }

        #[test]
        fn execute_search_with_limit() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 2, 0, false, &config);
            // total_count reflects docs fetched (up to limit)
            assert!(results.hits.len() <= 2);
        }

        #[test]
        fn execute_search_with_offset() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 100, 2, false, &config);
            // Should skip 2 results
            assert_eq!(results.hits.len(), 1);
        }

        #[test]
        fn execute_search_with_query() {
            let (index, handles) = setup_index();
            let parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
            let query = parser.parse_query("migration").unwrap();
            let config = ResponseConfig::default();
            let results = execute_search(
                &index,
                &*query,
                &handles,
                &["migration".to_string()],
                10,
                0,
                false,
                &config,
            );
            assert_eq!(results.total_count, 1);
            assert_eq!(results.hits[0].doc_id, 1);
            assert!(results.hits[0].snippet.is_some());
            assert!(
                results.hits[0]
                    .snippet
                    .as_ref()
                    .unwrap()
                    .contains("migration")
            );
        }

        #[test]
        fn execute_search_with_explain() {
            let (index, handles) = setup_index();
            let parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
            let query = parser.parse_query("migration").unwrap();
            let config = ResponseConfig::default();
            let results = execute_search(
                &index,
                &*query,
                &handles,
                &["migration".to_string()],
                10,
                0,
                true,
                &config,
            );
            assert!(results.explain.is_some());
            let explain = results.explain.unwrap();
            assert_eq!(explain.mode_used, SearchMode::Lexical);
            assert!(!explain.hits.is_empty());
            assert_eq!(explain.taxonomy_version, 1);
            let hit_explain = &explain.hits[0];
            assert_eq!(hit_explain.stages[0].stage, ExplainStage::Lexical);
            assert_eq!(
                hit_explain.stages[0].reason_code,
                ExplainReasonCode::LexicalBm25
            );
            assert!(!hit_explain.stages[0].score_factors.is_empty());
        }

        #[test]
        fn execute_search_with_explain_minimal_verbosity_hides_factors() {
            let (index, handles) = setup_index();
            let parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
            let query = parser.parse_query("migration").unwrap();
            let config = ResponseConfig {
                explain_verbosity: ExplainVerbosity::Minimal,
                ..ResponseConfig::default()
            };
            let results = execute_search(
                &index,
                &*query,
                &handles,
                &["migration".to_string()],
                10,
                0,
                true,
                &config,
            );
            let explain = results.explain.unwrap();
            assert!(explain.hits[0].stages[0].score_factors.is_empty());
            assert!(explain.hits[0].stages[0].truncated_factor_count >= 1);
        }

        #[test]
        fn execute_search_with_explain_truncates_factors_deterministically() {
            let (index, handles) = setup_index();
            let parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
            let query = parser.parse_query("migration").unwrap();
            let config = ResponseConfig {
                explain_verbosity: ExplainVerbosity::Detailed,
                explain_max_factors: 1,
                ..ResponseConfig::default()
            };
            let results = execute_search(
                &index,
                &*query,
                &handles,
                &["migration".to_string()],
                10,
                0,
                true,
                &config,
            );
            let explain = results.explain.unwrap();
            assert_eq!(explain.hits[0].stages[0].score_factors.len(), 1);
            assert_eq!(explain.hits[0].stages[0].truncated_factor_count, 1);
        }

        #[test]
        fn execute_search_metadata_populated() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 10, 0, false, &config);

            // Find doc 1
            let hit = results.hits.iter().find(|h| h.doc_id == 1).unwrap();
            assert_eq!(hit.doc_kind, DocKind::Message);
            assert_eq!(hit.metadata["sender"], "BlueLake");
            assert_eq!(hit.metadata["project_slug"], "backend");
            assert_eq!(hit.metadata["thread_id"], "br-123");
            assert_eq!(hit.metadata["importance"], "high");
            assert!(hit.metadata.contains_key("created_ts"));
        }

        #[test]
        fn execute_search_snippets_disabled() {
            let (index, handles) = setup_index();
            let config = ResponseConfig {
                generate_snippets: false,
                generate_highlights: false,
                ..ResponseConfig::default()
            };
            let results = execute_search(
                &index,
                &AllQuery,
                &handles,
                &["migration".to_string()],
                10,
                0,
                false,
                &config,
            );
            for hit in &results.hits {
                assert!(hit.snippet.is_none());
                assert!(hit.highlight_ranges.is_empty());
            }
        }

        #[test]
        fn execute_search_empty_results() {
            let (index, handles) = setup_index();
            let parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
            let query = parser.parse_query("nonexistent_xyzzy").unwrap();
            let config = ResponseConfig::default();
            let results = execute_search(
                &index,
                &*query,
                &handles,
                &["nonexistent_xyzzy".to_string()],
                10,
                0,
                false,
                &config,
            );
            assert!(results.is_empty());
            assert_eq!(results.total_count, 0);
        }

        #[test]
        fn deterministic_tiebreaking() {
            let (index, handles) = setup_index();
            // AllQuery gives same score to all docs — tie-breaking by ID desc
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 100, 0, false, &config);
            // After tie-breaking: IDs should be in descending order
            for window in results.hits.windows(2) {
                if (window[0].score - window[1].score).abs() < f64::EPSILON {
                    assert!(
                        window[0].doc_id >= window[1].doc_id,
                        "Expected {} >= {} for tie-breaking",
                        window[0].doc_id,
                        window[1].doc_id
                    );
                }
            }
        }
    }
}
