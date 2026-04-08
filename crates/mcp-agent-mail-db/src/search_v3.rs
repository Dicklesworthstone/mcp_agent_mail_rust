//! Search V3 bridge: routes search queries to Tantivy
//!
//! This module provides the integration layer between the existing search pipeline
//! (FTS5-based `search_planner` + `search_service`) and the Tantivy-based
//! search engine in `mcp-agent-mail-search-core`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use crate::query_assistance::{LexicalParser, ParseOutcome, extract_terms};
use crate::search_filter_compiler::compile_filters;
use crate::search_response::{self as lexical_response, ResponseConfig};
use crate::tantivy_schema::{FieldHandles, build_schema, register_tokenizer};
use mcp_agent_mail_core::metrics::global_metrics;
use mcp_agent_mail_core::search_types::{DateRange, ImportanceFilter, SearchFilter, SearchResults};
use sqlmodel_core::Value;
use tantivy::Order;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{AllQuery, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{Index, TantivyDocument, Term};

use crate::DbConn;
use crate::search_planner::{
    Direction, DocKind, Importance, SearchQuery as PlannerQuery, SearchResult as PlannerResult,
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
        let importance_plan = build_importance_filter_plan(query);
        let filter = build_search_filter(query, &importance_plan);
        let compiled = compile_filters(&filter, &self.handles);

        // Build text query
        let parser = LexicalParser::with_defaults(self.handles.subject, self.handles.body);
        let outcome = parser.parse(&self.index, &query.text);

        let text_query: Box<dyn Query> = match outcome {
            ParseOutcome::Parsed(q) | ParseOutcome::Fallback { query: q, .. } => q,
            ParseOutcome::Empty => {
                if compiled.is_empty() {
                    return Vec::new();
                }
                Box::new(AllQuery)
            }
        };

        let final_query = compiled.apply_to(text_query);

        // Extract terms for snippets
        let terms = extract_terms(&query.text);

        // Execute
        let limit = query.effective_limit();
        let config = ResponseConfig::default();
        let mut fetch_limit = if importance_plan.needs_post_filter {
            limit.saturating_mul(4).max(limit).max(16)
        } else {
            limit
        };
        let max_fetch_limit = limit.saturating_mul(16).max(fetch_limit).max(64);

        loop {
            let results = lexical_response::execute_search(
                &self.index,
                &*final_query,
                &self.handles,
                &terms,
                fetch_limit,
                0, // offset handled externally via cursor
                query.explain,
                &config,
            );

            let mut planner_results = convert_results(&results, query.doc_kind);
            if let Some(allowed) = importance_plan.exact_importances.as_ref() {
                planner_results.retain(|result| {
                    result
                        .importance
                        .as_deref()
                        .is_some_and(|importance| allowed.contains(importance))
                });
            }
            if planner_results.len() >= limit
                || !importance_plan.needs_post_filter
                || results.hits.len() < fetch_limit
                || fetch_limit >= max_fetch_limit
            {
                planner_results.truncate(limit);
                return planner_results;
            }
            fetch_limit = fetch_limit.saturating_mul(2).min(max_fetch_limit);
        }
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
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportanceFilterPlan {
    filter: Option<ImportanceFilter>,
    exact_importances: Option<BTreeSet<&'static str>>,
    needs_post_filter: bool,
}

fn build_importance_filter_plan(query: &PlannerQuery) -> ImportanceFilterPlan {
    if query.importance.is_empty() {
        return ImportanceFilterPlan {
            filter: None,
            exact_importances: None,
            needs_post_filter: false,
        };
    }

    let exact_importances: BTreeSet<&'static str> = query
        .importance
        .iter()
        .copied()
        .map(Importance::as_str)
        .collect();
    let has_urgent = exact_importances.contains("urgent");
    let has_high = exact_importances.contains("high");
    let has_normal = exact_importances.contains("normal");
    let has_low = exact_importances.contains("low");

    let filter = if has_urgent && !has_high && !has_normal && !has_low {
        Some(ImportanceFilter::Urgent)
    } else if has_high && !has_normal && !has_low {
        // High alone or High + Urgent both map to High (adjacent upper levels).
        Some(ImportanceFilter::High)
    } else if has_normal && !has_high && !has_urgent && !has_low {
        Some(ImportanceFilter::Normal)
    } else if has_low && !has_high && !has_urgent && !has_normal {
        Some(ImportanceFilter::Low)
    } else {
        None
    };
    let needs_post_filter = match filter {
        Some(ImportanceFilter::Urgent | ImportanceFilter::Normal | ImportanceFilter::Low) => false,
        Some(ImportanceFilter::High) => !has_urgent,
        Some(ImportanceFilter::Any) | None => true,
    };

    ImportanceFilterPlan {
        filter,
        exact_importances: Some(exact_importances),
        needs_post_filter,
    }
}

/// Convert a planner `SearchQuery` to search-core `SearchFilter`.
fn build_search_filter(
    query: &PlannerQuery,
    importance_plan: &ImportanceFilterPlan,
) -> SearchFilter {
    let mut filter = SearchFilter::default();

    // Project scope
    if let Some(pid) = query.project_id {
        filter.project_id = Some(pid);
    }

    // Only pure outbox queries can enforce agent_name at lexical-filter time.
    if let Some(ref agent) = query.agent_name
        && query.doc_kind == DocKind::Message
        && matches!(query.direction, Some(Direction::Outbox))
    {
        filter.sender = Some(agent.clone());
    }

    // Thread ID
    if let Some(ref tid) = query.thread_id {
        filter.thread_id = Some(tid.clone());
    }

    // Importance levels → filter
    filter.importance = importance_plan.filter;

    // Doc kind
    let doc_kind = match query.doc_kind {
        DocKind::Message => mcp_agent_mail_core::DocKind::Message,
        DocKind::Agent => mcp_agent_mail_core::DocKind::Agent,
        DocKind::Project => mcp_agent_mail_core::DocKind::Project,
        DocKind::Thread => mcp_agent_mail_core::DocKind::Thread,
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
                ..PlannerResult::default()
            }
        })
        .collect()
}

// ── Global bridge (lazy singleton) ──────────────────────────────────────

static BRIDGE: OnceLock<RwLock<Option<Arc<TantivyBridge>>>> = OnceLock::new();

fn bridge_slot() -> &'static RwLock<Option<Arc<TantivyBridge>>> {
    BRIDGE.get_or_init(|| RwLock::new(None))
}

fn same_index_dir(lhs: &Path, rhs: &Path) -> bool {
    match (lhs.canonicalize(), rhs.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => lhs == rhs,
    }
}

/// Initialize the global Tantivy bridge.
///
/// Should be called once at startup when `SearchEngine::Tantivy` or `Shadow`
/// is configured. Returns `Ok(())` on success.
pub fn init_bridge(index_dir: &Path) -> Result<(), String> {
    use crate::search_cache::WarmResource;
    use crate::search_service::{record_warmup, record_warmup_failure, record_warmup_start};

    record_warmup_start(WarmResource::LexicalIndex);
    let warmup_timer = std::time::Instant::now();
    if let Some(existing) = get_bridge() {
        if same_index_dir(existing.index_dir(), index_dir) {
            record_warmup(WarmResource::LexicalIndex, warmup_timer.elapsed());
            return Ok(());
        }
        let error = format!(
            "search bridge already initialized for {}; refusing to reinitialize for {}",
            existing.index_dir().display(),
            index_dir.display()
        );
        record_warmup_failure(WarmResource::LexicalIndex, &error);
        return Err(error);
    }
    let bridge = match TantivyBridge::open(index_dir) {
        Ok(b) => b,
        Err(e) => {
            record_warmup_failure(WarmResource::LexicalIndex, &e);
            return Err(e);
        }
    };
    let slot = bridge_slot();
    let mut guard = slot
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = guard.as_ref() {
        if same_index_dir(existing.index_dir(), index_dir) {
            drop(guard);
            record_warmup(WarmResource::LexicalIndex, warmup_timer.elapsed());
            return Ok(());
        }
        let error = format!(
            "search bridge already initialized for {}; refusing to reinitialize for {}",
            existing.index_dir().display(),
            index_dir.display()
        );
        drop(guard);
        record_warmup_failure(WarmResource::LexicalIndex, &error);
        return Err(error);
    }
    *guard = Some(Arc::new(bridge));
    drop(guard);
    record_warmup(WarmResource::LexicalIndex, warmup_timer.elapsed());
    Ok(())
}

/// Initialize the global Tantivy bridge, replacing an existing bridge when the
/// requested index directory differs.
pub fn init_or_switch_bridge(index_dir: &Path) -> Result<(), String> {
    use crate::search_cache::WarmResource;
    use crate::search_service::{record_warmup, record_warmup_failure, record_warmup_start};

    record_warmup_start(WarmResource::LexicalIndex);
    let warmup_timer = std::time::Instant::now();
    if let Some(existing) = get_bridge()
        && same_index_dir(existing.index_dir(), index_dir)
    {
        record_warmup(WarmResource::LexicalIndex, warmup_timer.elapsed());
        return Ok(());
    }

    let bridge = match TantivyBridge::open(index_dir) {
        Ok(b) => b,
        Err(e) => {
            record_warmup_failure(WarmResource::LexicalIndex, &e);
            return Err(e);
        }
    };

    let slot = bridge_slot();
    let mut guard = slot
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = guard.as_ref()
        && same_index_dir(existing.index_dir(), index_dir)
    {
        drop(guard);
        record_warmup(WarmResource::LexicalIndex, warmup_timer.elapsed());
        return Ok(());
    }
    *guard = Some(Arc::new(bridge));
    drop(guard);
    record_warmup(WarmResource::LexicalIndex, warmup_timer.elapsed());
    Ok(())
}

#[must_use]
pub fn is_bridge_initialized_for(index_dir: &Path) -> bool {
    get_bridge()
        .as_ref()
        .is_some_and(|bridge| same_index_dir(bridge.index_dir(), index_dir))
}

/// Get the global Tantivy bridge, if initialized.
pub fn get_bridge() -> Option<Arc<TantivyBridge>> {
    bridge_slot()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
pub(crate) fn reset_bridge_for_tests() {
    *bridge_slot()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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

fn add_indexable_message(
    writer: &tantivy::IndexWriter,
    handles: &FieldHandles,
    msg: &IndexableMessage,
) -> Result<(), String> {
    let id_u64 = u64::try_from(msg.id)
        .map_err(|_| format!("message id must be non-negative: {}", msg.id))?;
    let project_id_u64 = u64::try_from(msg.project_id)
        .map_err(|_| format!("project id must be non-negative: {}", msg.project_id))?;

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

    Ok(())
}

fn upsert_indexable_message(
    writer: &tantivy::IndexWriter,
    handles: &FieldHandles,
    msg: &IndexableMessage,
) -> Result<(), String> {
    let id_u64 = u64::try_from(msg.id)
        .map_err(|_| format!("message id must be non-negative: {}", msg.id))?;
    writer.delete_term(Term::from_field_u64(handles.id, id_u64));
    add_indexable_message(writer, handles, msg)
}

fn refresh_index_health_metrics(bridge: &TantivyBridge) {
    static LAST_MEASURED: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    let doc_count = bridge
        .index()
        .reader()
        .map_or(0, |reader| reader.searcher().num_docs());

    // Only perform the expensive recursive filesystem scan occasionally
    // to avoid blocking the synchronous message send path.
    let now = current_unix_micros();
    let last = LAST_MEASURED.load(std::sync::atomic::Ordering::Relaxed);

    // Measure at most once every 60 seconds
    let index_size_bytes = if now - last > 60_000_000 {
        let size = measure_index_dir_bytes(bridge.index_dir());
        LAST_MEASURED.store(now, std::sync::atomic::Ordering::Relaxed);
        size
    } else {
        // Fallback to the last known recorded metric value
        mcp_agent_mail_core::metrics::global_metrics()
            .search
            .tantivy_index_size_bytes
            .load()
    };

    mcp_agent_mail_core::metrics::global_metrics()
        .search
        .update_index_health(index_size_bytes, doc_count);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct MessageStats {
    count: u64,
    max_id: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct MessageWatermark {
    sequence: u64,
    max_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackfillPlan {
    Skip,
    Incremental { start_after_id: i64 },
    FullRebuild,
}

const BACKFILL_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BackfillDbFingerprint {
    len_bytes: u64,
    modified_micros: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct IndexMetaFingerprint {
    len_bytes: u64,
    modified_micros: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BackfillState {
    schema_version: u32,
    db_path: String,
    db_fingerprint: BackfillDbFingerprint,
    db_stats: MessageStats,
    #[serde(default)]
    message_watermark: MessageWatermark,
    #[serde(default)]
    index_meta_fingerprint: Option<IndexMetaFingerprint>,
    index_stats: MessageStats,
    updated_at_micros: i64,
}

fn backfill_state_path(bridge: &TantivyBridge) -> PathBuf {
    bridge.index_dir().join("backfill_state.json")
}

fn current_unix_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|dur| i64::try_from(dur.as_micros()).ok())
        .unwrap_or(0)
}

fn sqlite_file_backfill_fingerprint(db_path: &str) -> Option<BackfillDbFingerprint> {
    if db_path == ":memory:" {
        return None;
    }
    let metadata = std::fs::metadata(db_path).ok()?;
    let modified_micros = metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|dur| i64::try_from(dur.as_micros()).ok())
        .unwrap_or(0);
    Some(BackfillDbFingerprint {
        len_bytes: metadata.len(),
        modified_micros,
    })
}

fn index_meta_fingerprint(bridge: &TantivyBridge) -> Option<IndexMetaFingerprint> {
    let metadata = std::fs::metadata(bridge.index_dir().join("meta.json")).ok()?;
    let modified_micros = metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|dur| i64::try_from(dur.as_micros()).ok())
        .unwrap_or(0);
    Some(IndexMetaFingerprint {
        len_bytes: metadata.len(),
        modified_micros,
    })
}

fn read_backfill_state(bridge: &TantivyBridge) -> Option<BackfillState> {
    let path = backfill_state_path(bridge);
    let raw = std::fs::read_to_string(path).ok()?;
    let state = serde_json::from_str::<BackfillState>(&raw).ok()?;
    (state.schema_version == BACKFILL_STATE_SCHEMA_VERSION).then_some(state)
}

fn write_backfill_state(
    bridge: &TantivyBridge,
    db_path: &str,
    fingerprint: BackfillDbFingerprint,
    db_stats: MessageStats,
    message_watermark: MessageWatermark,
    index_meta_fingerprint: Option<IndexMetaFingerprint>,
    index_stats: MessageStats,
) {
    let path = backfill_state_path(bridge);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let state = BackfillState {
        schema_version: BACKFILL_STATE_SCHEMA_VERSION,
        db_path: db_path.to_string(),
        db_fingerprint: fingerprint,
        db_stats,
        message_watermark,
        index_meta_fingerprint,
        index_stats,
        updated_at_micros: current_unix_micros(),
    };
    let Ok(payload) = serde_json::to_string_pretty(&state) else {
        return;
    };
    let _ = std::fs::write(path, payload);
}

fn fetch_db_message_stats(conn: &DbConn) -> Result<MessageStats, String> {
    // Keep COUNT and MAX in separate scalar subqueries; FrankensQLite rejects
    // mixed aggregate/non-aggregate projections in one SELECT.
    // Also avoid wrapping MAX() with COALESCE() because FrankensQLite's current
    // aggregate planner can classify that shape as mixed aggregate/non-aggregate.
    let rows = match conn.query_sync(
        "SELECT \
             (SELECT COUNT(*) FROM messages) AS count, \
             (SELECT MAX(id) FROM messages) AS max_id",
        &[],
    ) {
        Ok(rows) => rows,
        Err(e) if sqlite_error_is_missing_table(&e.to_string(), "messages") => {
            return Ok(MessageStats::default());
        }
        Err(e) => return Err(format!("backfill stats query failed: {e}")),
    };
    let Some(row) = rows.first() else {
        return Ok(MessageStats::default());
    };

    let count_i64 = row.get_named::<i64>("count").unwrap_or(0).max(0);
    let max_id_i64 = row.get_named::<i64>("max_id").unwrap_or(0).max(0);

    Ok(MessageStats {
        count: u64::try_from(count_i64).unwrap_or(0),
        max_id: u64::try_from(max_id_i64).unwrap_or(0),
    })
}

fn fetch_db_message_watermark(conn: &DbConn) -> Result<MessageWatermark, String> {
    let max_id_rows = match conn.query_sync("SELECT MAX(id) AS max_id FROM messages", &[]) {
        Ok(rows) => rows,
        Err(e) if sqlite_error_is_missing_table(&e.to_string(), "messages") => {
            return Ok(MessageWatermark::default());
        }
        Err(e) => return Err(format!("backfill watermark max-id query failed: {e}")),
    };
    let max_id = max_id_rows
        .first()
        .and_then(|row| row.get_named::<i64>("max_id").ok())
        .and_then(|v| u64::try_from(v.max(0)).ok())
        .unwrap_or(0);

    let sequence = conn
        .query_sync(
            "SELECT seq FROM sqlite_sequence WHERE name = 'messages' LIMIT 1",
            &[],
        )
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|row| row.get_named::<i64>("seq").ok())
        })
        .and_then(|v| u64::try_from(v.max(0)).ok())
        // Fallback for legacy/malformed sqlite_sequence: max_id still gives a
        // monotonic watermark for append-only message IDs.
        .unwrap_or(max_id);

    Ok(MessageWatermark { sequence, max_id })
}

fn sqlite_error_is_missing_table(message: &str, table: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let table = table.to_ascii_lowercase();
    lower.contains(&format!("no such table: {table}"))
        || lower.contains(&format!("no such table: main.{table}"))
}

fn backfill_table_exists(conn: &DbConn, table: &str) -> Result<bool, String> {
    match conn.query_sync(&format!("SELECT 1 FROM {table} LIMIT 1"), &[]) {
        Ok(_) => Ok(true),
        Err(e) if sqlite_error_is_missing_table(&e.to_string(), table) => Ok(false),
        Err(e) => Err(format!("backfill table probe failed for {table}: {e}")),
    }
}

fn fetch_id_text_map(conn: &DbConn, sql: &str) -> Result<HashMap<i64, String>, String> {
    let rows = conn
        .query_sync(sql, &[])
        .map_err(|e| format!("backfill map query failed: {e}"))?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let id = row.get_named::<i64>("id").unwrap_or(0);
        let value = row.get_named::<String>("value").unwrap_or_default();
        out.insert(id, value);
    }
    Ok(out)
}

fn fetch_db_tail_count(conn: &DbConn, start_after_id: i64) -> Result<u64, String> {
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS count FROM messages WHERE id > ?",
            &[Value::BigInt(start_after_id)],
        )
        .map_err(|e| format!("backfill tail-count query failed: {e}"))?;
    let count_i64 = rows
        .first()
        .and_then(|row| row.get_named::<i64>("count").ok())
        .unwrap_or(0)
        .max(0);
    Ok(u64::try_from(count_i64).unwrap_or(0))
}

fn fetch_index_message_stats(bridge: &TantivyBridge) -> Result<MessageStats, String> {
    let reader = bridge
        .index()
        .reader()
        .map_err(|e| format!("backfill index reader error: {e}"))?;
    let searcher = reader.searcher();
    let handles = bridge.handles();
    let message_query = TermQuery::new(
        Term::from_field_text(handles.doc_kind, "message"),
        IndexRecordOption::Basic,
    );
    let count = searcher
        .search(&message_query, &Count)
        .map_err(|e| format!("backfill index count query failed: {e}"))?;
    if count == 0 {
        return Ok(MessageStats {
            count: 0,
            max_id: 0,
        });
    }
    let top_docs: Vec<(u64, tantivy::DocAddress)> = searcher
        .search(
            &message_query,
            &TopDocs::with_limit(1).order_by_fast_field::<u64>("id", Order::Desc),
        )
        .map_err(|e| format!("backfill index max-id query failed: {e}"))?;
    let max_id = top_docs.first().map_or(0, |(id, _)| *id);

    Ok(MessageStats {
        count: u64::try_from(count).unwrap_or(u64::MAX),
        max_id,
    })
}

fn choose_backfill_plan(
    conn: &DbConn,
    db: MessageStats,
    index: MessageStats,
) -> Result<BackfillPlan, String> {
    if db.count == 0 {
        return Ok(if index.count == 0 {
            BackfillPlan::Skip
        } else {
            // DB was cleared/reset — clear stale index docs too.
            BackfillPlan::FullRebuild
        });
    }

    if index.count == 0 {
        return Ok(BackfillPlan::FullRebuild);
    }

    if db.count == index.count && db.max_id == index.max_id {
        return Ok(BackfillPlan::Skip);
    }

    if db.max_id >= index.max_id && db.count >= index.count {
        let Ok(start_after_id) = i64::try_from(index.max_id) else {
            return Ok(BackfillPlan::FullRebuild);
        };
        let tail_count = fetch_db_tail_count(conn, start_after_id)?;
        if index.count.saturating_add(tail_count) == db.count {
            // Pure append since the last indexed id.
            return Ok(BackfillPlan::Incremental { start_after_id });
        }
    }

    // Any other shape implies deletes/resets/mismatch; rebuild is the safe path.
    Ok(BackfillPlan::FullRebuild)
}

/// Acquire an IndexWriter with retries. Tantivy acquires an exclusive directory lock
/// for writers. In concurrent environments, this can fail. We retry a few times
/// with exponential backoff to handle concurrent index updates.
fn acquire_writer_with_retry(index: &tantivy::Index) -> Result<tantivy::IndexWriter, String> {
    let mut retries = 5;
    let mut delay = std::time::Duration::from_millis(50);
    loop {
        match index.writer(15_000_000) {
            Ok(writer) => return Ok(writer),
            Err(e) => {
                if retries == 0 {
                    return Err(format!("Tantivy writer error (after retries): {e}"));
                }
                retries -= 1;
                std::thread::sleep(delay);
                delay *= 2; // Exponential backoff
            }
        }
    }
}

/// Index a single message into the global Tantivy bridge.
///
/// Returns `Ok(true)` if the message was indexed, `Ok(false)` if the bridge
/// is not initialized (search V3 disabled), or `Err` on write failure.
///
/// This is intentionally fire-and-forget safe: callers should not fail the
/// message send operation if indexing fails.
pub fn index_message(msg: &IndexableMessage) -> Result<bool, String> {
    let Some(bridge) = get_bridge() else {
        return Ok(false); // bridge not initialized, skip silently
    };

    let handles = bridge.handles();
    let mut writer = acquire_writer_with_retry(bridge.index())?;
    upsert_indexable_message(&writer, handles, msg)?;

    writer
        .commit()
        .map_err(|e| format!("Tantivy commit error: {e}"))?;

    refresh_index_health_metrics(&bridge);

    // Invalidate search cache so new messages appear immediately.
    crate::search_service::invalidate_search_cache(
        crate::search_cache::InvalidationTrigger::IndexUpdate,
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

    let Some(bridge) = get_bridge() else {
        return Ok(0);
    };

    let handles = bridge.handles();
    let mut writer = acquire_writer_with_retry(bridge.index())?;

    for msg in messages {
        upsert_indexable_message(&writer, handles, msg)?;
    }

    writer
        .commit()
        .map_err(|e| format!("Tantivy commit error: {e}"))?;

    refresh_index_health_metrics(&bridge);

    crate::search_service::invalidate_search_cache(
        crate::search_cache::InvalidationTrigger::IndexUpdate,
    );

    Ok(messages.len())
}

// ── Startup backfill ─────────────────────────────────────────────────────

pub(crate) fn resolve_search_sqlite_path_from_database_url(db_url: &str) -> Option<String> {
    crate::pool::resolve_mailbox_sqlite_path(db_url)
        .ok()
        .map(|resolved| resolved.canonical_path)
}

/// Backfill the Tantivy index with all messages from the database.
///
/// Uses a sync `DbConn` (`FrankenSQLite`) to scan the messages table joined with
/// agents and projects, then batch-indexes everything with a single writer.
/// The index is rebuilt from scratch on each backfill to guarantee that stale
/// or duplicate message documents cannot survive DB resets, migrations, or
/// interrupted runs.
///
/// Returns `(indexed_count, skipped_count)` where `skipped_count` is always 0.
#[allow(clippy::too_many_lines)]
pub fn backfill_from_db(db_url: &str) -> Result<(usize, usize), String> {
    const FETCH_BATCH_SIZE: i64 = 500;
    const COMMIT_EVERY_BATCHES: usize = 8;

    let Some(bridge) = get_bridge() else {
        return Ok((0, 0));
    };

    // Open a sync connection via FrankenSQLite.
    let db_path_owned = if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(db_url) {
        ":memory:".to_string()
    } else if let Some(path) = resolve_search_sqlite_path_from_database_url(db_url) {
        path
    } else {
        db_url.to_string()
    };
    let db_path = &db_path_owned;

    let db_fingerprint = sqlite_file_backfill_fingerprint(db_path);
    let initial_index_fingerprint = index_meta_fingerprint(&bridge);
    if let (Some(fingerprint), Some(state)) = (db_fingerprint, read_backfill_state(&bridge))
        && state.db_path == *db_path
        && state.db_fingerprint == fingerprint
        && state.index_meta_fingerprint == initial_index_fingerprint
    {
        tracing::info!(
            db_count = state.db_stats.count,
            db_max_id = state.db_stats.max_id,
            index_count = state.index_stats.count,
            index_max_id = state.index_stats.max_id,
            "backfill: sqlite/index meta fingerprints unchanged, skipping"
        );
        refresh_index_health_metrics(&bridge);
        return Ok((
            0,
            usize::try_from(state.db_stats.count).unwrap_or(usize::MAX),
        ));
    }

    let conn = crate::guard_db_conn(
        DbConn::open_file(db_path)
            .map_err(|e| format!("backfill: cannot open DB {db_path}: {e}"))?,
        "search backfill connection",
    );

    if !backfill_table_exists(&conn, "messages")? {
        let index_stats = fetch_index_message_stats(&bridge)?;
        if index_stats.count > 0 {
            let mut writer: tantivy::IndexWriter<TantivyDocument> = bridge
                .index()
                .writer(15_000_000)
                .map_err(|e| format!("Tantivy writer error: {e}"))?;
            writer
                .delete_all_documents()
                .map_err(|e| format!("Tantivy delete_all_documents error: {e}"))?;
            writer
                .commit()
                .map_err(|e| format!("Tantivy commit error: {e}"))?;
            crate::search_service::invalidate_search_cache(
                crate::search_cache::InvalidationTrigger::IndexUpdate,
            );
        }
        tracing::info!("backfill: messages table missing, treating database as empty");
        refresh_index_health_metrics(&bridge);
        return Ok((0, 0));
    }

    let message_watermark = fetch_db_message_watermark(&conn)?;
    let current_index_fingerprint = index_meta_fingerprint(&bridge);
    if let Some(state) = read_backfill_state(&bridge)
        && state.db_path == *db_path
        && state.message_watermark == message_watermark
        && state.index_meta_fingerprint == current_index_fingerprint
    {
        tracing::info!(
            message_seq = message_watermark.sequence,
            message_max_id = message_watermark.max_id,
            "backfill: message watermark unchanged, skipping"
        );
        refresh_index_health_metrics(&bridge);
        return Ok((
            0,
            usize::try_from(state.db_stats.count).unwrap_or(usize::MAX),
        ));
    }

    let db_stats = fetch_db_message_stats(&conn)?;
    let index_stats = fetch_index_message_stats(&bridge)?;
    let plan = choose_backfill_plan(&conn, db_stats, index_stats)?;

    if matches!(plan, BackfillPlan::Skip) {
        tracing::info!(
            db_count = db_stats.count,
            db_max_id = db_stats.max_id,
            index_count = index_stats.count,
            index_max_id = index_stats.max_id,
            "backfill: Tantivy index already up-to-date, skipping"
        );
        if let Some(fingerprint) = sqlite_file_backfill_fingerprint(db_path) {
            write_backfill_state(
                &bridge,
                db_path,
                fingerprint,
                db_stats,
                message_watermark,
                current_index_fingerprint,
                index_stats,
            );
        }
        refresh_index_health_metrics(&bridge);
        return Ok((0, usize::try_from(db_stats.count).unwrap_or(usize::MAX)));
    }

    let mut writer = bridge
        .index()
        .writer(15_000_000)
        .map_err(|e| format!("Tantivy writer error: {e}"))?;
    let handles = bridge.handles();
    if matches!(plan, BackfillPlan::FullRebuild) {
        writer
            .delete_all_documents()
            .map_err(|e| format!("Tantivy delete_all_documents error: {e}"))?;
    }

    // Paged reads avoid loading the full mailbox into memory during startup.
    // Keep this query JOIN-free to avoid parity-cert fallback overhead on
    // FrankenSQLite for join-heavy startup scans.
    let sql = "SELECT id, project_id, sender_id, subject, body_md, \
               thread_id, importance, created_ts \
               FROM messages \
               WHERE id > ? \
               ORDER BY id \
               LIMIT ?";
    let sender_name_map = fetch_id_text_map(&conn, "SELECT id, name AS value FROM agents")?;
    let project_slug_map = fetch_id_text_map(&conn, "SELECT id, slug AS value FROM projects")?;

    let mut last_id = match plan {
        BackfillPlan::Incremental { start_after_id } => start_after_id,
        BackfillPlan::Skip | BackfillPlan::FullRebuild => 0_i64,
    };
    let mut pending_batches = 0_usize;
    let mut total_indexed = 0_usize;
    loop {
        let rows = conn
            .query_sync(
                sql,
                &[Value::BigInt(last_id), Value::BigInt(FETCH_BATCH_SIZE)],
            )
            .map_err(|e| format!("backfill: query failed: {e}"))?;
        if rows.is_empty() {
            break;
        }

        for row in &rows {
            let project_id = row.get_as::<i64>(1).unwrap_or(0);
            let sender_id = row.get_as::<i64>(2).unwrap_or(0);
            let project_slug = project_slug_map
                .get(&project_id)
                .cloned()
                .unwrap_or_default();
            let sender_name = sender_name_map.get(&sender_id).cloned().unwrap_or_default();
            let msg = IndexableMessage {
                id: row.get_as::<i64>(0).unwrap_or(0),
                project_id,
                project_slug,
                sender_name,
                subject: row.get_as::<String>(3).unwrap_or_default(),
                body_md: row.get_as::<String>(4).unwrap_or_default(),
                thread_id: row.get_as::<Option<String>>(5).unwrap_or_default(),
                importance: row
                    .get_as::<String>(6)
                    .unwrap_or_else(|_| "normal".to_string()),
                created_ts: row.get_as::<i64>(7).unwrap_or(0),
            };
            add_indexable_message(&writer, handles, &msg)?;
            total_indexed += 1;
            if msg.id > last_id {
                last_id = msg.id;
            }
        }

        pending_batches += 1;
        if pending_batches >= COMMIT_EVERY_BATCHES {
            writer
                .commit()
                .map_err(|e| format!("Tantivy commit error: {e}"))?;
            pending_batches = 0;
        }
    }

    if pending_batches > 0 || (matches!(plan, BackfillPlan::FullRebuild) && total_indexed == 0) {
        writer
            .commit()
            .map_err(|e| format!("Tantivy commit error: {e}"))?;
    }

    refresh_index_health_metrics(&bridge);
    crate::search_service::invalidate_search_cache(
        crate::search_cache::InvalidationTrigger::IndexUpdate,
    );

    let final_index_stats = fetch_index_message_stats(&bridge).unwrap_or(db_stats);
    if let Some(fingerprint) = sqlite_file_backfill_fingerprint(db_path) {
        write_backfill_state(
            &bridge,
            db_path,
            fingerprint,
            db_stats,
            message_watermark,
            index_meta_fingerprint(&bridge),
            final_index_stats,
        );
    }

    match plan {
        BackfillPlan::Incremental { start_after_id } => tracing::info!(
            total_indexed,
            start_after_id,
            db_count = db_stats.count,
            index_count_before = index_stats.count,
            "backfill: incrementally indexed new messages"
        ),
        BackfillPlan::FullRebuild => tracing::info!(
            total_indexed,
            db_count = db_stats.count,
            index_count_before = index_stats.count,
            "backfill: Tantivy index rebuilt from database"
        ),
        BackfillPlan::Skip => {}
    }

    Ok((total_indexed, 0))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static BRIDGE_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
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
    fn concurrent_writer_behavior() {
        let dir = tempfile::TempDir::new().unwrap();
        let bridge = TantivyBridge::open(dir.path()).unwrap();
        let _writer1 = bridge
            .index()
            .writer::<TantivyDocument>(15_000_000)
            .unwrap();
        let writer2_res = bridge.index().writer::<TantivyDocument>(15_000_000);
        assert!(
            writer2_res.is_err(),
            "second writer should fail with lock error"
        );
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
        assert_eq!(
            results.len(),
            2,
            "Empty query with project filter should return all project documents"
        );
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
            direction: Some(Direction::Outbox),
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert_eq!(filter.doc_kind, Some(mcp_agent_mail_core::DocKind::Message));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert_eq!(filter.doc_kind, Some(mcp_agent_mail_core::DocKind::Agent));
    }

    #[test]
    fn filter_project_doc_kind() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Project,
            ..Default::default()
        };
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert_eq!(filter.doc_kind, Some(mcp_agent_mail_core::DocKind::Project));
    }

    #[test]
    fn filter_thread_doc_kind() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Thread,
            ..Default::default()
        };
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert_eq!(filter.doc_kind, Some(mcp_agent_mail_core::DocKind::Thread));
    }

    #[test]
    fn filter_with_sender() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            direction: Some(Direction::Outbox),
            agent_name: Some("BlueLake".to_string()),
            ..Default::default()
        };
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert_eq!(filter.sender.as_deref(), Some("BlueLake"));
    }

    #[test]
    fn filter_with_agent_name_without_direction_requires_post_filter() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            agent_name: Some("BlueLake".to_string()),
            ..Default::default()
        };
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert!(filter.sender.is_none());
    }

    #[test]
    fn filter_with_agent_name_inbox_requires_post_filter() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            direction: Some(Direction::Inbox),
            agent_name: Some("BlueLake".to_string()),
            ..Default::default()
        };
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert!(filter.sender.is_none());
    }

    #[test]
    fn filter_with_thread_id() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            thread_id: Some("br-42".to_string()),
            ..Default::default()
        };
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        assert!(filter.date_range.is_none());
    }

    #[test]
    fn importance_plan_high_only_requires_post_filter() {
        let query = PlannerQuery {
            text: "test".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::High],
            ..Default::default()
        };
        let plan = build_importance_filter_plan(&query);
        assert_eq!(plan.filter, Some(ImportanceFilter::High));
        assert!(plan.needs_post_filter);
        assert_eq!(
            plan.exact_importances.expect("importance set"),
            BTreeSet::from(["high"])
        );
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
        let filter = build_search_filter(&query, &build_importance_filter_plan(&query));
        let date_range = filter.date_range.expect("should have date_range");
        assert_eq!(date_range.start, Some(1_000_000));
        assert!(date_range.end.is_none());
    }

    // -- convert_results tests ---------------------------------------------

    fn make_search_results(hits: Vec<mcp_agent_mail_core::SearchHit>) -> SearchResults {
        use mcp_agent_mail_core::SearchMode;
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
    ) -> mcp_agent_mail_core::SearchHit {
        use mcp_agent_mail_core::DocKind as CoreDocKind;
        mcp_agent_mail_core::SearchHit {
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

    #[test]
    fn init_bridge_rejects_different_index_dir_after_first_init() {
        let _guard = BRIDGE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_bridge_for_tests();
        let temp_a = tempfile::tempdir().expect("tempdir a");
        let temp_b = tempfile::tempdir().expect("tempdir b");

        init_bridge(temp_a.path()).expect("init first bridge");
        init_bridge(temp_a.path()).expect("reinit same path");
        let err = init_bridge(temp_b.path()).expect_err("reject different bridge path");

        assert!(err.contains("already initialized"));
        assert_eq!(
            get_bridge()
                .expect("bridge should stay initialized")
                .index_dir(),
            temp_a.path()
        );
        reset_bridge_for_tests();
    }

    #[test]
    fn init_or_switch_bridge_replaces_different_index_dir() {
        let _guard = BRIDGE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_bridge_for_tests();
        let temp_a = tempfile::tempdir().expect("tempdir a");
        let temp_b = tempfile::tempdir().expect("tempdir b");

        init_bridge(temp_a.path()).expect("init first bridge");
        init_or_switch_bridge(temp_b.path()).expect("switch bridge path");

        assert_eq!(
            get_bridge()
                .expect("bridge should stay initialized")
                .index_dir(),
            temp_b.path()
        );
        reset_bridge_for_tests();
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
        // If another test already initialized the process-global bridge,
        // index_message may legitimately return Ok(true) instead.
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

        let msg = make_indexable(
            100,
            "Indexing test subject",
            "Body about database migration",
        );

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
    fn search_with_empty_text_and_project_filter_returns_filtered_results() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let project_one = make_indexable(1, "Alpha subject", "first body");
        let mut project_two = make_indexable(2, "Beta subject", "second body");
        project_two.project_id = 2;
        project_two.project_slug = "other-project".to_string();
        project_two.thread_id = Some("thread-2".to_string());

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        for msg in [&project_one, &project_two] {
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

        let query = PlannerQuery {
            text: String::new(),
            doc_kind: DocKind::Message,
            project_id: Some(2),
            ..Default::default()
        };
        let results = bridge.search(&query);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 2);
        assert_eq!(results[0].project_id, Some(2));
    }

    #[test]
    fn upsert_indexable_message_replaces_previous_document_by_id() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();
        let mut writer = bridge.index().writer(15_000_000).unwrap();

        let first = make_indexable(99, "Legacy subject", "legacy token alpha");
        let second = make_indexable(99, "Canonical subject", "canonical token beta");

        upsert_indexable_message(&writer, handles, &first).unwrap();
        upsert_indexable_message(&writer, handles, &second).unwrap();
        writer.commit().unwrap();

        let reader = bridge.index().reader().unwrap();
        assert_eq!(
            reader.searcher().num_docs(),
            1,
            "upsert must leave exactly one live document per id"
        );

        let beta_query = PlannerQuery {
            text: "canonical token".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(1),
            ..Default::default()
        };
        let beta_results = bridge.search(&beta_query);
        assert_eq!(beta_results.len(), 1);
        assert_eq!(beta_results[0].id, 99);
        assert_eq!(beta_results[0].title, "Canonical subject");

        let legacy_query = PlannerQuery {
            text: "legacy token".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(1),
            ..Default::default()
        };
        let legacy_results = bridge.search(&legacy_query);
        assert!(
            legacy_results.is_empty(),
            "legacy document should be replaced"
        );
    }

    #[test]
    fn upsert_indexable_message_rejects_negative_id() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();
        let writer = bridge.index().writer(15_000_000).unwrap();

        let mut invalid = make_indexable(1, "x", "y");
        invalid.id = -1;

        let result = upsert_indexable_message(&writer, handles, &invalid);
        assert!(result.is_err());
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

    // ── Backfill tests ──────────────────────────────────────────────────────

    /// Helper: create a temp `SQLite` DB with the minimal schema needed for
    /// `backfill_from_db` (projects, agents, messages tables).
    fn create_test_db(dir: &std::path::Path, messages: &[(i64, &str, &str, &str, &str)]) -> String {
        let db_path = dir.join("test.sqlite3");
        let path_str = db_path.to_str().unwrap();
        let conn = DbConn::open_file(path_str).unwrap();

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, \
             human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test-proj', 'test', 0)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, \
             name TEXT NOT NULL, program TEXT NOT NULL DEFAULT '', \
             model TEXT NOT NULL DEFAULT '', task_description TEXT NOT NULL DEFAULT '', \
             inception_ts INTEGER NOT NULL DEFAULT 0, last_active_ts INTEGER NOT NULL DEFAULT 0, \
             attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 1, 'BlueLake')",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, \
             thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, \
             importance TEXT NOT NULL DEFAULT 'normal', ack_required INTEGER NOT NULL DEFAULT 0, \
             created_ts INTEGER NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
            &[],
        )
        .unwrap();

        for (id, subject, body, importance, thread_id) in messages {
            use sqlmodel_core::Value;
            conn.execute_sync(
                "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, created_ts) \
                 VALUES (?, 1, 1, ?, ?, ?, ?, 1000000)",
                &[
                    Value::BigInt(*id),
                    Value::Text(thread_id.to_string()),
                    Value::Text(subject.to_string()),
                    Value::Text(body.to_string()),
                    Value::Text(importance.to_string()),
                ],
            )
            .unwrap();
        }

        path_str.to_string()
    }

    #[test]
    fn backfill_from_db_empty_database() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = create_test_db(tmp.path(), &[]);

        // backfill_from_db requires the global bridge to be set.
        // Without the bridge, it returns (0, 0) immediately.
        let result = backfill_from_db(&db_path);
        assert!(result.is_ok());
        let (indexed, _skipped) = result.unwrap();
        assert_eq!(indexed, 0, "empty DB should index 0 messages");
    }

    #[test]
    fn backfill_from_db_nonexistent_file_returns_error() {
        let result = backfill_from_db("/tmp/nonexistent_test_backfill_db.sqlite3");
        // Should return Ok((0,0)) when bridge is not set, or error if bridge is set
        // but DB doesn't exist.
        assert!(result.is_ok() || result.is_err());
        if let Err(e) = &result {
            assert!(
                e.contains("cannot open DB"),
                "error should mention DB open failure: {e}"
            );
        }
    }

    #[test]
    fn backfill_from_db_with_sqlite_url_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = create_test_db(tmp.path(), &[]);

        // Test with sqlite:// prefix — backfill should strip it.
        let url = format!("sqlite://{db_path}");
        let result = backfill_from_db(&url);
        assert!(result.is_ok());
    }

    #[test]
    fn backfill_from_db_with_sqlite_triple_slash_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = create_test_db(tmp.path(), &[]);

        // Test with sqlite:/// prefix.
        let url = format!("sqlite:///{db_path}");
        let result = backfill_from_db(&url);
        assert!(result.is_ok());
    }

    #[test]
    fn fetch_db_message_watermark_handles_empty_messages_without_coalesce() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT)",
            &[],
        )
        .expect("create messages");

        let watermark = fetch_db_message_watermark(&conn).expect("watermark query");
        assert_eq!(watermark.max_id, 0);
        assert_eq!(watermark.sequence, 0);
    }

    #[test]
    fn fetch_db_message_watermark_treats_missing_messages_table_as_empty() {
        let conn = DbConn::open_memory().expect("open in-memory db");

        let watermark = fetch_db_message_watermark(&conn).expect("watermark query");
        assert_eq!(watermark.max_id, 0);
        assert_eq!(watermark.sequence, 0);
    }

    #[test]
    fn fetch_db_message_stats_handles_empty_messages_without_coalesce() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT)",
            &[],
        )
        .expect("create messages");

        let stats = fetch_db_message_stats(&conn).expect("stats query");
        assert_eq!(stats.count, 0);
        assert_eq!(stats.max_id, 0);
    }

    #[test]
    fn fetch_db_message_stats_treats_missing_messages_table_as_empty() {
        let conn = DbConn::open_memory().expect("open in-memory db");

        let stats = fetch_db_message_stats(&conn).expect("stats query");
        assert_eq!(stats.count, 0);
        assert_eq!(stats.max_id, 0);
    }

    // ── Batch indexing edge-case tests ──────────────────────────────────────

    #[test]
    fn batch_index_empty_fields_do_not_crash() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let msg = IndexableMessage {
            id: 1,
            project_id: 0,
            project_slug: String::new(),
            sender_name: String::new(),
            subject: String::new(),
            body_md: String::new(),
            thread_id: None,
            importance: String::new(),
            created_ts: 0,
        };

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

        let reader = bridge.index().reader().unwrap();
        assert_eq!(
            reader.searcher().num_docs(),
            1,
            "empty-field message should still index"
        );
    }

    #[test]
    fn batch_index_duplicate_ids_creates_separate_docs() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        for _ in 0..3 {
            writer
                .add_document(doc!(
                    handles.id => 1u64,
                    handles.doc_kind => "message",
                    handles.subject => "Same ID",
                    handles.body => "Same body",
                    handles.sender => "Agent",
                    handles.project_slug => "proj",
                    handles.project_id => 1u64,
                    handles.thread_id => "",
                    handles.importance => "normal",
                    handles.created_ts => 0i64
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let reader = bridge.index().reader().unwrap();
        // Tantivy doesn't enforce uniqueness on `id` — all 3 docs are stored.
        assert_eq!(reader.searcher().num_docs(), 3);
    }

    #[test]
    fn batch_index_many_messages_searchable() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        let topics = [
            "database migration",
            "API endpoint",
            "authentication flow",
            "deployment pipeline",
            "error handling",
        ];
        for (i, topic) in topics.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let id = (i + 1) as u64;
            writer
                .add_document(doc!(
                    handles.id => id,
                    handles.doc_kind => "message",
                    handles.subject => format!("Topic: {topic}"),
                    handles.body => format!("Detailed discussion about {topic} improvements"),
                    handles.sender => "TestAgent",
                    handles.project_slug => "backend",
                    handles.project_id => 1u64,
                    handles.thread_id => format!("thread-{id}"),
                    handles.importance => "normal",
                    handles.created_ts => i64::try_from(i).unwrap_or(0) * 1_000_000
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let reader = bridge.index().reader().unwrap();
        assert_eq!(reader.searcher().num_docs(), 5);

        // Search for specific topic.
        let query = PlannerQuery {
            text: "authentication".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert!(
            !results.is_empty(),
            "should find message about authentication"
        );
        assert_eq!(results[0].id, 3, "authentication message has id=3");
    }

    #[test]
    fn batch_index_importance_filter_after_indexing() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        let importances = ["normal", "high", "urgent", "low"];
        for (i, imp) in importances.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let id = (i + 1) as u64;
            writer
                .add_document(doc!(
                    handles.id => id,
                    handles.doc_kind => "message",
                    handles.subject => format!("Message with {imp} importance"),
                    handles.body => format!("Body with {imp} content"),
                    handles.sender => "Agent",
                    handles.project_slug => "proj",
                    handles.project_id => 1u64,
                    handles.thread_id => "",
                    handles.importance => *imp,
                    handles.created_ts => 0i64
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        // Search with importance filter.
        let query = PlannerQuery {
            text: "importance".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::Urgent],
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert!(!results.is_empty(), "should find urgent messages");
        // All matching results should have urgent importance.
        for r in &results {
            assert_eq!(
                r.importance.as_deref(),
                Some("urgent"),
                "importance filter should only return urgent"
            );
        }
    }

    #[test]
    fn batch_index_high_only_filter_excludes_urgent_matches() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        for (id, importance) in [(1_u64, "high"), (2_u64, "urgent"), (3_u64, "high")] {
            writer
                .add_document(doc!(
                    handles.id => id,
                    handles.doc_kind => "message",
                    handles.subject => "importance exactness",
                    handles.body => "importance exactness body",
                    handles.sender => "Agent",
                    handles.project_slug => "proj",
                    handles.project_id => 1u64,
                    handles.thread_id => "",
                    handles.importance => importance,
                    handles.created_ts => 0i64
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let query = PlannerQuery {
            text: "importance".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::High],
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert_eq!(
            results.len(),
            2,
            "only high-importance documents should remain"
        );
        assert!(
            results
                .iter()
                .all(|result| result.importance.as_deref() == Some("high"))
        );
    }

    #[test]
    fn batch_index_mixed_importance_filter_returns_exact_requested_set() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        for (id, importance) in [
            (1_u64, "normal"),
            (2_u64, "high"),
            (3_u64, "urgent"),
            (4_u64, "low"),
        ] {
            writer
                .add_document(doc!(
                    handles.id => id,
                    handles.doc_kind => "message",
                    handles.subject => "importance mixed",
                    handles.body => "importance mixed body",
                    handles.sender => "Agent",
                    handles.project_slug => "proj",
                    handles.project_id => 1u64,
                    handles.thread_id => "",
                    handles.importance => importance,
                    handles.created_ts => 0i64
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let query = PlannerQuery {
            text: "importance".to_string(),
            doc_kind: DocKind::Message,
            importance: vec![Importance::High, Importance::Low],
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        let returned: BTreeSet<&str> = results
            .iter()
            .map(|result| result.importance.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(returned, BTreeSet::from(["high", "low"]));
    }

    #[test]
    fn batch_index_sender_filter_after_indexing() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        let senders = ["AlphaAgent", "BetaAgent", "AlphaAgent"];
        for (i, sender) in senders.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let id = (i + 1) as u64;
            writer
                .add_document(doc!(
                    handles.id => id,
                    handles.doc_kind => "message",
                    handles.subject => format!("From {sender}"),
                    handles.body => "Search engine testing content",
                    handles.sender => *sender,
                    handles.project_slug => "proj",
                    handles.project_id => 1u64,
                    handles.thread_id => "",
                    handles.importance => "normal",
                    handles.created_ts => 0i64
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        // Search with agent_name filter.
        let query = PlannerQuery {
            text: "search engine".to_string(),
            doc_kind: DocKind::Message,
            direction: Some(Direction::Outbox),
            agent_name: Some("AlphaAgent".to_string()),
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert_eq!(results.len(), 2, "should find 2 messages from AlphaAgent");
        for r in &results {
            assert_eq!(
                r.from_agent.as_deref(),
                Some("AlphaAgent"),
                "agent_name filter should only return AlphaAgent"
            );
        }
    }

    #[test]
    fn batch_index_project_filter_isolates_projects() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        // Index messages in two different projects.
        for project_id in [1u64, 2u64] {
            writer
                .add_document(doc!(
                    handles.id => project_id * 100,
                    handles.doc_kind => "message",
                    handles.subject => "Shared topic across projects",
                    handles.body => "Content mentioning deployment pipeline",
                    handles.sender => "Agent",
                    handles.project_slug => format!("project-{project_id}"),
                    handles.project_id => project_id,
                    handles.thread_id => "",
                    handles.importance => "normal",
                    handles.created_ts => 0i64
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        // Search with project_id=1 should only find that project's message.
        let query = PlannerQuery {
            text: "deployment".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 100);
    }

    #[test]
    fn batch_index_thread_id_filter() {
        let bridge = TantivyBridge::in_memory();
        let handles = bridge.handles();

        let mut writer = bridge.index().writer(15_000_000).unwrap();
        for i in 1..=4u64 {
            let thread = if i <= 2 { "thread-A" } else { "thread-B" };
            writer
                .add_document(doc!(
                    handles.id => i,
                    handles.doc_kind => "message",
                    handles.subject => format!("Message {i}"),
                    handles.body => "Relevant content for search",
                    handles.sender => "Agent",
                    handles.project_slug => "proj",
                    handles.project_id => 1u64,
                    handles.thread_id => thread,
                    handles.importance => "normal",
                    handles.created_ts => 0i64
                ))
                .unwrap();
        }
        writer.commit().unwrap();

        let query = PlannerQuery {
            text: "relevant content".to_string(),
            doc_kind: DocKind::Message,
            thread_id: Some("thread-A".to_string()),
            project_id: Some(1),
            ..Default::default()
        };
        let results = bridge.search(&query);
        assert_eq!(
            results.len(),
            2,
            "thread filter should return 2 messages from thread-A"
        );
    }

    #[test]
    fn resolve_search_sqlite_path_from_database_url_uses_absolute_candidate_when_relative_path_is_missing()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = dir.path().join("backfill-missing-relative.sqlite3");
        std::fs::write(&absolute_db, b"seed").expect("write absolute db");

        let relative_path = absolute_db
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string();
        let relative_candidate = PathBuf::from(&relative_path);
        assert!(
            !relative_candidate.exists(),
            "relative shadow path should be absent so search backfill resolves the absolute candidate"
        );

        let db_url = format!("sqlite:///{}", relative_path);
        let resolved =
            resolve_search_sqlite_path_from_database_url(&db_url).expect("resolve search path");
        assert_eq!(
            resolved,
            absolute_db.to_string_lossy(),
            "search backfill should open the existing absolute candidate"
        );
    }

    #[test]
    fn backfill_url_path_extraction() {
        let cases = [
            ("sqlite+aiosqlite:///absolute/path.db", "/absolute/path.db"),
            ("sqlite://relative/path.db", "relative/path.db"),
            ("sqlite:////abs/path.db", "/abs/path.db"),
            ("/plain/path.db", "/plain/path.db"),
            ("path.db", "path.db"),
            ("sqlite:///:memory:", ":memory:"),
        ];
        for (input, expected) in &cases {
            let extracted = if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(input) {
                ":memory:".to_string()
            } else if let Some(path) =
                mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(input)
            {
                crate::pool::normalize_sqlite_path_for_pool_key(path.to_string_lossy().as_ref())
            } else {
                input.to_string()
            };
            assert_eq!(
                extracted, *expected,
                "URL prefix extraction failed for {input}"
            );
        }
    }
}
