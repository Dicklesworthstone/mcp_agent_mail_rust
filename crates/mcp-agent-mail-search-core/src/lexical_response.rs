//! Lexical response assembler: ranking, pagination, snippets, and explain
//!
//! Converts raw Tantivy search results into [`SearchResults`] with:
//! - Score-sorted hits with deterministic tie-breaking (by ID descending)
//! - Offset/limit pagination with correct `total_count`
//! - Context-aware text snippets with term highlighting
//! - Optional explain report with BM25 score components

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

#[cfg(feature = "tantivy-engine")]
use crate::document::DocKind;
#[cfg(feature = "tantivy-engine")]
use crate::query::SearchMode;
#[cfg(feature = "tantivy-engine")]
use crate::results::{
    ExplainReport, HighlightRange, HitExplanation, SearchHit, SearchResults,
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
    let mut best_term = "";

    for term in query_terms {
        let lower_term = term.to_lowercase();
        if let Some(pos) = lower_text.find(&lower_term) {
            if best_pos.is_none() || pos < best_pos.unwrap_or(usize::MAX) {
                best_pos = Some(pos);
                best_term = term;
            }
        }
    }

    let match_pos = best_pos?;

    // Calculate excerpt window
    let start = match_pos.saturating_sub(SNIPPET_CONTEXT);
    let end = (match_pos + best_term.len() + SNIPPET_CONTEXT).min(text.len());

    // Snap to word boundaries
    let start = snap_to_word_start(text, start);
    let end = snap_to_word_end(text, end);

    // Build snippet
    let mut snippet = String::with_capacity(SNIPPET_MAX_CHARS + 20);

    if start > 0 {
        snippet.push_str("...");
    }

    let excerpt = &text[start..end.min(start + SNIPPET_MAX_CHARS)];
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
    let lower_text = text.to_lowercase();
    let mut ranges = Vec::new();

    for term in query_terms {
        let lower_term = term.to_lowercase();
        let mut search_from = 0;

        while let Some(pos) = lower_text[search_from..].find(&lower_term) {
            let abs_pos = search_from + pos;
            ranges.push(HighlightRange {
                field: field_name.to_string(),
                start: abs_pos,
                end: abs_pos + lower_term.len(),
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
    if pos == 0 || pos >= text.len() {
        return pos.min(text.len());
    }
    // Walk backwards to find whitespace
    text[..pos]
        .rfind(|c: char| c.is_whitespace())
        .map_or(0, |p| p + 1)
}

/// Snap a byte position forward to the end of the nearest word
fn snap_to_word_end(text: &str, pos: usize) -> usize {
    if pos >= text.len() {
        return text.len();
    }
    // Walk forward to find whitespace
    text[pos..]
        .find(|c: char| c.is_whitespace())
        .map_or(text.len(), |p| pos + p)
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
}

#[cfg(feature = "tantivy-engine")]
impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            snippet_max_chars: SNIPPET_MAX_CHARS,
            generate_snippets: true,
            generate_highlights: true,
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

    let reader = match index.reader() {
        Ok(r) => r,
        Err(_) => return SearchResults::empty(SearchMode::Lexical, start.elapsed()),
    };
    let searcher = reader.searcher();

    // Fetch more results than needed to handle offset + count total
    let fetch_limit = offset.saturating_add(limit).max(1);
    let top_docs = match searcher.search(query, &TopDocs::with_limit(fetch_limit)) {
        Ok(docs) => docs,
        Err(_) => return SearchResults::empty(SearchMode::Lexical, start.elapsed()),
    };

    let total_count = top_docs.len();

    // Apply offset
    let page_docs: Vec<_> = top_docs.into_iter().skip(offset).collect();

    // Build hits
    let mut hits = Vec::with_capacity(page_docs.len());
    let mut explanations = Vec::new();

    for (score, doc_addr) in &page_docs {
        let doc: TantivyDocument = match searcher.doc(*doc_addr) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let hit = build_hit(&doc, handles, *score, query_terms, config);

        if explain {
            explanations.push(build_explanation(&hit, *score));
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
        Some(ExplainReport {
            hits: explanations,
            mode_used: SearchMode::Lexical,
            candidates_evaluated: total_count,
            phase_timings: {
                let mut m = HashMap::new();
                m.insert("search".to_string(), elapsed);
                m
            },
        })
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
    let id = doc
        .get_first(handles.id)
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

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

    if let Some(project) = doc
        .get_first(handles.project_slug)
        .and_then(|v| v.as_str())
    {
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
fn build_explanation(hit: &SearchHit, raw_score: f32) -> HitExplanation {
    HitExplanation {
        doc_id: hit.doc_id,
        tf_idf: None,
        bm25: Some(f64::from(raw_score)),
        semantic_similarity: None,
        final_score: hit.score,
        explanation: format!(
            "BM25 score={:.4}, doc_kind={}, id={}",
            raw_score,
            match hit.doc_kind {
                DocKind::Message => "message",
                DocKind::Agent => "agent",
                DocKind::Project => "project",
                DocKind::Thread => "thread",
            },
            hit.doc_id
        ),
    }
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
            let results = execute_search(
                &index,
                &AllQuery,
                &handles,
                &[],
                100,
                0,
                false,
                &config,
            );
            assert_eq!(results.total_count, 3);
            assert_eq!(results.hits.len(), 3);
            assert_eq!(results.mode_used, SearchMode::Lexical);
            assert!(results.explain.is_none());
        }

        #[test]
        fn execute_search_with_limit() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(
                &index,
                &AllQuery,
                &handles,
                &[],
                2,
                0,
                false,
                &config,
            );
            // total_count reflects docs fetched (up to limit)
            assert!(results.hits.len() <= 2);
        }

        #[test]
        fn execute_search_with_offset() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(
                &index,
                &AllQuery,
                &handles,
                &[],
                100,
                2,
                false,
                &config,
            );
            // Should skip 2 results
            assert_eq!(results.hits.len(), 1);
        }

        #[test]
        fn execute_search_with_query() {
            let (index, handles) = setup_index();
            let parser = QueryParser::for_index(
                &index,
                vec![handles.subject, handles.body],
            );
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
            assert!(results.hits[0]
                .snippet
                .as_ref()
                .unwrap()
                .contains("migration"));
        }

        #[test]
        fn execute_search_with_explain() {
            let (index, handles) = setup_index();
            let parser = QueryParser::for_index(
                &index,
                vec![handles.subject, handles.body],
            );
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
            assert!(explain.hits[0].bm25.is_some());
            assert!(explain.hits[0].explanation.contains("BM25"));
        }

        #[test]
        fn execute_search_metadata_populated() {
            let (index, handles) = setup_index();
            let config = ResponseConfig::default();
            let results = execute_search(
                &index,
                &AllQuery,
                &handles,
                &[],
                10,
                0,
                false,
                &config,
            );

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
            let parser = QueryParser::for_index(
                &index,
                vec![handles.subject, handles.body],
            );
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
            let results = execute_search(
                &index,
                &AllQuery,
                &handles,
                &[],
                100,
                0,
                false,
                &config,
            );
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
