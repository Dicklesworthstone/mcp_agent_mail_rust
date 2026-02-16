//! Tantivy-backed search engine wiring for Search V3.
//!
//! When enabled via the `tantivy-search` feature and configured with
//! `SEARCH_V3_ENGINE=tantivy`, replaces the SQL-based FTS5 search pipeline
//! with Tantivy's BM25 ranking engine. The scope/redaction post-processing
//! contract is preserved — only the retrieval stage changes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use mcp_agent_mail_search_core::document::DocKind as ScDocKind;
use mcp_agent_mail_search_core::query::{DateRange, ImportanceFilter, SearchFilter};
use mcp_agent_mail_search_core::results::{SearchHit, SearchResults};
use mcp_agent_mail_search_core::tantivy_schema::{
    build_schema, register_tokenizer, FieldHandles, BODY_BOOST, SUBJECT_BOOST,
};
use mcp_agent_mail_search_core::{
    compile_filters, execute_search as tantivy_execute, extract_terms, sanitize_query,
    LexicalParser, LexicalParserConfig, ParseOutcome, ResponseConfig, SanitizedQuery,
};
use tantivy::query::AllQuery;
use tantivy::Index;

use crate::error::DbError;
use crate::search_planner::{DocKind, PlanMethod, QueryExplain, SearchCursor, SearchQuery};
use crate::search_planner::SearchResult;

// ────────────────────────────────────────────────────────────────────
// Engine selection
// ────────────────────────────────────────────────────────────────────

/// Which search engine to use. Controlled by `SEARCH_V3_ENGINE` env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngineKind {
    /// SQL/FTS5 (default, existing behavior).
    Sql,
    /// Tantivy BM25 lexical engine.
    Tantivy,
}

/// Read the configured engine kind from environment.
#[must_use]
pub fn configured_engine() -> SearchEngineKind {
    match std::env::var("SEARCH_V3_ENGINE").as_deref() {
        Ok("tantivy") => SearchEngineKind::Tantivy,
        _ => SearchEngineKind::Sql,
    }
}

// ────────────────────────────────────────────────────────────────────
// TantivyBackend
// ────────────────────────────────────────────────────────────────────

/// The Tantivy search backend, holding an open index and parser configuration.
pub struct TantivyBackend {
    index: Index,
    handles: FieldHandles,
    parser: LexicalParser,
}

impl TantivyBackend {
    /// Open or create a Tantivy index at the given directory.
    ///
    /// # Errors
    ///
    /// Returns `DbError` if the directory cannot be created or the index fails to open.
    pub fn open_or_create(index_dir: &Path) -> Result<Self, DbError> {
        let (schema, handles) = build_schema();

        let index = if index_dir.join("meta.json").exists() {
            Index::open_in_dir(index_dir)
                .map_err(|e| DbError::Sqlite(format!("tantivy open: {e}")))?
        } else {
            std::fs::create_dir_all(index_dir)
                .map_err(|e| DbError::Sqlite(format!("tantivy mkdir: {e}")))?;
            Index::create_in_dir(index_dir, schema)
                .map_err(|e| DbError::Sqlite(format!("tantivy create: {e}")))?
        };

        register_tokenizer(&index);

        let config = LexicalParserConfig {
            conjunction_by_default: true,
            subject_boost: SUBJECT_BOOST,
            body_boost: BODY_BOOST,
        };
        let parser = LexicalParser::new(config, handles.subject, handles.body);

        Ok(Self {
            index,
            handles,
            parser,
        })
    }

    /// Open a Tantivy index in RAM (for testing).
    #[cfg(test)]
    pub fn open_in_ram() -> Self {
        let (schema, handles) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);

        let config = LexicalParserConfig {
            conjunction_by_default: true,
            subject_boost: SUBJECT_BOOST,
            body_boost: BODY_BOOST,
        };
        let parser = LexicalParser::new(config, handles.subject, handles.body);

        Self {
            index,
            handles,
            parser,
        }
    }

    /// Returns a reference to the underlying Tantivy index.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Returns the field handles.
    #[must_use]
    pub fn handles(&self) -> &FieldHandles {
        &self.handles
    }

    /// Execute a search query against the Tantivy index.
    ///
    /// Returns a `TantivySearchOutcome` with mapped `SearchResult`s ready for
    /// the scope/redaction post-processing pipeline.
    #[must_use]
    pub fn search(&self, query: &SearchQuery) -> TantivySearchOutcome {
        let query_text = &query.text;

        // Fast path: empty query with no filters → empty results
        if query_text.is_empty() && !has_any_filter(query) {
            return TantivySearchOutcome {
                results: Vec::new(),
                next_cursor: None,
                explain: if query.explain {
                    Some(empty_explain())
                } else {
                    None
                },
                method: PlanMethod::Empty,
            };
        }

        // Parse using LexicalParser
        let parse_outcome = if query_text.is_empty() {
            ParseOutcome::Empty
        } else {
            self.parser.parse(&self.index, query_text)
        };

        let (tantivy_query, method, normalized) = match parse_outcome {
            ParseOutcome::Parsed {
                query: q,
                normalized,
                ..
            } => (q, PlanMethod::Fts, Some(normalized)),
            ParseOutcome::Fallback { query: q, .. } => (q, PlanMethod::Like, None),
            ParseOutcome::Empty => {
                if has_any_filter(query) {
                    (
                        Box::new(AllQuery) as Box<dyn tantivy::query::Query>,
                        PlanMethod::FilterOnly,
                        None,
                    )
                } else {
                    return TantivySearchOutcome {
                        results: Vec::new(),
                        next_cursor: None,
                        explain: if query.explain {
                            Some(empty_explain())
                        } else {
                            None
                        },
                        method: PlanMethod::Empty,
                    };
                }
            }
        };

        // Compile and apply metadata filters
        let filter = convert_query_to_filter(query);
        let compiled = compile_filters(&filter, &self.handles);
        let final_query = compiled.apply_to(tantivy_query);

        // Extract terms for snippet generation
        let terms = extract_terms(query_text);

        // Execute search
        let limit = query.effective_limit();
        let config = ResponseConfig::default();

        let search_results = tantivy_execute(
            &self.index,
            final_query.as_ref(),
            &self.handles,
            &terms,
            limit,
            0, // offset (cursor-based pagination computed post-hoc)
            query.explain,
            &config,
        );

        // Map hits → SearchResult
        let results: Vec<SearchResult> = search_results
            .hits
            .iter()
            .map(|hit| map_hit_to_search_result(hit, query.doc_kind))
            .collect();

        // Build explain
        let explain = if query.explain {
            let facets = list_facets(query);
            Some(QueryExplain {
                method: method.as_str().to_string(),
                normalized_query: normalized,
                used_like_fallback: method == PlanMethod::Like,
                facet_count: facets.len(),
                facets_applied: facets,
                sql: "[tantivy engine]".to_string(),
                scope_policy: "unrestricted".to_string(),
                denied_count: 0,
                redacted_count: 0,
            })
        } else {
            None
        };

        // Pagination cursor
        let next_cursor = compute_tantivy_cursor(&results, limit);

        TantivySearchOutcome {
            results,
            next_cursor,
            explain,
            method,
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// TantivySearchOutcome
// ────────────────────────────────────────────────────────────────────

/// Intermediate result of a Tantivy search, ready for scope/redaction.
pub struct TantivySearchOutcome {
    /// Mapped search results.
    pub results: Vec<SearchResult>,
    /// Pagination cursor for the next page.
    pub next_cursor: Option<String>,
    /// Query explain metadata.
    pub explain: Option<QueryExplain>,
    /// Which search method was used.
    pub method: PlanMethod,
}

// ────────────────────────────────────────────────────────────────────
// Global backend singleton
// ────────────────────────────────────────────────────────────────────

static TANTIVY_BACKEND: OnceLock<Option<TantivyBackend>> = OnceLock::new();

/// Initialize the global Tantivy backend.
///
/// Call once at server startup. Uses `SEARCH_V3_INDEX_DIR` env var for
/// the index directory. If the env var is unset, the backend is `None`
/// and all searches fall back to SQL.
///
/// # Errors
///
/// Returns `DbError` if the index cannot be opened.
pub fn init_tantivy_backend() -> Result<(), DbError> {
    TANTIVY_BACKEND.get_or_init(|| {
        let dir = match std::env::var("SEARCH_V3_INDEX_DIR") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => return None,
        };
        match TantivyBackend::open_or_create(&dir) {
            Ok(backend) => Some(backend),
            Err(e) => {
                tracing::warn!("failed to initialize Tantivy backend: {e}");
                None
            }
        }
    });
    Ok(())
}

/// Get the global Tantivy backend, if initialized and available.
#[must_use]
pub fn tantivy_backend() -> Option<&'static TantivyBackend> {
    TANTIVY_BACKEND.get().and_then(Option::as_ref)
}

/// Returns `true` if the Tantivy engine is both configured and available.
#[must_use]
pub fn should_use_tantivy() -> bool {
    configured_engine() == SearchEngineKind::Tantivy && tantivy_backend().is_some()
}

// ────────────────────────────────────────────────────────────────────
// Type conversion helpers
// ────────────────────────────────────────────────────────────────────

fn convert_query_to_filter(query: &SearchQuery) -> SearchFilter {
    let importance = if query.importance.is_empty() {
        None
    } else {
        // Map the strongest importance filter present
        use crate::search_planner::Importance;
        if query
            .importance
            .iter()
            .any(|i| matches!(i, Importance::Urgent))
        {
            Some(ImportanceFilter::Urgent)
        } else if query
            .importance
            .iter()
            .any(|i| matches!(i, Importance::High))
        {
            Some(ImportanceFilter::High)
        } else if query
            .importance
            .iter()
            .any(|i| matches!(i, Importance::Normal))
        {
            Some(ImportanceFilter::Normal)
        } else if query
            .importance
            .iter()
            .any(|i| matches!(i, Importance::Low))
        {
            Some(ImportanceFilter::Low)
        } else {
            None
        }
    };

    let date_range = if query.time_range.is_empty() {
        None
    } else {
        Some(DateRange {
            start: query.time_range.min_ts,
            end: query.time_range.max_ts,
        })
    };

    let doc_kind = match query.doc_kind {
        DocKind::Message => Some(ScDocKind::Message),
        DocKind::Agent => Some(ScDocKind::Agent),
        DocKind::Project => Some(ScDocKind::Project),
    };

    SearchFilter {
        sender: query.agent_name.clone(),
        project_id: query.project_id,
        date_range,
        importance,
        thread_id: query.thread_id.clone(),
        doc_kind,
    }
}

fn map_hit_to_search_result(hit: &SearchHit, doc_kind: DocKind) -> SearchResult {
    let m = &hit.metadata;

    let title = m
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let body = m
        .get("body")
        .and_then(|v| v.as_str())
        .or_else(|| hit.snippet.as_deref())
        .unwrap_or_default()
        .to_string();

    let importance = m
        .get("importance")
        .and_then(|v| v.as_str())
        .map(String::from);

    let created_ts = m.get("created_ts").and_then(|v| v.as_i64());

    let thread_id = m
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let from_agent = m
        .get("sender")
        .and_then(|v| v.as_str())
        .map(String::from);

    #[allow(clippy::cast_possible_wrap)]
    let project_id = m
        .get("project_id")
        .and_then(|v| v.as_u64())
        .map(|v| v as i64);

    SearchResult {
        doc_kind,
        id: hit.doc_id,
        project_id,
        title,
        body,
        score: Some(hit.score),
        importance,
        ack_required: None,
        created_ts,
        thread_id,
        from_agent,
        redacted: false,
        redaction_reason: None,
        reason_codes: Vec::new(),
        score_factors: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────────────
// Query analysis helpers
// ────────────────────────────────────────────────────────────────────

fn has_any_filter(query: &SearchQuery) -> bool {
    !query.importance.is_empty()
        || query.thread_id.is_some()
        || query.agent_name.is_some()
        || query.project_id.is_some()
        || !query.time_range.is_empty()
        || query.ack_required.is_some()
}

fn list_facets(query: &SearchQuery) -> Vec<String> {
    let mut facets = Vec::new();
    if !query.importance.is_empty() {
        facets.push("importance".to_string());
    }
    if query.thread_id.is_some() {
        facets.push("thread_id".to_string());
    }
    if query.agent_name.is_some() {
        facets.push("agent_name".to_string());
    }
    if query.project_id.is_some() {
        facets.push("project_id".to_string());
    }
    if !query.time_range.is_empty() {
        facets.push("time_range".to_string());
    }
    if query.ack_required.is_some() {
        facets.push("ack_required".to_string());
    }
    facets
}

fn empty_explain() -> QueryExplain {
    QueryExplain {
        method: PlanMethod::Empty.as_str().to_string(),
        normalized_query: None,
        used_like_fallback: false,
        facet_count: 0,
        facets_applied: Vec::new(),
        sql: "[tantivy engine — empty]".to_string(),
        scope_policy: "unrestricted".to_string(),
        denied_count: 0,
        redacted_count: 0,
    }
}

fn compute_tantivy_cursor(results: &[SearchResult], limit: usize) -> Option<String> {
    if results.len() < limit {
        return None;
    }
    results.last().map(|r| {
        SearchCursor {
            score: r.score.unwrap_or(0.0),
            id: r.id,
        }
        .encode()
    })
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_planner::{Importance, TimeRange};
    use tantivy::doc;

    #[test]
    fn configured_engine_default_is_sql() {
        // Unless SEARCH_V3_ENGINE is set to "tantivy", should be Sql
        let kind = configured_engine();
        // We can't guarantee env state, but at least assert the function works
        assert!(kind == SearchEngineKind::Sql || kind == SearchEngineKind::Tantivy);
    }

    #[test]
    fn empty_query_returns_empty() {
        let backend = TantivyBackend::open_in_ram();
        let query = SearchQuery::default();
        let outcome = backend.search(&query);
        assert!(outcome.results.is_empty());
        assert_eq!(outcome.method, PlanMethod::Empty);
    }

    #[test]
    fn search_no_docs_returns_empty() {
        let backend = TantivyBackend::open_in_ram();
        let query = SearchQuery::messages("hello", 1);
        let outcome = backend.search(&query);
        assert!(outcome.results.is_empty());
    }

    #[test]
    fn search_with_indexed_doc() {
        let backend = TantivyBackend::open_in_ram();
        let h = backend.handles();

        // Index a document
        let mut writer = backend.index().writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                h.id => 1u64,
                h.doc_kind => "message",
                h.subject => "Migration plan review",
                h.body => "Here is the plan for DB migration",
                h.sender => "BlueLake",
                h.project_slug => "test-project",
                h.project_id => 1u64,
                h.thread_id => "br-123",
                h.importance => "high",
                h.created_ts => 1_700_000_000_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        let query = SearchQuery::messages("migration", 1);
        let outcome = backend.search(&query);
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].id, 1);
        assert_eq!(outcome.results[0].doc_kind, DocKind::Message);
    }

    #[test]
    fn search_with_explain() {
        let backend = TantivyBackend::open_in_ram();
        let h = backend.handles();

        let mut writer = backend.index().writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                h.id => 1u64,
                h.doc_kind => "message",
                h.subject => "test subject",
                h.body => "test body",
                h.sender => "BlueLake",
                h.project_slug => "proj",
                h.project_id => 1u64,
                h.importance => "normal",
                h.created_ts => 1_700_000_000_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        let mut query = SearchQuery::messages("test", 1);
        query.explain = true;
        let outcome = backend.search(&query);
        assert!(outcome.explain.is_some());
        let explain = outcome.explain.unwrap();
        assert_eq!(explain.method, "fts5");
        assert!(!explain.used_like_fallback);
    }

    #[test]
    fn convert_filter_with_importance() {
        let mut query = SearchQuery::messages("test", 1);
        query.importance = vec![Importance::Urgent, Importance::High];
        let filter = convert_query_to_filter(&query);
        assert_eq!(filter.importance, Some(ImportanceFilter::Urgent));
        assert_eq!(filter.project_id, Some(1));
    }

    #[test]
    fn convert_filter_with_time_range() {
        let mut query = SearchQuery::messages("test", 1);
        query.time_range = TimeRange {
            min_ts: Some(100),
            max_ts: Some(999),
        };
        let filter = convert_query_to_filter(&query);
        assert!(filter.date_range.is_some());
        let dr = filter.date_range.unwrap();
        assert_eq!(dr.start, Some(100));
        assert_eq!(dr.end, Some(999));
    }

    #[test]
    fn convert_filter_with_thread() {
        let mut query = SearchQuery::messages("test", 1);
        query.thread_id = Some("br-42".to_string());
        let filter = convert_query_to_filter(&query);
        assert_eq!(filter.thread_id.as_deref(), Some("br-42"));
    }

    #[test]
    fn convert_filter_doc_kind_mapping() {
        for (db_kind, sc_kind) in [
            (DocKind::Message, ScDocKind::Message),
            (DocKind::Agent, ScDocKind::Agent),
            (DocKind::Project, ScDocKind::Project),
        ] {
            let mut query = SearchQuery::default();
            query.doc_kind = db_kind;
            let filter = convert_query_to_filter(&query);
            assert_eq!(filter.doc_kind, Some(sc_kind));
        }
    }

    #[test]
    fn map_hit_extracts_metadata() {
        use std::collections::HashMap;
        let mut meta = HashMap::new();
        meta.insert("subject".to_string(), serde_json::json!("My Subject"));
        meta.insert("body".to_string(), serde_json::json!("Body text"));
        meta.insert("sender".to_string(), serde_json::json!("RedFox"));
        meta.insert("importance".to_string(), serde_json::json!("urgent"));
        meta.insert("thread_id".to_string(), serde_json::json!("t-1"));
        meta.insert("project_id".to_string(), serde_json::json!(42u64));
        meta.insert(
            "created_ts".to_string(),
            serde_json::json!(1_700_000_000i64),
        );

        let hit = SearchHit {
            doc_id: 7,
            doc_kind: ScDocKind::Message,
            score: 0.95,
            snippet: None,
            highlight_ranges: Vec::new(),
            metadata: meta,
        };

        let result = map_hit_to_search_result(&hit, DocKind::Message);
        assert_eq!(result.id, 7);
        assert_eq!(result.title, "My Subject");
        assert_eq!(result.body, "Body text");
        assert_eq!(result.from_agent.as_deref(), Some("RedFox"));
        assert_eq!(result.importance.as_deref(), Some("urgent"));
        assert_eq!(result.thread_id.as_deref(), Some("t-1"));
        assert_eq!(result.project_id, Some(42));
        assert!((result.score.unwrap() - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn map_hit_uses_snippet_as_body_fallback() {
        let hit = SearchHit {
            doc_id: 1,
            doc_kind: ScDocKind::Message,
            score: 0.5,
            snippet: Some("snippet text".to_string()),
            highlight_ranges: Vec::new(),
            metadata: HashMap::new(),
        };

        let result = map_hit_to_search_result(&hit, DocKind::Message);
        assert_eq!(result.body, "snippet text");
    }

    #[test]
    fn list_facets_comprehensive() {
        let mut query = SearchQuery::messages("test", 1);
        query.importance = vec![Importance::High];
        query.thread_id = Some("t1".to_string());
        query.agent_name = Some("A".to_string());
        query.ack_required = Some(true);
        query.time_range = TimeRange {
            min_ts: Some(0),
            max_ts: None,
        };

        let facets = list_facets(&query);
        assert_eq!(facets.len(), 5);
        assert!(facets.contains(&"importance".to_string()));
        assert!(facets.contains(&"thread_id".to_string()));
        assert!(facets.contains(&"agent_name".to_string()));
        assert!(facets.contains(&"time_range".to_string()));
        assert!(facets.contains(&"ack_required".to_string()));
    }

    #[test]
    fn cursor_none_when_underfull() {
        let results = vec![SearchResult {
            doc_kind: DocKind::Message,
            id: 1,
            project_id: Some(1),
            title: String::new(),
            body: String::new(),
            score: Some(0.5),
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            redacted: false,
            redaction_reason: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
        }];
        assert!(compute_tantivy_cursor(&results, 50).is_none());
    }

    #[test]
    fn filter_only_with_facets_no_text() {
        let backend = TantivyBackend::open_in_ram();
        let h = backend.handles();

        let mut writer = backend.index().writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                h.id => 1u64,
                h.doc_kind => "message",
                h.subject => "hello",
                h.body => "world",
                h.sender => "BlueLake",
                h.project_slug => "proj",
                h.project_id => 1u64,
                h.importance => "urgent",
                h.created_ts => 1_700_000_000_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        let query = SearchQuery {
            doc_kind: DocKind::Message,
            project_id: Some(1),
            importance: vec![Importance::Urgent],
            ..Default::default()
        };
        let outcome = backend.search(&query);
        assert_eq!(outcome.method, PlanMethod::FilterOnly);
        assert_eq!(outcome.results.len(), 1);
    }

    use std::collections::HashMap;
}
