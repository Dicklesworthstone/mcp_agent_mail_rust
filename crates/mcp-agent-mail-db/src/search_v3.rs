//! Search V3 bridge: routes search queries to Tantivy when enabled
//!
//! This module provides the integration layer between the existing search pipeline
//! (FTS5-based `search_planner` + `search_service`) and the new Tantivy-based
//! search engine in `mcp-agent-mail-search-core`.
//!
//! Feature-gated behind `search-v3` to keep the default build unchanged.

#[cfg(feature = "search-v3")]
mod inner {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};

    use mcp_agent_mail_search_core::filter_compiler::compile_filters;
    use mcp_agent_mail_search_core::lexical_parser::{LexicalParser, ParseOutcome, extract_terms};
    use mcp_agent_mail_search_core::lexical_response::{self, ResponseConfig};
    use mcp_agent_mail_search_core::query::{DateRange, ImportanceFilter, SearchFilter};
    use mcp_agent_mail_search_core::results::SearchResults;
    use mcp_agent_mail_search_core::tantivy_schema::{
        FieldHandles, build_schema, register_tokenizer,
    };
    use tantivy::Index;

    use crate::search_planner::{
        DocKind, Importance, SearchQuery as PlannerQuery, SearchResult as PlannerResult,
    };

    /// Bridge between the Tantivy search engine and the planner query/result types.
    pub struct TantivyBridge {
        index: Index,
        handles: FieldHandles,
        index_dir: PathBuf,
    }

    impl TantivyBridge {
        /// Open or create a Tantivy index at the given directory.
        ///
        /// If the directory doesn't exist, it will be created.
        /// If an index already exists, it will be opened.
        pub fn open(index_dir: &Path) -> Result<Self, String> {
            let (schema, handles) = build_schema();

            let index = if index_dir.join("meta.json").exists() {
                Index::open_in_dir(index_dir)
                    .map_err(|e| format!("failed to open Tantivy index: {e}"))?
            } else {
                std::fs::create_dir_all(index_dir)
                    .map_err(|e| format!("failed to create index dir: {e}"))?;
                Index::create_in_dir(index_dir, schema)
                    .map_err(|e| format!("failed to create Tantivy index: {e}"))?
            };

            register_tokenizer(&index);

            Ok(Self {
                index,
                handles,
                index_dir: index_dir.to_owned(),
            })
        }

        /// Create an in-memory index (for testing).
        #[cfg(test)]
        pub fn in_memory() -> Self {
            let (schema, handles) = build_schema();
            let index = Index::create_in_ram(schema);
            register_tokenizer(&index);
            Self {
                index,
                handles,
                index_dir: PathBuf::new(),
            }
        }

        /// Get a reference to the underlying Tantivy `Index`.
        pub fn index(&self) -> &Index {
            &self.index
        }

        /// Get the field handles.
        pub fn handles(&self) -> &FieldHandles {
            &self.handles
        }

        /// Get the index directory path.
        pub fn index_dir(&self) -> &Path {
            &self.index_dir
        }

        /// Execute a search using the planner query types.
        ///
        /// Converts the planner `SearchQuery` to Tantivy-native queries,
        /// executes the search, and converts results back to `SearchResult`.
        pub fn search(&self, query: &PlannerQuery) -> Vec<PlannerResult> {
            // Build text query
            let parser = LexicalParser::with_defaults(self.handles.subject, self.handles.body);
            let outcome = parser.parse(&self.index, &query.text);

            let text_query = match outcome {
                ParseOutcome::Parsed(q) | ParseOutcome::Fallback { query: q, .. } => q,
                ParseOutcome::Empty => return Vec::new(),
            };

            // Build filters
            let filter = build_search_filter(query);
            let compiled = compile_filters(&filter, &self.handles);
            let final_query = compiled.apply_to(text_query);

            // Extract terms for snippets
            let terms = extract_terms(&query.text);

            // Execute
            let limit = query.effective_limit();
            let config = ResponseConfig::default();
            let results = lexical_response::execute_search(
                &self.index,
                &*final_query,
                &self.handles,
                &terms,
                limit,
                0, // offset handled externally via cursor
                query.explain,
                &config,
            );

            // Convert to planner results
            convert_results(&results, query.doc_kind)
        }
    }

    /// Convert a planner `SearchQuery` to search-core `SearchFilter`.
    fn build_search_filter(query: &PlannerQuery) -> SearchFilter {
        let mut filter = SearchFilter::default();

        // Project scope
        if let Some(pid) = query.project_id {
            filter.project_id = Some(pid);
        }

        // Agent name → sender filter
        if let Some(ref agent) = query.agent_name {
            filter.sender = Some(agent.clone());
        }

        // Thread ID
        if let Some(ref tid) = query.thread_id {
            filter.thread_id = Some(tid.clone());
        }

        // Importance levels → filter
        if !query.importance.is_empty() {
            // Map planner importance levels to search-core filter
            let has_urgent = query.importance.contains(&Importance::Urgent);
            let has_high = query.importance.contains(&Importance::High);
            let has_normal = query.importance.contains(&Importance::Normal);
            let has_low = query.importance.contains(&Importance::Low);

            if has_urgent && !has_high && !has_normal && !has_low {
                filter.importance = Some(ImportanceFilter::Urgent);
            } else if (has_high || has_urgent) && !has_normal && !has_low {
                filter.importance = Some(ImportanceFilter::High);
            } else if has_normal && !has_high && !has_urgent && !has_low {
                filter.importance = Some(ImportanceFilter::Normal);
            } else if has_low && !has_high && !has_urgent && !has_normal {
                filter.importance = Some(ImportanceFilter::Low);
            }
            // If multiple non-adjacent levels, we can't express it as a single filter;
            // leave importance filter as None (accept all) and post-filter if needed
        }

        // Doc kind
        let doc_kind = match query.doc_kind {
            DocKind::Message => mcp_agent_mail_search_core::document::DocKind::Message,
            DocKind::Agent => mcp_agent_mail_search_core::document::DocKind::Agent,
            DocKind::Project => mcp_agent_mail_search_core::document::DocKind::Project,
        };
        filter.doc_kind = Some(doc_kind);

        // Time range → date range
        if !query.time_range.is_empty() {
            filter.date_range = Some(DateRange {
                start: query.time_range.min_ts,
                end: query.time_range.max_ts,
            });
        }

        filter
    }

    /// Convert search-core results back to planner `SearchResult` format.
    fn convert_results(results: &SearchResults, doc_kind: DocKind) -> Vec<PlannerResult> {
        results
            .hits
            .iter()
            .map(|hit| {
                let importance = hit
                    .metadata
                    .get("importance")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let thread_id = hit
                    .metadata
                    .get("thread_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let from_agent = hit
                    .metadata
                    .get("sender")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let created_ts = hit.metadata.get("created_ts").and_then(|v| v.as_i64());
                let subject = hit
                    .metadata
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                #[allow(clippy::cast_possible_wrap)]
                PlannerResult {
                    doc_kind,
                    id: hit.doc_id as i64,
                    project_id: hit.metadata.get("project_id").and_then(|v| v.as_i64()),
                    title: subject,
                    body: hit.snippet.clone().unwrap_or_default(),
                    score: Some(hit.score),
                    importance,
                    ack_required: None, // not in Tantivy index
                    created_ts,
                    thread_id,
                    from_agent,
                    redacted: false,
                    redaction_reason: None,
                }
            })
            .collect()
    }

    // ── Global bridge (lazy singleton) ──────────────────────────────────────

    static BRIDGE: OnceLock<Option<Arc<TantivyBridge>>> = OnceLock::new();

    /// Initialize the global Tantivy bridge.
    ///
    /// Should be called once at startup when `SearchEngine::Tantivy` or `Shadow`
    /// is configured. Returns `Ok(())` on success.
    pub fn init_bridge(index_dir: &Path) -> Result<(), String> {
        let bridge = TantivyBridge::open(index_dir)?;
        let _ = BRIDGE.set(Some(Arc::new(bridge)));
        Ok(())
    }

    /// Get the global Tantivy bridge, if initialized.
    pub fn get_bridge() -> Option<Arc<TantivyBridge>> {
        BRIDGE.get().and_then(|opt| opt.clone())
    }
}

#[cfg(feature = "search-v3")]
pub use inner::*;

// When the feature is disabled, provide stub types so callers can compile
// without feature-gating every call site.
#[cfg(not(feature = "search-v3"))]
mod stubs {
    use std::path::Path;

    /// Stub: always returns `Err` when search-v3 feature is not enabled.
    pub fn init_bridge(_index_dir: &Path) -> Result<(), String> {
        Err("search-v3 feature not enabled".to_string())
    }
}

#[cfg(not(feature = "search-v3"))]
pub use stubs::*;

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(feature = "search-v3")]
mod tests {
    use super::inner::*;
    use crate::search_planner::{DocKind, SearchQuery as PlannerQuery};
    use tantivy::doc;

    fn setup_bridge_with_docs() -> TantivyBridge {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => 1u64,
                handles.doc_kind => "message",
                handles.subject => "Migration plan review",
                handles.body => "Here is the plan for DB migration to v3",
                handles.sender => "BlueLake",
                handles.project_slug => "backend",
                handles.project_id => 1u64,
                handles.thread_id => "br-100",
                handles.importance => "high",
                handles.created_ts => 1_000_000_000_000i64
            ))
            .unwrap();
        writer
            .add_document(doc!(
                handles.id => 2u64,
                handles.doc_kind => "message",
                handles.subject => "Deployment checklist",
                handles.body => "Steps for deploying the new search engine",
                handles.sender => "RedPeak",
                handles.project_slug => "backend",
                handles.project_id => 1u64,
                handles.thread_id => "br-200",
                handles.importance => "normal",
                handles.created_ts => 2_000_000_000_000i64
            ))
            .unwrap();
        writer
            .add_document(doc!(
                handles.id => 3u64,
                handles.doc_kind => "message",
                handles.subject => "Critical hotfix required",
                handles.body => "Urgent fix needed for login auth flow",
                handles.sender => "BlueLake",
                handles.project_slug => "frontend",
                handles.project_id => 2u64,
                handles.thread_id => "br-300",
                handles.importance => "urgent",
                handles.created_ts => 3_000_000_000_000i64
            ))
            .unwrap();
        writer.commit().unwrap();

        bridge
    }

    #[test]
    fn search_simple_term() {
        let bridge = setup_bridge_with_docs();
        let query = PlannerQuery::messages("migration", 1);
        let results = bridge.search(&query);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 1);
    }

    #[test]
    fn search_empty_query() {
        let bridge = setup_bridge_with_docs();
        let mut query = PlannerQuery::messages("", 1);
        let results = bridge.search(&query);
        assert!(results.is_empty());
    }

    #[test]
    fn search_project_scoped() {
        let bridge = setup_bridge_with_docs();
        let query = PlannerQuery::messages("plan", 1);
        let results = bridge.search(&query);
        // "plan" appears in doc 1 (project 1), not doc 3 (project 2)
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 1);
    }

    #[test]
    fn search_no_project_scope() {
        let bridge = setup_bridge_with_docs();
        let mut query = PlannerQuery::default();
        query.text = "search".to_string();
        query.doc_kind = DocKind::Message;
        // No project_id filter
        let results = bridge.search(&query);
        // "search" only appears in doc 2
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 2);
    }

    #[test]
    fn search_with_sender_filter() {
        let bridge = setup_bridge_with_docs();
        let mut query = PlannerQuery::default();
        query.text = "plan fix".to_string();
        query.doc_kind = DocKind::Message;
        query.agent_name = Some("BlueLake".to_string());
        // Should match docs from BlueLake only
        let results = bridge.search(&query);
        for r in &results {
            assert_eq!(r.from_agent.as_deref(), Some("BlueLake"));
        }
    }

    #[test]
    fn search_results_have_metadata() {
        let bridge = setup_bridge_with_docs();
        let query = PlannerQuery::messages("migration", 1);
        let results = bridge.search(&query);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.doc_kind, DocKind::Message);
        assert_eq!(r.from_agent.as_deref(), Some("BlueLake"));
        assert_eq!(r.importance.as_deref(), Some("high"));
        assert_eq!(r.thread_id.as_deref(), Some("br-100"));
        assert!(r.created_ts.is_some());
        assert!(r.score.is_some());
    }

    #[test]
    fn search_no_results() {
        let bridge = setup_bridge_with_docs();
        let query = PlannerQuery::messages("nonexistent_xyzzy", 1);
        let results = bridge.search(&query);
        assert!(results.is_empty());
    }

    #[test]
    fn search_with_thread_filter() {
        let bridge = setup_bridge_with_docs();
        let mut query = PlannerQuery::default();
        query.text = "plan deploy fix".to_string();
        query.doc_kind = DocKind::Message;
        query.thread_id = Some("br-100".to_string());
        let results = bridge.search(&query);
        for r in &results {
            assert_eq!(r.thread_id.as_deref(), Some("br-100"));
        }
    }
}
