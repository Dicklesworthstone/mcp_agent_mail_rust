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
use tantivy::collector::{Count, TopDocs};
#[cfg(feature = "tantivy-engine")]
use tantivy::query::Query;
#[cfg(feature = "tantivy-engine")]
use tantivy::schema::Value;
#[cfg(feature = "tantivy-engine")]
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument};

// Always available (used by find_highlights)
use mcp_agent_mail_core::HighlightRange;

#[cfg(feature = "tantivy-engine")]
use crate::tantivy_schema::FieldHandles;
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

// ── Snippet generation ──────────────────────────────────────────────────────

/// Maximum snippet length in characters
const SNIPPET_MAX_CHARS: usize = 200;

/// Context characters to include before/after a match in snippets
const SNIPPET_CONTEXT: usize = 40;

#[cfg(feature = "tantivy-engine")]
fn manual_index_reader(index: &Index) -> tantivy::Result<IndexReader> {
    index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
}

/// Lowercased search text with a map back to the original UTF-8 boundaries.
/// Lowercasing can expand (`İ`) or shrink (`K`) the byte representation, so
/// offsets in the lowercased string are not offsets in the stored message.
struct LowercaseText {
    text: String,
    boundaries: Vec<(usize, usize)>,
}

impl LowercaseText {
    fn new(original: &str) -> Self {
        let text = original.to_lowercase();
        let mut boundaries = Vec::new();
        if !original.is_ascii() {
            let mut lowered_offset = 0;
            for (original_offset, ch) in original.char_indices() {
                boundaries.push((lowered_offset, original_offset));
                // str::to_lowercase also handles contextual final sigma; its
                // two forms have the same UTF-8 width as char::to_lowercase.
                lowered_offset += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            }
            boundaries.push((text.len(), original.len()));
        }
        Self { text, boundaries }
    }

    /// A partial match within a lowercase expansion highlights the whole
    /// original character, never half of its UTF-8 representation.
    fn original_range(&self, start: usize, end: usize) -> (usize, usize) {
        if self.boundaries.is_empty() {
            return (start, end);
        }
        let first = self.boundaries.partition_point(|&(offset, _)| offset <= start) - 1;
        let last = self.boundaries.partition_point(|&(offset, _)| offset < end);
        (self.boundaries[first].1, self.boundaries[last].1)
    }
}

/// Generate a plain-text excerpt centered around the first matching term.
/// Highlight byte ranges are returned separately by [`find_highlights`].
#[must_use]
pub fn generate_snippet(text: &str, query_terms: &[String]) -> Option<String> {
    generate_snippet_with_limit(text, query_terms, SNIPPET_MAX_CHARS)
}

/// The character budget excludes the optional leading/trailing ellipses.
fn generate_snippet_with_limit(
    text: &str,
    query_terms: &[String],
    max_chars: usize,
) -> Option<String> {
    if text.is_empty() || query_terms.is_empty() || max_chars == 0 {
        return None;
    }
    let lowered = LowercaseText::new(text);
    let mut best_match = None;
    for term in query_terms {
        let term = term.to_lowercase();
        if term.is_empty() {
            continue;
        }
        if let Some(pos) = lowered.text.find(&term) {
            let range = lowered.original_range(pos, pos + term.len());
            if best_match.is_none_or(|(start, _)| range.0 < start) {
                best_match = Some(range);
            }
        }
    }
    let (match_start, match_end) = best_match?;
    let match_chars = text[match_start..match_end].chars().count();
    let context_before = SNIPPET_CONTEXT.min(max_chars.saturating_sub(match_chars));
    let start = retreat_chars(text, match_start, context_before);
    let word_start = snap_to_word_start(text, start);
    // A very long word before the match must not push the match out of the
    // snippet. Prefer the unsnapped boundary when the word exceeds the budget.
    let start = if text[word_start..match_end]
        .chars()
        .take(max_chars.saturating_add(1))
        .count()
        <= max_chars
    {
        word_start
    } else {
        start
    };
    let end = snap_to_word_end(text, advance_chars(text, match_end, SNIPPET_CONTEXT));
    let excerpt_end = end.min(advance_chars(text, start, max_chars));
    let mut snippet = String::new();
    if start > 0 {
        snippet.push_str("...");
    }
    snippet.push_str(&text[start..excerpt_end]);
    if excerpt_end < text.len() {
        snippet.push_str("...");
    }
    Some(snippet)
}

fn retreat_chars(text: &str, pos: usize, count: usize) -> usize {
    if count == 0 {
        return pos;
    }
    text[..pos]
        .char_indices()
        .rev()
        .nth(count - 1)
        .map_or(0, |(offset, _)| offset)
}

fn advance_chars(text: &str, pos: usize, count: usize) -> usize {
    text[pos..]
        .char_indices()
        .nth(count)
        .map_or(text.len(), |(offset, _)| pos + offset)
}

/// Find UTF-8 byte ranges in the original text, not its lowercased copy.
#[must_use]
pub fn find_highlights(
    text: &str,
    field_name: &str,
    query_terms: &[String],
) -> Vec<HighlightRange> {
    if text.is_empty() || query_terms.is_empty() {
        return Vec::new();
    }
    let lowered = LowercaseText::new(text);
    let mut ranges = Vec::new();
    for term in query_terms {
        let term = term.to_lowercase();
        if term.is_empty() {
            continue;
        }
        for (pos, matched) in lowered.text.match_indices(&term) {
            let (start, end) = lowered.original_range(pos, pos + matched.len());
            ranges.push(HighlightRange {
                field: field_name.to_string(),
                start,
                end,
            });
        }
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges.dedup_by(|a, b| a.start == b.start && a.end == b.end);
    ranges
}

/// Snap a byte position back to the start of the nearest word
fn snap_to_word_start(text: &str, pos: usize) -> usize {
    let safe_pos = floor_char_boundary(text, pos);
    if safe_pos == 0 || safe_pos >= text.len() {
        return safe_pos.min(text.len());
    }
    text[..safe_pos]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(offset, ch)| offset + ch.len_utf8())
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
    /// Maximum snippet length in characters, excluding ellipses.
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

    let Ok(reader) = manual_index_reader(index) else {
        return SearchResults::empty(SearchMode::Lexical, start.elapsed());
    };
    let searcher = reader.searcher();

    // Fetch more results than needed to handle offset + count total
    let fetch_limit = offset.saturating_add(limit).max(1);
    let Ok((total_count, top_docs)) = searcher.search(
        query,
        &(Count, TopDocs::with_limit(fetch_limit).order_by_score()),
    ) else {
        return SearchResults::empty(SearchMode::Lexical, start.elapsed());
    };

    // Build hits
    let mut ranked_hits = Vec::with_capacity(top_docs.len());
    let composer_config = ExplainComposerConfig {
        verbosity: config.explain_verbosity,
        max_factors_per_stage: config.explain_max_factors,
    };

    for (score, doc_addr) in top_docs {
        let doc: TantivyDocument = match searcher.doc(doc_addr) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let hit = build_hit(&doc, handles, score, query_terms, config);
        let explanation =
            explain.then(|| build_explanation(&hit, score, query_terms, &composer_config));
        ranked_hits.push((hit, explanation));
    }

    // Sort primarily by score descending, secondary by ID for determinism.
    ranked_hits.sort_by(|(a, _), (b, _)| {
        let score_cmp = b.score.total_cmp(&a.score);
        if score_cmp == std::cmp::Ordering::Equal {
            b.doc_id.cmp(&a.doc_id)
        } else {
            score_cmp
        }
    });

    if offset > 0 {
        ranked_hits.drain(0..offset.min(ranked_hits.len()));
    }
    if ranked_hits.len() > limit {
        ranked_hits.truncate(limit);
    }

    let mut hits = Vec::with_capacity(ranked_hits.len());
    let mut explanations = Vec::new();
    for (hit, explanation) in ranked_hits {
        if let Some(explanation) = explanation {
            explanations.push(explanation);
        }
        hits.push(hit);
    }

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
        generate_snippet_with_limit(text, query_terms, config.snippet_max_chars)
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

    #[test]
    fn snippet_handles_multibyte_whitespace() {
        for separator in ['\u{00a0}', '\u{2003}', '\u{2028}', '\u{3000}'] {
            let text = format!("prefix{separator}{} NEEDLE tail", "x".repeat(50));
            let snippet = generate_snippet(&text, &["needle".to_string()]).unwrap();
            assert!(snippet.contains("NEEDLE"), "{separator:?}: {snippet}");
            assert_eq!(snap_to_word_start(&text, text.find("NEEDLE").unwrap() - 2),
                "prefix".len() + separator.len_utf8());
        }
    }

    #[test]
    fn highlights_map_length_changing_lowercase_to_original() {
        for prefix in ["İ", "K", "İK", "KİKİ"] {
            let text = format!("{prefix} NEEDLE and NEEDLE");
            let ranges = find_highlights(&text, "body", &["needle".to_string()]);
            assert_eq!(ranges.len(), 2);
            for range in ranges {
                assert_eq!(&text[range.start..range.end], "NEEDLE");
            }
        }
    }

    #[test]
    fn highlights_expand_partial_lowercase_match_to_whole_character() {
        let text = "İ K";
        let ranges = find_highlights(
            text,
            "body",
            &["i".to_string(), "\u{0307}".to_string(), "k".to_string()],
        );
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[0].start..ranges[0].end], "İ");
        assert_eq!(&text[ranges[1].start..ranges[1].end], "K");
    }

    #[test]
    fn highlights_preserve_contextual_final_sigma() {
        let text = "ΟΣ NEEDLE";
        let ranges = find_highlights(text, "body", &["ος".to_string()]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].start..ranges[0].end], "ΟΣ");
    }

    #[test]
    fn highlights_deduplicate_repeated_terms() {
        let ranges = find_highlights("Needle", "body", &["needle".into(), "NEEDLE".into()]);
        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].start, ranges[0].end), (0, 6));
    }

    #[test]
    fn snippet_keeps_match_after_long_unbroken_word() {
        let text = format!("{}NEEDLE{}", "x".repeat(500), "y".repeat(500));
        let snippet = generate_snippet(&text, &["needle".into()]).unwrap();
        assert!(snippet.contains("NEEDLE"));
        assert!(snippet.starts_with("..."));
        assert!(snippet.ends_with("..."));
        assert!(snippet.chars().count() <= SNIPPET_MAX_CHARS + 6);
    }

    #[test]
    fn snippet_budget_counts_characters_not_bytes() {
        let snippet = generate_snippet_with_limit("猫犬鳥魚熊", &["鳥".into()], 3).unwrap();
        assert_eq!(snippet, "猫犬鳥...");
    }

    #[test]
    fn snippet_maps_anchor_after_many_lowercase_expansions() {
        for prefix in ["İ".repeat(500), "K".repeat(500)] {
            let text = format!("{prefix} NEEDLE tail");
            let snippet = generate_snippet(&text, &["needle".into()]).unwrap();
            assert!(snippet.contains("NEEDLE"));
        }
    }

    #[test]
    fn snippet_marks_truncated_overlong_match() {
        let text = "x".repeat(300);
        let snippet = generate_snippet_with_limit(&text, &[text.clone()], 20).unwrap();
        assert_eq!(snippet, format!("{}...", "x".repeat(20)));
    }

    #[test]
    fn snippet_zero_budget_and_empty_terms_have_no_excerpt() {
        assert!(generate_snippet_with_limit("needle", &["needle".into()], 0).is_none());
        assert!(generate_snippet("needle", &[String::new()]).is_none());
        assert!(find_highlights("needle", "body", &[String::new()]).is_empty());
    }

    #[test]
    fn snippet_and_highlights_cover_multilingual_matches() {
        for text in ["İK 猫 犬 鳥", "ΑΒΓ ΣΟΣ needle", "🦀\u{2003}é NEEDLE"] {
            for term in text.split_whitespace() {
                let terms = [term.to_string()];
                let snippet = generate_snippet(text, &terms).unwrap();
                assert!(snippet.contains(term));
                let ranges = find_highlights(text, "body", &terms);
                assert!(ranges.iter().any(|range| &text[range.start..range.end] == term));
            }
        }
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
            assert_eq!(results.total_count, 3);
            assert_eq!(results.hits.len(), 2);
        }

        #[test]
        fn execute_search_with_offset() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 100, 2, false, &config);
            assert_eq!(results.hits.len(), 1);
            assert_eq!(results.hits[0].doc_id, 1);
        }

        #[test]
        fn execute_search_offset_applies_after_stable_tie_break() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 2, 1, false, &config);
            let ids: Vec<i64> = results.hits.iter().map(|hit| hit.doc_id).collect();
            assert_eq!(results.total_count, 3);
            assert_eq!(ids, vec![2, 1]);
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
        fn build_hit_respects_snippet_character_budget() {
            let (_, handles) = setup_index();
            let document = doc!(
                handles.id => 10u64,
                handles.doc_kind => "message",
                handles.body => "needle abcdefgh"
            );
            let config = ResponseConfig {
                snippet_max_chars: 6,
                ..ResponseConfig::default()
            };
            let hit = build_hit(&document, &handles, 1.0, &["needle".into()], &config);
            assert_eq!(hit.snippet.as_deref(), Some("needle..."));
            assert_eq!(hit.highlight_ranges.len(), 1);
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
