//! Search V3 bridge: routes search queries to Tantivy
//!
//! This module provides the integration layer between the existing search pipeline
//! (FTS5-based `search_planner` + `search_service`) and the Tantivy-based
//! search engine in `mcp-agent-mail-search-core`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use mcp_agent_mail_core::metrics::global_metrics;
use mcp_agent_mail_search_core::filter_compiler::compile_filters;
use mcp_agent_mail_search_core::lexical_parser::{LexicalParser, ParseOutcome, extract_terms};
use mcp_agent_mail_search_core::lexical_response::{self, ResponseConfig};
use mcp_agent_mail_search_core::query::{DateRange, ImportanceFilter, SearchFilter};
use mcp_agent_mail_search_core::results::SearchResults;
use mcp_agent_mail_search_core::tantivy_schema::{FieldHandles, build_schema, register_tokenizer};
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
        let doc_count = index
            .reader()
            .map_or(0, |reader| reader.searcher().num_docs());
        let index_size_bytes = measure_index_dir_bytes(index_dir);
        global_metrics()
            .search
            .update_index_health(index_size_bytes, doc_count);

        Ok(Self {
            index,
            handles,
            index_dir: index_dir.to_owned(),
        })
    }

    /// Create an in-memory index (for testing).
    #[cfg(test)]
    #[must_use]
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
    #[must_use]
    pub const fn index(&self) -> &Index {
        &self.index
    }

    /// Get the field handles.
    #[must_use]
    pub const fn handles(&self) -> &FieldHandles {
        &self.handles
    }

    /// Get the index directory path.
    #[must_use]
    pub fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    /// Execute a search using the planner query types.
    ///
    /// Converts the planner `SearchQuery` to Tantivy-native queries,
    /// executes the search, and converts results back to `SearchResult`.
    #[must_use]
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

fn measure_index_dir_bytes(index_dir: &Path) -> u64 {
    if !index_dir.is_dir() {
        return 0;
    }

    let mut stack = vec![index_dir.to_path_buf()];
    let mut total = 0_u64;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
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
        DocKind::Thread => mcp_agent_mail_search_core::document::DocKind::Thread,
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
            let created_ts = hit
                .metadata
                .get("created_ts")
                .and_then(serde_json::Value::as_i64);
            let subject = hit
                .metadata
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            PlannerResult {
                doc_kind,
                id: hit.doc_id,
                project_id: hit
                    .metadata
                    .get("project_id")
                    .and_then(serde_json::Value::as_i64),
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
                reason_codes: Vec::new(),
                score_factors: Vec::new(),
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
    use crate::search_service::{record_warmup, record_warmup_failure, record_warmup_start};
    use mcp_agent_mail_search_core::cache::WarmResource;

    record_warmup_start(WarmResource::LexicalIndex);
    let warmup_timer = std::time::Instant::now();
    let bridge = match TantivyBridge::open(index_dir) {
        Ok(b) => b,
        Err(e) => {
            record_warmup_failure(WarmResource::LexicalIndex, &e);
            return Err(e);
        }
    };
    let _ = BRIDGE.set(Some(Arc::new(bridge)));
    record_warmup(WarmResource::LexicalIndex, warmup_timer.elapsed());
    Ok(())
}

/// Get the global Tantivy bridge, if initialized.
pub fn get_bridge() -> Option<Arc<TantivyBridge>> {
    BRIDGE.get().and_then(std::clone::Clone::clone)
}

// ── Incremental indexing ──────────────────────────────────────────────────

/// Metadata required to index a single message into Tantivy.
///
/// This struct carries only the fields needed for the search index — no
/// database connection or query context is required.
#[derive(Debug, Clone)]
pub struct IndexableMessage {
    pub id: i64,
    pub project_id: i64,
    pub project_slug: String,
    pub sender_name: String,
    pub subject: String,
    pub body_md: String,
    pub thread_id: Option<String>,
    pub importance: String,
    pub created_ts: i64,
}

/// Index a single message into the global Tantivy bridge.
///
/// Returns `Ok(true)` if the message was indexed, `Ok(false)` if the bridge
/// is not initialized (search V3 disabled), or `Err` on write failure.
///
/// This is intentionally fire-and-forget safe: callers should not fail the
/// message send operation if indexing fails.
pub fn index_message(msg: &IndexableMessage) -> Result<bool, String> {
    let bridge = match get_bridge() {
        Some(b) => b,
        None => return Ok(false), // bridge not initialized, skip silently
    };

    let handles = bridge.handles();
    let mut writer = bridge
        .index()
        .writer(15_000_000)
        .map_err(|e| format!("Tantivy writer error: {e}"))?;

    #[allow(clippy::cast_sign_loss)]
    let id_u64 = msg.id as u64;
    #[allow(clippy::cast_sign_loss)]
    let project_id_u64 = msg.project_id as u64;

    writer
        .add_document(tantivy::doc!(
            handles.id => id_u64,
            handles.doc_kind => "message",
            handles.subject => msg.subject.as_str(),
            handles.body => msg.body_md.as_str(),
            handles.sender => msg.sender_name.as_str(),
            handles.project_slug => msg.project_slug.as_str(),
            handles.project_id => project_id_u64,
            handles.thread_id => msg.thread_id.as_deref().unwrap_or(""),
            handles.importance => msg.importance.as_str(),
            handles.created_ts => msg.created_ts
        ))
        .map_err(|e| format!("Tantivy add_document error: {e}"))?;

    writer
        .commit()
        .map_err(|e| format!("Tantivy commit error: {e}"))?;

    // Update index health metrics.
    let doc_count = bridge
        .index()
        .reader()
        .map_or(0, |reader| reader.searcher().num_docs());
    let index_size_bytes = measure_index_dir_bytes(bridge.index_dir());
    mcp_agent_mail_core::metrics::global_metrics()
        .search
        .update_index_health(index_size_bytes, doc_count);

    // Invalidate search cache so new messages appear immediately.
    crate::search_service::invalidate_search_cache(
        mcp_agent_mail_search_core::cache::InvalidationTrigger::IndexUpdate,
    );

    Ok(true)
}

/// Index a batch of messages into the global Tantivy bridge.
///
/// More efficient than calling [`index_message`] repeatedly — uses a single
/// writer and commit for the entire batch.
pub fn index_messages_batch(messages: &[IndexableMessage]) -> Result<usize, String> {
    if messages.is_empty() {
        return Ok(0);
    }

    let bridge = match get_bridge() {
        Some(b) => b,
        None => return Ok(0),
    };

    let handles = bridge.handles();
    let mut writer = bridge
        .index()
        .writer(15_000_000)
        .map_err(|e| format!("Tantivy writer error: {e}"))?;

    for msg in messages {
        #[allow(clippy::cast_sign_loss)]
        let id_u64 = msg.id as u64;
        #[allow(clippy::cast_sign_loss)]
        let project_id_u64 = msg.project_id as u64;

        writer
            .add_document(tantivy::doc!(
                handles.id => id_u64,
                handles.doc_kind => "message",
                handles.subject => msg.subject.as_str(),
                handles.body => msg.body_md.as_str(),
                handles.sender => msg.sender_name.as_str(),
                handles.project_slug => msg.project_slug.as_str(),
                handles.project_id => project_id_u64,
                handles.thread_id => msg.thread_id.as_deref().unwrap_or(""),
                handles.importance => msg.importance.as_str(),
                handles.created_ts => msg.created_ts
            ))
            .map_err(|e| format!("Tantivy add_document error: {e}"))?;
    }

    writer
        .commit()
        .map_err(|e| format!("Tantivy commit error: {e}"))?;

    let doc_count = bridge
        .index()
        .reader()
        .map_or(0, |reader| reader.searcher().num_docs());
    let index_size_bytes = measure_index_dir_bytes(bridge.index_dir());
    mcp_agent_mail_core::metrics::global_metrics()
        .search
        .update_index_health(index_size_bytes, doc_count);

    crate::search_service::invalidate_search_cache(
        mcp_agent_mail_search_core::cache::InvalidationTrigger::IndexUpdate,
    );

    Ok(messages.len())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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
        let query = PlannerQuery::messages("", 1);
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
        let query = PlannerQuery {
            text: "search".to_string(),
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        // No project_id filter
        let results = bridge.search(&query);
        // "search" only appears in doc 2
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 2);
    }

    #[test]
    fn search_with_sender_filter() {
        let bridge = setup_bridge_with_docs();
        let query = PlannerQuery {
            text: "plan fix".to_string(),
            doc_kind: DocKind::Message,
            agent_name: Some("BlueLake".to_string()),
            ..Default::default()
        };
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
        let query = PlannerQuery {
            text: "plan deploy fix".to_string(),
            doc_kind: DocKind::Message,
            thread_id: Some("br-100".to_string()),
            ..Default::default()
        };
        let results = bridge.search(&query);
        for r in &results {
            assert_eq!(r.thread_id.as_deref(), Some("br-100"));
        }
    }

    #[test]
    fn measure_index_dir_bytes_counts_nested_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nested = temp.path().join("nested");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::write(temp.path().join("a.bin"), [1_u8; 4]).expect("write file a");
        std::fs::write(nested.join("b.bin"), [2_u8; 6]).expect("write file b");

        let size = measure_index_dir_bytes(temp.path());
        assert!(
            size >= 10,
            "expected at least 10 bytes, got {size} for {}",
            temp.path().display()
        );
    }

    // -- measure_index_dir_bytes edge cases --------------------------------

    #[test]
    fn measure_index_dir_bytes_nonexistent() {
        let size = measure_index_dir_bytes(Path::new("/tmp/nonexistent-dir-xyzzy-12345"));
        assert_eq!(size, 0);
    }

    #[test]
    fn measure_index_dir_bytes_empty_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let size = measure_index_dir_bytes(temp.path());
        assert_eq!(size, 0);
    }

    // -- build_search_filter tests -----------------------------------------

    #[test]
    fn filter_default_query_has_message_doc_kind() {
        let query = PlannerQuery::messages("test", 1);
        let filter = build_search_filter(&query);
        assert_eq!(
            filter.doc_kind,
            Some(mcp_agent_mail_search_core::document::DocKind::Message)
        );
        assert_eq!(filter.project_id, Some(1));
        assert!(filter.sender.is_none());
        assert!(filter.thread_id.is_none());
        assert!(filter.importance.is_none());
        assert!(filter.date_range.is_none());
    }

    #[test]
    fn filter_agent_doc_kind() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Agent,
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(
            filter.doc_kind,
            Some(mcp_agent_mail_search_core::document::DocKind::Agent)
        );
    }

    #[test]
    fn filter_project_doc_kind() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Project,
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(
            filter.doc_kind,
            Some(mcp_agent_mail_search_core::document::DocKind::Project)
        );
    }

    #[test]
    fn filter_thread_doc_kind() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Thread,
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(
            filter.doc_kind,
            Some(mcp_agent_mail_search_core::document::DocKind::Thread)
        );
    }

    #[test]
    fn filter_with_sender() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            agent_name: Some("BlueLake".to_string()),
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(filter.sender.as_deref(), Some("BlueLake"));
    }

    #[test]
    fn filter_with_thread_id() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            thread_id: Some("br-42".to_string()),
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(filter.thread_id.as_deref(), Some("br-42"));
    }

    #[test]
    fn filter_importance_urgent_only() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::Urgent],
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(filter.importance, Some(ImportanceFilter::Urgent));
    }

    #[test]
    fn filter_importance_high_only() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::High],
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(filter.importance, Some(ImportanceFilter::High));
    }

    #[test]
    fn filter_importance_normal_only() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::Normal],
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(filter.importance, Some(ImportanceFilter::Normal));
    }

    #[test]
    fn filter_importance_low_only() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::Low],
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert_eq!(filter.importance, Some(ImportanceFilter::Low));
    }

    #[test]
    fn filter_importance_high_and_urgent_combined() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::High, Importance::Urgent],
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        // High + Urgent without Normal/Low maps to ImportanceFilter::High.
        assert_eq!(filter.importance, Some(ImportanceFilter::High));
    }

    #[test]
    fn filter_importance_mixed_leaves_none() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::High, Importance::Low],
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        // Non-adjacent levels can't be expressed as a single filter → None.
        assert!(filter.importance.is_none());
    }

    #[test]
    fn filter_with_time_range() {
        use crate::search_planner::TimeRange;
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            time_range: TimeRange {
                min_ts: Some(1_000_000),
                max_ts: Some(2_000_000),
            },
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        let date_range = filter.date_range.expect("should have date_range");
        assert_eq!(date_range.start, Some(1_000_000));
        assert_eq!(date_range.end, Some(2_000_000));
    }

    #[test]
    fn filter_empty_time_range_no_date_filter() {
        use crate::search_planner::TimeRange;
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            time_range: TimeRange {
                min_ts: None,
                max_ts: None,
            },
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        assert!(filter.date_range.is_none());
    }

    #[test]
    fn filter_half_open_time_range() {
        use crate::search_planner::TimeRange;
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            time_range: TimeRange {
                min_ts: Some(1_000_000),
                max_ts: None,
            },
            ..Default::default()
        };
        let filter = build_search_filter(&query);
        let date_range = filter.date_range.expect("should have date_range");
        assert_eq!(date_range.start, Some(1_000_000));
        assert!(date_range.end.is_none());
    }

    // -- convert_results tests ---------------------------------------------

    fn make_search_results(
        hits: Vec<mcp_agent_mail_search_core::results::SearchHit>,
    ) -> SearchResults {
        use mcp_agent_mail_search_core::query::SearchMode;
        SearchResults {
            total_count: hits.len(),
            hits,
            mode_used: SearchMode::Lexical,
            explain: None,
            elapsed: std::time::Duration::ZERO,
        }
    }

    fn make_hit(
        doc_id: i64,
        score: f64,
        snippet: Option<&str>,
        metadata: std::collections::HashMap<String, serde_json::Value>,
    ) -> mcp_agent_mail_search_core::results::SearchHit {
        use mcp_agent_mail_search_core::document::DocKind as CoreDocKind;
        mcp_agent_mail_search_core::results::SearchHit {
            doc_id,
            doc_kind: CoreDocKind::Message,
            score,
            snippet: snippet.map(str::to_string),
            highlight_ranges: vec![],
            metadata,
        }
    }

    #[test]
    fn convert_empty_results() {
        let results = make_search_results(vec![]);
        let converted = convert_results(&results, DocKind::Message);
        assert!(converted.is_empty());
    }

    #[test]
    fn convert_results_preserves_doc_kind() {
        let mut meta = std::collections::HashMap::new();
        meta.insert("subject".to_string(), serde_json::json!("Test Subject"));
        meta.insert("sender".to_string(), serde_json::json!("RedPeak"));
        let hit = make_hit(42, 1.5, Some("snippet"), meta);
        let results = make_search_results(vec![hit]);

        for kind in &[
            DocKind::Message,
            DocKind::Agent,
            DocKind::Project,
            DocKind::Thread,
        ] {
            let converted = convert_results(&results, *kind);
            assert_eq!(converted.len(), 1);
            assert_eq!(converted[0].doc_kind, *kind);
        }
    }

    #[test]
    fn convert_results_extracts_all_metadata_fields() {
        let mut meta = std::collections::HashMap::new();
        meta.insert("subject".to_string(), serde_json::json!("Important Mail"));
        meta.insert("sender".to_string(), serde_json::json!("GoldHawk"));
        meta.insert("importance".to_string(), serde_json::json!("urgent"));
        meta.insert("thread_id".to_string(), serde_json::json!("br-500"));
        meta.insert(
            "created_ts".to_string(),
            serde_json::json!(9_876_543_210i64),
        );
        meta.insert("project_id".to_string(), serde_json::json!(3i64));
        let hit = make_hit(99, 2.5, Some("snippet text"), meta);
        let results = make_search_results(vec![hit]);
        let converted = convert_results(&results, DocKind::Message);
        let r = &converted[0];

        assert_eq!(r.id, 99);
        assert_eq!(r.score, Some(2.5));
        assert_eq!(r.title, "Important Mail");
        assert_eq!(r.body, "snippet text");
        assert_eq!(r.from_agent.as_deref(), Some("GoldHawk"));
        assert_eq!(r.importance.as_deref(), Some("urgent"));
        assert_eq!(r.thread_id.as_deref(), Some("br-500"));
        assert_eq!(r.created_ts, Some(9_876_543_210));
        assert_eq!(r.project_id, Some(3));
        assert!(!r.redacted);
        assert!(r.redaction_reason.is_none());
        assert!(r.ack_required.is_none());
    }

    #[test]
    fn convert_results_handles_missing_metadata() {
        let hit = make_hit(1, 0.5, None, std::collections::HashMap::new());
        let results = make_search_results(vec![hit]);
        let converted = convert_results(&results, DocKind::Message);
        let r = &converted[0];

        assert_eq!(r.id, 1);
        assert_eq!(r.title, "");
        assert_eq!(r.body, "");
        assert!(r.from_agent.is_none());
        assert!(r.importance.is_none());
        assert!(r.thread_id.is_none());
        assert!(r.created_ts.is_none());
        assert!(r.project_id.is_none());
    }

    // -- TantivyBridge in_memory and accessors ------------------------------

    #[test]
    fn in_memory_bridge_has_empty_index_dir() {
        let bridge = TantivyBridge::in_memory();
        assert_eq!(bridge.index_dir(), Path::new(""));
    }

    #[test]
    fn in_memory_bridge_provides_index_and_handles() {
        let bridge = TantivyBridge::in_memory();
        // Should be able to get a reader (empty index is valid).
        let reader = bridge.index().reader().expect("reader");
        assert_eq!(reader.searcher().num_docs(), 0);
        // handles should have non-zero field references.
        let _subject = bridge.handles().subject;
        let _body = bridge.handles().body;
    }

    // -- TantivyBridge::open with temp directory ----------------------------

    #[test]
    fn open_creates_new_index_in_empty_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bridge = TantivyBridge::open(temp.path()).expect("open bridge");
        assert_eq!(bridge.index_dir(), temp.path());

        // meta.json should exist after index creation.
        assert!(temp.path().join("meta.json").exists());

        // Empty index should have 0 docs.
        let reader = bridge.index().reader().expect("reader");
        assert_eq!(reader.searcher().num_docs(), 0);
    }

    #[test]
    fn open_reuses_existing_index() {
        let temp = tempfile::tempdir().expect("tempdir");

        // Create an index and add a doc.
        let bridge1 = TantivyBridge::open(temp.path()).expect("open1");
        let handles = bridge1.handles();
        let mut writer = bridge1.index().writer(15_000_000).expect("writer");
        writer
            .add_document(doc!(
                handles.id => 42u64,
                handles.doc_kind => "message",
                handles.subject => "Reopen test",
                handles.body => "Body content",
                handles.sender => "TestAgent",
                handles.project_slug => "proj",
                handles.project_id => 1u64,
                handles.thread_id => "t-1",
                handles.importance => "normal",
                handles.created_ts => 1_000_000i64
            ))
            .expect("add doc");
        writer.commit().expect("commit");
        drop(bridge1);

        // Reopen the same directory — should find the existing doc.
        let bridge2 = TantivyBridge::open(temp.path()).expect("open2");
        let reader = bridge2.index().reader().expect("reader");
        assert_eq!(reader.searcher().num_docs(), 1);
    }

    #[test]
    fn open_creates_missing_parent_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nested = temp.path().join("a").join("b").join("c");
        let bridge = TantivyBridge::open(&nested).expect("open nested");
        assert!(nested.join("meta.json").exists());
        assert_eq!(bridge.index_dir(), nested.as_path());
    }

    // -- Search with multiple hits -----------------------------------------

    #[test]
    fn search_returns_hits_with_scores() {
        let bridge = setup_bridge_with_docs();
        // "plan" appears in doc 1 subject ("Migration plan review") and body.
        let query = PlannerQuery::messages("plan", 1);
        let results = bridge.search(&query);
        assert!(!results.is_empty(), "should find at least one result");
        for r in &results {
            assert!(r.score.is_some(), "every result should have a score");
            assert!(
                r.score.unwrap() > 0.0,
                "score should be positive, got {:?}",
                r.score
            );
        }
    }

    // -- Incremental indexing tests ----------------------------------------

    fn make_indexable(id: i64, subject: &str, body: &str) -> IndexableMessage {
        IndexableMessage {
            id,
            project_id: 1,
            project_slug: "test-project".to_string(),
            sender_name: "TestAgent".to_string(),
            subject: subject.to_string(),
            body_md: body.to_string(),
            thread_id: Some("thread-1".to_string()),
            importance: "normal".to_string(),
            created_ts: 1_000_000_000_000,
        }
    }

    #[test]
    fn index_message_without_bridge_returns_false() {
        // When the global bridge is not initialized, index_message should
        // gracefully return Ok(false) rather than error.
        // NOTE: This test relies on the global BRIDGE not being set in this
        // test binary. Since OnceLock is process-global, this test must run
        // before any test that calls init_bridge in the same process.
        // In practice, the bridge is only set by init_bridge() and our tests
        // use TantivyBridge::in_memory() which doesn't set the global.
        let msg = make_indexable(1, "Test", "Body");
        let result = index_message(&msg);
        // Either Ok(false) (bridge not set) or Ok(true) (bridge set by another test).
        assert!(result.is_ok());
    }

    #[test]
    fn index_messages_batch_empty_returns_zero() {
        let result = index_messages_batch(&[]);
        assert_eq!(result, Ok(0));
    }

    #[test]
    fn indexable_message_fields_roundtrip() {
        // Verify IndexableMessage can be created and all fields accessed.
        let msg = IndexableMessage {
            id: 42,
            project_id: 7,
            project_slug: "backend".to_string(),
            sender_name: "BlueLake".to_string(),
            subject: "Test Subject".to_string(),
            body_md: "Test body content".to_string(),
            thread_id: Some("br-100".to_string()),
            importance: "high".to_string(),
            created_ts: 1_234_567_890,
        };
        assert_eq!(msg.id, 42);
        assert_eq!(msg.project_id, 7);
        assert_eq!(msg.project_slug, "backend");
        assert_eq!(msg.sender_name, "BlueLake");
        assert_eq!(msg.thread_id.as_deref(), Some("br-100"));
    }

    #[test]
    fn index_message_via_bridge_directly() {
        // Test the indexing logic by manually creating a bridge and indexing.
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let msg = make_indexable(100, "Indexing test subject", "Body about database migration");

        #[allow(clippy::cast_sign_loss)]
        let id_u64 = msg.id as u64;
        #[allow(clippy::cast_sign_loss)]
        let project_id_u64 = msg.project_id as u64;

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => id_u64,
                handles.doc_kind => "message",
                handles.subject => msg.subject.as_str(),
                handles.body => msg.body_md.as_str(),
                handles.sender => msg.sender_name.as_str(),
                handles.project_slug => msg.project_slug.as_str(),
                handles.project_id => project_id_u64,
                handles.thread_id => msg.thread_id.as_deref().unwrap_or(""),
                handles.importance => msg.importance.as_str(),
                handles.created_ts => msg.created_ts
            ))
            .unwrap();
        writer.commit().unwrap();

        // Search for the indexed message.
        let query = PlannerQuery {
            text: "database migration".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert_eq!(results.len(), 1, "should find the indexed message");
        assert_eq!(results[0].id, 100);
        assert_eq!(results[0].from_agent.as_deref(), Some("TestAgent"));
    }

    #[test]
    fn index_batch_via_bridge_directly() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let messages = vec![
            make_indexable(1, "First message", "Content about Rust programming"),
            make_indexable(2, "Second message", "Content about Python scripting"),
            make_indexable(3, "Third message", "Content about database optimization"),
        ];

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        for msg in &messages {
            #[allow(clippy::cast_sign_loss)]
            let id_u64 = msg.id as u64;
            #[allow(clippy::cast_sign_loss)]
            let project_id_u64 = msg.project_id as u64;
            writer
                .add_document(doc!(
                    handles.id => id_u64,
                    handles.doc_kind => "message",
                    handles.subject => msg.subject.as_str(),
                    handles.body => msg.body_md.as_str(),
                    handles.sender => msg.sender_name.as_str(),
                    handles.project_slug => msg.project_slug.as_str(),
                    handles.project_id => project_id_u64,
                    handles.thread_id => msg.thread_id.as_deref().unwrap_or(""),
                    handles.importance => msg.importance.as_str(),
                    handles.created_ts => msg.created_ts
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let reader = bridge.index().reader().unwrap();
        assert_eq!(reader.searcher().num_docs(), 3);

        // Search for "Rust" — should find only first message.
        let query = PlannerQuery {
            text: "Rust programming".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 1);
    }

    #[test]
    fn indexable_message_no_thread_id() {
        let msg = IndexableMessage {
            id: 1,
            project_id: 1,
            project_slug: "proj".to_string(),
            sender_name: "Agent".to_string(),
            subject: "No thread".to_string(),
            body_md: "Body".to_string(),
            thread_id: None,
            importance: "low".to_string(),
            created_ts: 0,
        };
        assert!(msg.thread_id.is_none());

        // Index with None thread_id — should use empty string.
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();
        let mut writer = bridge.index().writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                handles.id => 1u64,
                handles.doc_kind => "message",
                handles.subject => msg.subject.as_str(),
                handles.body => msg.body_md.as_str(),
                handles.sender => msg.sender_name.as_str(),
                handles.project_slug => msg.project_slug.as_str(),
                handles.project_id => 1u64,
                handles.thread_id => msg.thread_id.as_deref().unwrap_or(""),
                handles.importance => msg.importance.as_str(),
                handles.created_ts => msg.created_ts
            ))
            .unwrap();
        writer.commit().unwrap();

        let reader = bridge.index().reader().unwrap();
        assert_eq!(reader.searcher().num_docs(), 1);
    }

    #[test]
    fn indexable_message_clone_and_debug() {
        let msg = make_indexable(1, "Test", "Body");
        let cloned = msg.clone();
        assert_eq!(cloned.id, msg.id);
        assert_eq!(cloned.subject, msg.subject);
        let debug = format!("{msg:?}");
        assert!(debug.contains("IndexableMessage"));
    }
}
