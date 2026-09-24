//! Lexical response assembler: ranking, pagination, snippets, and explain
//!
//! Converts raw Tantivy search results into [`SearchResults`] with:
//! - Score-sorted hits with deterministic tie-breaking (by ID ascending)
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

/// A character whose lowercase representation has a different UTF-8 width.
struct LowercaseChange {
    lowered_start: usize,
    lowered_end: usize,
    original_start: usize,
    original_end: usize,
}

/// Lowercased search text with sparse corrections to original UTF-8 offsets.
/// Lowercasing can expand (`İ`) or shrink (`K`) the byte representation. Most
/// characters, including most non-ASCII text, need no correction entries.
struct LowercaseText {
    text: String,
    changes: Vec<LowercaseChange>,
}

impl LowercaseText {
    fn new(original: &str) -> Self {
        let text = original.to_lowercase();
        let mut changes = Vec::new();
        if !original.is_ascii() {
            let mut lowered_offset = 0;
            for (original_offset, ch) in original.char_indices() {
                // str::to_lowercase also handles contextual final sigma; its
                // two forms have the same UTF-8 width as char::to_lowercase.
                let lowered_len = ch.to_lowercase().map(char::len_utf8).sum::<usize>();
                let lowered_end = lowered_offset + lowered_len;
                if lowered_len != ch.len_utf8() {
                    changes.push(LowercaseChange {
                        lowered_start: lowered_offset,
                        lowered_end,
                        original_start: original_offset,
                        original_end: original_offset + ch.len_utf8(),
                    });
                }
                lowered_offset = lowered_end;
            }
            debug_assert_eq!(lowered_offset, text.len());
        }
        Self { text, changes }
    }

    /// A partial match within a lowercase expansion highlights the whole
    /// original character, never half of its UTF-8 representation. Between
    /// width-changing characters, offsets have a constant displacement.
    fn original_range(&self, start: usize, end: usize) -> (usize, usize) {
        if self.changes.is_empty() {
            return (start, end);
        }
        let first = self
            .changes
            .partition_point(|change| change.lowered_start <= start);
        let last = self
            .changes
            .partition_point(|change| change.lowered_start < end);
        let original_start = if first == 0 {
            start
        } else {
            let change = &self.changes[first - 1];
            if start < change.lowered_end {
                change.original_start
            } else {
                change.original_end + (start - change.lowered_end)
            }
        };
        let original_end = if last == 0 {
            end
        } else {
            let change = &self.changes[last - 1];
            if end <= change.lowered_end {
                change.original_end
            } else {
                change.original_end + (end - change.lowered_end)
            }
        };
        (original_start, original_end)
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

/// The complete ranking key must participate in collection, not just in a
/// sort after collection: discarded ties cannot be recovered by sorting.
#[cfg(feature = "tantivy-engine")]
#[derive(Debug, Clone, Copy)]
struct LexicalRank {
    score: f32,
    doc_id: i64,
}

#[cfg(feature = "tantivy-engine")]
impl PartialEq for LexicalRank {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

#[cfg(feature = "tantivy-engine")]
impl Eq for LexicalRank {}

#[cfg(feature = "tantivy-engine")]
impl PartialOrd for LexicalRank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(feature = "tantivy-engine")]
impl Ord for LexicalRank {
    /// Higher score ranks first; among equal scores the LOWER document id
    /// ranks first. That is the (score desc, id asc) order the SQL planner
    /// (`m.id ASC`) and search_service's cursor pagination use: a page
    /// boundary that selected the highest tied ids instead made the next
    /// cursor page skip every remaining tie (br-t31jg).
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.doc_id.cmp(&self.doc_id))
    }
}

#[cfg(feature = "tantivy-engine")]
type RankedDocuments = Vec<(LexicalRank, tantivy::DocAddress)>;

#[cfg(feature = "tantivy-engine")]
#[derive(Clone, Copy)]
enum LexicalPagination<'a> {
    Offset(usize),
    Cursor(Option<&'a crate::search_planner::SearchCursor>),
}

/// Refresh a surviving boundary against the same filtered snapshot as its page.
/// BM25 scores can change after unrelated messages are indexed; the boundary's
/// identity still marks where to continue. If it was removed or no longer
/// matches the query, retain the encoded score/ID boundary.
#[cfg(feature = "tantivy-engine")]
fn current_cursor_boundary(
    searcher: &tantivy::Searcher,
    query: &dyn Query,
    handles: &FieldHandles,
    cursor: &crate::search_planner::SearchCursor,
) -> tantivy::Result<(f64, i64)> {
    use tantivy::query::{BooleanQuery, EnableScoring, Occur, TermQuery};
    use tantivy::schema::IndexRecordOption;

    let Ok(id) = u64::try_from(cursor.id) else {
        return Ok((cursor.score, cursor.id));
    };
    let identity_query = TermQuery::new(
        tantivy::Term::from_field_u64(handles.id, id),
        IndexRecordOption::Basic,
    );
    // IDs may also occur on another document kind in the unified index. Only
    // a boundary that matches the original query and all its filters counts.
    let boundary_query = BooleanQuery::new(vec![
        (Occur::Must, Box::new(identity_query)),
        (Occur::Must, query.box_clone()),
    ]);
    let addresses = searcher.search(&boundary_query, &TopDocs::with_limit(1))?;
    let Some((_, address)) = addresses.first() else {
        return Ok((cursor.score, cursor.id));
    };
    // Read the original query's score, without the extra identity term used
    // to locate this document and without assembling an explanation tree.
    let weight = query.weight(EnableScoring::enabled_from_searcher(searcher))?;
    let mut scorer = weight.scorer(searcher.segment_reader(address.segment_ord), 1.0)?;
    let score = if tantivy::DocSet::seek(scorer.as_mut(), address.doc_id) == address.doc_id {
        f64::from(scorer.score())
    } else {
        cursor.score
    };
    Ok((score, cursor.id))
}

#[cfg(feature = "tantivy-engine")]
fn collect_ranked_page(
    searcher: &tantivy::Searcher,
    query: &dyn Query,
    handles: &FieldHandles,
    limit: usize,
    pagination: LexicalPagination<'_>,
) -> tantivy::Result<(usize, RankedDocuments)> {
    let (offset, boundary) = match pagination {
        LexicalPagination::Offset(offset) => (offset, None),
        LexicalPagination::Cursor(cursor) => (
            0,
            cursor
                .map(|cursor| current_cursor_boundary(searcher, query, handles, cursor))
                .transpose()?,
        ),
    };
    let num_docs = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
    if limit == 0 || offset >= num_docs {
        // Preserve the exact matching count, including for count-only queries
        // and offsets far beyond the index, without allocating a top-K heap.
        return searcher
            .search(query, &Count)
            .map(|count| (count, Vec::new()));
    }

    // Bound offset + limit by the immutable snapshot's size before Tantivy
    // allocates its collector. In particular, usize::MAX is not a heap size.
    let page_limit = limit.min(num_docs - offset);
    let id_field = searcher.schema().get_field_name(handles.id).to_string();
    let id_columns = searcher
        .segment_readers()
        .iter()
        .map(|segment| {
            segment
                .fast_fields()
                .u64(&id_field)
                .map(|column| (segment.segment_id(), column))
        })
        .collect::<tantivy::Result<HashMap<_, _>>>()?;

    let collector = TopDocs::with_limit(page_limit)
        .and_offset(offset)
        .tweak_score(move |segment: &tantivy::SegmentReader| {
            // These are precisely the segments of this immutable searcher;
            // opening a missing or invalid fast field already returned Err.
            let ids = id_columns[&segment.segment_id()].clone();
            move |doc: tantivy::DocId, score: tantivy::Score| {
                // Match build_hit's representation of the stored document ID.
                #[allow(clippy::cast_possible_wrap)]
                let doc_id = ids.first(doc).unwrap_or(0) as i64;
                if boundary.is_some_and(|(boundary_score, boundary_id)| {
                    let order = f64::from(score).total_cmp(&boundary_score);
                    order.is_gt() || (order.is_eq() && doc_id <= boundary_id)
                }) {
                    return None;
                }
                Some(LexicalRank { score, doc_id })
            }
        });
    let (count, ranked) = searcher.search(query, &(Count, collector))?;
    // None sorts below every eligible rank. A short last page may retain some
    // excluded documents in the bounded heap; never hydrate or return them.
    Ok((
        count,
        ranked
            .into_iter()
            .filter_map(|(rank, address)| rank.map(|rank| (rank, address)))
            .collect(),
    ))
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
    execute_search_page(
        index,
        query,
        handles,
        query_terms,
        limit,
        LexicalPagination::Offset(offset),
        explain,
        config,
    )
}

/// Collect a planner page in score-descending, ID-ascending order. The cursor
/// boundary is applied during collection, before top-K truncation, so page
/// depth does not require retaining an ever-growing prefix of the corpus.
#[cfg(feature = "tantivy-engine")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_search_with_cursor(
    index: &Index,
    query: &dyn Query,
    handles: &FieldHandles,
    query_terms: &[String],
    limit: usize,
    cursor: Option<&crate::search_planner::SearchCursor>,
    explain: bool,
    config: &ResponseConfig,
) -> SearchResults {
    execute_search_page(
        index,
        query,
        handles,
        query_terms,
        limit,
        LexicalPagination::Cursor(cursor),
        explain,
        config,
    )
}

#[cfg(feature = "tantivy-engine")]
#[allow(clippy::too_many_arguments)]
fn execute_search_page(
    index: &Index,
    query: &dyn Query,
    handles: &FieldHandles,
    query_terms: &[String],
    limit: usize,
    pagination: LexicalPagination<'_>,
    explain: bool,
    config: &ResponseConfig,
) -> SearchResults {
    let start = Instant::now();

    let Ok(reader) = manual_index_reader(index) else {
        return SearchResults::empty(SearchMode::Lexical, start.elapsed());
    };
    let searcher = reader.searcher();
    let Ok((total_count, top_docs)) =
        collect_ranked_page(&searcher, query, handles, limit, pagination)
    else {
        return SearchResults::empty(SearchMode::Lexical, start.elapsed());
    };

    let composer_config = ExplainComposerConfig {
        verbosity: config.explain_verbosity,
        max_factors_per_stage: config.explain_max_factors,
    };
    let mut hits = Vec::with_capacity(top_docs.len());
    let mut explanations = Vec::new();

    // The collector has already ranked and paginated using the complete key.
    // Do not load bodies, generate snippets, or explain skipped documents.
    for (rank, doc_addr) in top_docs {
        let doc: TantivyDocument = match searcher.doc(doc_addr) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let hit = build_hit(&doc, handles, rank.score, query_terms, config);
        if explain {
            explanations.push(build_explanation(
                &hit,
                rank.score,
                query_terms,
                &composer_config,
            ));
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
            assert_eq!(
                snap_to_word_start(&text, text.find("NEEDLE").unwrap() - 2),
                "prefix".len() + separator.len_utf8()
            );
        }
    }

    #[test]
    fn highlights_map_length_changing_lowercase_to_original() {
        for prefix in ["İ", "\u{212a}", "İ\u{212a}", "\u{212a}İ\u{212a}İ"] {
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
        let text = "İ \u{212a}";
        let ranges = find_highlights(
            text,
            "body",
            &["i".to_string(), "\u{0307}".to_string(), "k".to_string()],
        );
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[0].start..ranges[0].end], "İ");
        assert_eq!(&text[ranges[1].start..ranges[1].end], "\u{212a}");
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
        for prefix in ["İ".repeat(500), "\u{212a}".repeat(500)] {
            let text = format!("{prefix} NEEDLE tail");
            let snippet = generate_snippet(&text, &["needle".into()]).unwrap();
            assert!(snippet.contains("NEEDLE"));
        }
    }

    #[test]
    fn snippet_marks_truncated_overlong_match() {
        let text = "x".repeat(300);
        let snippet = generate_snippet_with_limit(&text, std::slice::from_ref(&text), 20).unwrap();
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
        for text in ["İ\u{212a} 猫 犬 鳥", "ΑΒΓ ΣΟΣ needle", "🦀\u{2003}é NEEDLE"] {
            for term in text.split_whitespace() {
                let terms = [term.to_string()];
                let snippet = generate_snippet(text, &terms).unwrap();
                assert!(snippet.contains(term));
                let ranges = find_highlights(text, "body", &terms);
                assert!(
                    ranges
                        .iter()
                        .any(|range| &text[range.start..range.end] == term)
                );
            }
        }
    }

    #[test]
    fn lowercase_offset_map_scales_with_width_changes_not_message_length() {
        let text = "Résumé ΣΟΣ 🦀 猫 — ordinary text ".repeat(10_000);
        assert!(LowercaseText::new(&text).changes.is_empty());
        let mixed = format!("{text}İ\u{212a}{text}");
        assert_eq!(LowercaseText::new(&mixed).changes.len(), 2);
    }

    #[test]
    fn compact_lowercase_map_matches_dense_reference_at_all_character_boundaries() {
        for text in [
            "İ\u{212a}ẞȺȾ NEEDLE 猫",
            "plain 🦀 É ΟΣ text",
            "İİ\u{212a}\u{212a}\u{0307}İ\u{212a} end",
            "\u{212a}İ\u{212a}İ\u{212a}İ",
        ] {
            let lowered = LowercaseText::new(text);
            let mut dense = Vec::new();
            let mut lower_offset = 0;
            for (original_offset, ch) in text.char_indices() {
                dense.push((lower_offset, original_offset));
                lower_offset += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            }
            dense.push((lowered.text.len(), text.len()));
            let positions: Vec<_> = lowered
                .text
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(std::iter::once(lowered.text.len()))
                .collect();
            for (start_index, &start) in positions.iter().enumerate() {
                for &end in &positions[start_index + 1..] {
                    let first = dense.partition_point(|&(offset, _)| offset <= start) - 1;
                    let last = dense.partition_point(|&(offset, _)| offset < end);
                    let actual = lowered.original_range(start, end);
                    assert_eq!(actual, (dense[first].1, dense[last].1));
                    assert!(text.is_char_boundary(actual.0));
                    assert!(text.is_char_boundary(actual.1));
                }
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
            assert_eq!(results.hits[0].doc_id, 3);
        }

        #[test]
        fn execute_search_offset_applies_after_stable_tie_break() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 2, 1, false, &config);
            let ids: Vec<i64> = results.hits.iter().map(|hit| hit.doc_id).collect();
            assert_eq!(results.total_count, 3);
            assert_eq!(ids, vec![2, 3]);
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
            // AllQuery gives same score to all docs — tie-breaking by ID asc,
            // the order the SQL planner and cursor pagination use.
            let config = ResponseConfig::default();
            let results = execute_search(&index, &AllQuery, &handles, &[], 100, 0, false, &config);
            // After tie-breaking: IDs should be in ascending order
            for window in results.hits.windows(2) {
                if (window[0].score - window[1].score).abs() < f64::EPSILON {
                    assert!(
                        window[0].doc_id <= window[1].doc_id,
                        "Expected {} <= {} for tie-breaking",
                        window[0].doc_id,
                        window[1].doc_id
                    );
                }
            }
        }

        #[test]
        fn single_result_pages_do_not_repeat_or_omit_tied_documents() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let mut ids = Vec::new();
            for offset in 0..3 {
                let results =
                    execute_search(&index, &AllQuery, &handles, &[], 1, offset, true, &config);
                assert_eq!(results.total_count, 3);
                assert_eq!(results.hits.len(), 1);
                assert_eq!(results.explain.as_ref().unwrap().hits.len(), 1);
                ids.push(results.hits[0].doc_id);
            }
            assert_eq!(ids, vec![1, 2, 3]);
        }

        #[test]
        fn tied_pages_are_stable_across_segments_and_insertion_order() {
            let (schema, handles) = build_schema();
            let index = Index::create_in_ram(schema);
            register_tokenizer(&index);
            let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
            writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
            let ids = [12u64, 4, 8, 1, 15, 11, 3, 14, 9, 2, 7, 13, 6, 10, 5];
            for batch in ids.chunks(5) {
                for &id in batch {
                    writer
                        .add_document(doc!(
                            handles.id => id,
                            handles.doc_kind => "message",
                            handles.body => "same matching text"
                        ))
                        .unwrap();
                }
                writer.commit().unwrap();
            }
            let reader = manual_index_reader(&index).unwrap();
            assert_eq!(reader.searcher().segment_readers().len(), 3);

            let config = ResponseConfig::default();
            let mut paged_ids = Vec::new();
            for offset in (0..15).step_by(2) {
                let results =
                    execute_search(&index, &AllQuery, &handles, &[], 2, offset, false, &config);
                assert_eq!(results.total_count, 15);
                paged_ids.extend(results.hits.iter().map(|hit| hit.doc_id));
            }
            assert_eq!(paged_ids, (1i64..=15).collect::<Vec<_>>());
        }

        #[test]
        fn cursor_pages_exhaust_tied_corpus_beyond_candidate_prefix() {
            use crate::search_planner::SearchCursor;

            let (schema, handles) = build_schema();
            let index = Index::create_in_ram(schema);
            register_tokenizer(&index);
            let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
            writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
            // Reverse insertion order and independent segments must not affect
            // the score/ID boundary, even after passing the old 64-hit prefix.
            for segment in (0..3_u64).rev() {
                for id in (segment * 32 + 1..=segment * 32 + 32).rev() {
                    writer
                        .add_document(doc!(
                            handles.id => id,
                            handles.doc_kind => "message",
                            handles.body => "same matching text"
                        ))
                        .unwrap();
                }
                writer.commit().unwrap();
            }
            assert_eq!(
                manual_index_reader(&index)
                    .unwrap()
                    .searcher()
                    .segment_readers()
                    .len(),
                3
            );

            let config = ResponseConfig::default();
            for limit in [1, 3, 7] {
                let mut cursor = None;
                let mut ids = Vec::new();
                for _ in 0..=96 {
                    let page = execute_search_with_cursor(
                        &index,
                        &AllQuery,
                        &handles,
                        &[],
                        limit,
                        cursor.as_ref(),
                        true,
                        &config,
                    );
                    assert_eq!(page.total_count, 96);
                    assert_eq!(page.explain.as_ref().unwrap().hits.len(), page.hits.len());
                    assert!(page.hits.len() <= limit);
                    ids.extend(page.hits.iter().map(|hit| hit.doc_id));
                    let Some(last) = page.hits.last() else { break };
                    cursor = Some(SearchCursor {
                        score: last.score,
                        id: last.doc_id,
                    });
                }
                assert_eq!(ids, (1..=96).collect::<Vec<_>>(), "page size {limit}");
            }
        }

        #[test]
        fn cursor_page_continues_after_deleted_boundary() {
            use crate::search_planner::SearchCursor;

            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let first = execute_search_with_cursor(
                &index,
                &AllQuery,
                &handles,
                &[],
                2,
                None,
                false,
                &config,
            );
            assert_eq!(
                first.hits.iter().map(|hit| hit.doc_id).collect::<Vec<_>>(),
                vec![1, 2]
            );
            let last = first.hits.last().unwrap();
            let cursor = SearchCursor {
                score: last.score,
                id: last.doc_id,
            };
            let mut writer = index.writer::<TantivyDocument>(15_000_000).unwrap();
            writer.delete_term(tantivy::Term::from_field_u64(handles.id, 2));
            writer.commit().unwrap();

            let next = execute_search_with_cursor(
                &index,
                &AllQuery,
                &handles,
                &[],
                2,
                Some(&cursor),
                false,
                &config,
            );
            assert_eq!(
                next.hits.iter().map(|hit| hit.doc_id).collect::<Vec<_>>(),
                vec![3]
            );
        }

        #[test]
        fn cursor_page_refreshes_boundary_score_after_corpus_growth() {
            use crate::search_planner::SearchCursor;

            let (schema, handles) = build_schema();
            let index = Index::create_in_ram(schema);
            register_tokenizer(&index);
            let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
            for id in 1..=3_u64 {
                writer
                    .add_document(doc!(handles.id => id, handles.body => "needle shared"))
                    .unwrap();
            }
            writer.commit().unwrap();
            let query = QueryParser::for_index(&index, vec![handles.body])
                .parse_query("needle")
                .unwrap();
            let config = ResponseConfig::default();
            let first =
                execute_search_with_cursor(&index, &*query, &handles, &[], 2, None, false, &config);
            let last = first.hits.last().unwrap();
            assert_eq!(last.doc_id, 2);
            let cursor = SearchCursor {
                score: last.score,
                id: last.doc_id,
            };
            for id in 4..=20_u64 {
                writer
                    .add_document(doc!(handles.id => id, handles.body => "unrelated corpus growth"))
                    .unwrap();
            }
            writer.commit().unwrap();

            let next = execute_search_with_cursor(
                &index,
                &*query,
                &handles,
                &[],
                2,
                Some(&cursor),
                false,
                &config,
            );
            assert_eq!(
                next.hits.iter().map(|hit| hit.doc_id).collect::<Vec<_>>(),
                vec![3]
            );
            assert_ne!(
                next.hits[0].score.to_bits(),
                cursor.score.to_bits(),
                "fixture must change BM25 scores"
            );
        }

        #[test]
        fn relevance_precedes_id_and_original_scores_are_preserved() {
            use tantivy::query::{BooleanQuery, Occur, TermQuery};
            use tantivy::schema::IndexRecordOption;

            let (index, handles) = setup_index();
            let query = BooleanQuery::new(vec![
                (Occur::Should, Box::new(AllQuery)),
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        tantivy::Term::from_field_u64(handles.id, 1),
                        IndexRecordOption::Basic,
                    )),
                ),
            ]);
            let reader = manual_index_reader(&index).unwrap();
            let searcher = reader.searcher();
            let baseline = searcher
                .search(&query, &TopDocs::with_limit(3).order_by_score())
                .unwrap();
            let expected_scores: HashMap<_, _> = baseline
                .into_iter()
                .map(|(score, address)| {
                    let doc: TantivyDocument = searcher.doc(address).unwrap();
                    let id = doc.get_first(handles.id).unwrap().as_u64().unwrap();
                    (id, f64::from(score).to_bits())
                })
                .collect();
            let config = ResponseConfig::default();
            let mut ids = Vec::new();
            for offset in 0..3 {
                let result =
                    execute_search(&index, &query, &handles, &[], 1, offset, true, &config);
                let hit = &result.hits[0];
                let id = u64::try_from(hit.doc_id).unwrap();
                assert_eq!(hit.score.to_bits(), expected_scores[&id]);
                ids.push(hit.doc_id);
            }
            assert_eq!(ids, vec![1, 2, 3]);

            let mut cursor = None;
            let mut cursor_ids = Vec::new();
            for _ in 0..4 {
                let page = execute_search_with_cursor(
                    &index,
                    &query,
                    &handles,
                    &[],
                    1,
                    cursor.as_ref(),
                    false,
                    &config,
                );
                let Some(hit) = page.hits.first() else { break };
                let id = u64::try_from(hit.doc_id).unwrap();
                assert_eq!(hit.score.to_bits(), expected_scores[&id]);
                cursor_ids.push(hit.doc_id);
                cursor = Some(crate::search_planner::SearchCursor {
                    score: hit.score,
                    id: hit.doc_id,
                });
            }
            assert_eq!(cursor_ids, vec![1, 2, 3]);
        }

        #[test]
        fn zero_limit_and_out_of_range_offsets_keep_exact_counts() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            for (limit, offset) in [(0, 0), (0, usize::MAX), (10, 3), (1, usize::MAX)] {
                let result = execute_search(
                    &index,
                    &AllQuery,
                    &handles,
                    &[],
                    limit,
                    offset,
                    true,
                    &config,
                );
                assert_eq!(result.total_count, 3);
                assert!(result.hits.is_empty());
                assert!(result.explain.unwrap().hits.is_empty());
            }
            let parser = QueryParser::for_index(&index, vec![handles.subject, handles.body]);
            let query = parser.parse_query("migration").unwrap();
            let result = execute_search(
                &index,
                &*query,
                &handles,
                &[],
                0,
                usize::MAX,
                false,
                &config,
            );
            assert_eq!(result.total_count, 1);
        }

        #[test]
        fn oversized_limits_are_bounded_by_snapshot_size() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            for (offset, expected) in [(0, vec![1, 2, 3]), (1, vec![2, 3]), (2, vec![3])] {
                let result = execute_search(
                    &index,
                    &AllQuery,
                    &handles,
                    &[],
                    usize::MAX,
                    offset,
                    false,
                    &config,
                );
                assert_eq!(result.total_count, 3);
                let ids: Vec<_> = result.hits.iter().map(|hit| hit.doc_id).collect();
                assert_eq!(ids, expected);
            }
        }

        #[test]
        fn lexical_rank_uses_a_total_score_order_and_id_tiebreak() {
            let higher_score = LexicalRank {
                score: 2.0,
                doc_id: 1,
            };
            let higher_id = LexicalRank {
                score: 1.0,
                doc_id: 100,
            };
            let lower_id = LexicalRank {
                score: 1.0,
                doc_id: 2,
            };
            assert!(higher_score > lower_id);
            assert!(
                lower_id > higher_id,
                "among equal scores the lower id ranks first"
            );
            for score in [f32::NEG_INFINITY, -0.0, 0.0, f32::INFINITY, f32::NAN] {
                let rank = LexicalRank { score, doc_id: 1 };
                let equivalent = LexicalRank {
                    score,
                    doc_id: rank.doc_id,
                };
                assert_eq!(rank, equivalent);
                assert_eq!(
                    rank.partial_cmp(&equivalent),
                    Some(std::cmp::Ordering::Equal)
                );
            }
        }
    }
}
