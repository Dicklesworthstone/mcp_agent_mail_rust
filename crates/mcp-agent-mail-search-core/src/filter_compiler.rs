//! Metadata filter compiler for Tantivy lexical search
//!
//! Compiles [`SearchFilter`] into Tantivy `Box<dyn Query>` clauses that can be
//! combined with full-text queries via `BooleanQuery`. Filters target exact-match
//! (STRING) and fast (FAST) fields:
//! - `sender` — exact agent name match
//! - `project_id` — exact project ID match
//! - `thread_id` — exact thread ID match
//! - `importance` — exact importance level or set membership
//! - `doc_kind` — exact document kind filter
//! - `date_range` — timestamp range query on `created_ts`

#[cfg(feature = "tantivy-engine")]
use tantivy::Term;
#[cfg(feature = "tantivy-engine")]
use tantivy::query::{BooleanQuery, Occur, Query, RangeQuery, TermQuery};
#[cfg(feature = "tantivy-engine")]
use tantivy::schema::{Field, IndexRecordOption};

#[cfg(any(feature = "tantivy-engine", test))]
use crate::document::DocKind;
use crate::query::{ImportanceFilter, SearchFilter};

#[cfg(feature = "tantivy-engine")]
use crate::tantivy_schema::FieldHandles;

// ── Filter compiler (behind feature gate) ───────────────────────────────────

/// Compiled filter: a list of `(Occur, Query)` clauses ready to be merged
/// into a `BooleanQuery` alongside the full-text query.
#[cfg(feature = "tantivy-engine")]
#[derive(Debug)]
pub struct CompiledFilters {
    clauses: Vec<(Occur, Box<dyn Query>)>,
}

#[cfg(feature = "tantivy-engine")]
impl CompiledFilters {
    /// Returns `true` if no filter clauses were produced.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }

    /// Number of filter clauses.
    #[must_use]
    pub fn len(&self) -> usize {
        self.clauses.len()
    }

    /// Consume self and return the raw clauses.
    #[must_use]
    pub fn into_clauses(self) -> Vec<(Occur, Box<dyn Query>)> {
        self.clauses
    }

    /// Wrap an existing query with these filter clauses.
    ///
    /// If there are no filters, returns the query unchanged.
    /// Otherwise, creates a `BooleanQuery` with the original query as `Must`
    /// and all filter clauses as `Must`.
    #[must_use]
    pub fn apply_to(self, query: Box<dyn Query>) -> Box<dyn Query> {
        if self.clauses.is_empty() {
            return query;
        }

        let mut all_clauses = Vec::with_capacity(1 + self.clauses.len());
        all_clauses.push((Occur::Must, query));
        all_clauses.extend(self.clauses);
        Box::new(BooleanQuery::new(all_clauses))
    }
}

/// Compile a `SearchFilter` into Tantivy filter clauses.
///
/// Each non-`None` filter field produces a `Must` clause that restricts results.
/// All filters are combined with AND semantics.
#[cfg(feature = "tantivy-engine")]
#[must_use]
pub fn compile_filters(filter: &SearchFilter, handles: &FieldHandles) -> CompiledFilters {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    // Sender filter (exact match on STRING field)
    if let Some(ref sender) = filter.sender {
        clauses.push(term_filter(handles.sender, sender));
    }

    // Project ID filter
    if let Some(project_id) = filter.project_id {
        #[allow(clippy::cast_sign_loss)]
        let uid = project_id as u64;
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_u64(handles.project_id, uid),
                IndexRecordOption::Basic,
            )),
        ));
    }

    // Thread ID filter (exact match on STRING field)
    if let Some(ref thread_id) = filter.thread_id {
        clauses.push(term_filter(handles.thread_id, thread_id));
    }

    // Document kind filter
    if let Some(ref doc_kind) = filter.doc_kind {
        let kind_str = match doc_kind {
            DocKind::Message => "message",
            DocKind::Agent => "agent",
            DocKind::Project => "project",
            DocKind::Thread => "thread",
        };
        clauses.push(term_filter(handles.doc_kind, kind_str));
    }

    // Importance filter
    if let Some(importance) = filter.importance {
        if let Some(clause) = importance_filter(handles.importance, importance) {
            clauses.push(clause);
        }
    }

    // Date range filter
    if let Some(ref date_range) = filter.date_range {
        if let Some(clause) = date_range_filter(handles.created_ts, date_range) {
            clauses.push(clause);
        }
    }

    CompiledFilters { clauses }
}

/// Build a `Must` clause for an exact-match STRING field.
#[cfg(feature = "tantivy-engine")]
fn term_filter(field: Field, value: &str) -> (Occur, Box<dyn Query>) {
    (
        Occur::Must,
        Box::new(TermQuery::new(
            Term::from_field_text(field, value),
            IndexRecordOption::Basic,
        )),
    )
}

/// Build an importance filter clause.
///
/// `Any` produces no filter. `Urgent` matches only "urgent".
/// `High` matches "urgent" OR "high". `Normal` matches "normal". `Low` matches "low".
#[cfg(feature = "tantivy-engine")]
fn importance_filter(field: Field, filter: ImportanceFilter) -> Option<(Occur, Box<dyn Query>)> {
    match filter {
        ImportanceFilter::Any => None,
        ImportanceFilter::Urgent => Some(term_filter(field, "urgent")),
        ImportanceFilter::Normal => Some(term_filter(field, "normal")),
        ImportanceFilter::Low => Some(term_filter(field, "low")),
        ImportanceFilter::High => {
            // "high" means urgent OR high
            let clause: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, "urgent"),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                ),
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, "high"),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                ),
            ]));
            Some((Occur::Must, clause))
        }
    }
}

/// Build a date range filter clause on an i64 fast field.
///
/// Uses Tantivy `RangeQuery` for efficient range scanning.
/// Both bounds are inclusive. If both are `None`, no filter is produced.
#[cfg(feature = "tantivy-engine")]
fn date_range_filter(
    field: Field,
    range: &crate::query::DateRange,
) -> Option<(Occur, Box<dyn Query>)> {
    let start = range.start.unwrap_or(i64::MIN);
    let end = range.end.unwrap_or(i64::MAX);

    if start == i64::MIN && end == i64::MAX {
        return None;
    }

    let lower = Term::from_field_i64(field, start);
    let upper = Term::from_field_i64(field, end);
    let range_query = RangeQuery::new(
        std::ops::Bound::Included(lower),
        std::ops::Bound::Included(upper),
    );

    Some((Occur::Must, Box::new(range_query)))
}

// ── Engine-independent filter validation ────────────────────────────────────

/// Check if a `SearchFilter` has any active (non-default) filter conditions.
#[must_use]
pub fn has_active_filters(filter: &SearchFilter) -> bool {
    filter.sender.is_some()
        || filter.project_id.is_some()
        || filter.thread_id.is_some()
        || filter.doc_kind.is_some()
        || filter.date_range.is_some()
        || filter
            .importance
            .as_ref()
            .is_some_and(|i| *i != ImportanceFilter::Any)
}

/// Count the number of active filter conditions.
#[must_use]
pub fn active_filter_count(filter: &SearchFilter) -> usize {
    let mut count = 0;
    if filter.sender.is_some() {
        count += 1;
    }
    if filter.project_id.is_some() {
        count += 1;
    }
    if filter.thread_id.is_some() {
        count += 1;
    }
    if filter.doc_kind.is_some() {
        count += 1;
    }
    if filter.date_range.is_some() {
        count += 1;
    }
    if filter
        .importance
        .as_ref()
        .is_some_and(|i| *i != ImportanceFilter::Any)
    {
        count += 1;
    }
    count
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::DateRange;

    // ── Engine-independent tests ──

    #[test]
    fn has_active_filters_empty() {
        assert!(!has_active_filters(&SearchFilter::default()));
    }

    #[test]
    fn has_active_filters_sender() {
        let filter = SearchFilter {
            sender: Some("BlueLake".to_string()),
            ..SearchFilter::default()
        };
        assert!(has_active_filters(&filter));
    }

    #[test]
    fn has_active_filters_importance_any_is_inactive() {
        let filter = SearchFilter {
            importance: Some(ImportanceFilter::Any),
            ..SearchFilter::default()
        };
        assert!(!has_active_filters(&filter));
    }

    #[test]
    fn has_active_filters_importance_urgent_is_active() {
        let filter = SearchFilter {
            importance: Some(ImportanceFilter::Urgent),
            ..SearchFilter::default()
        };
        assert!(has_active_filters(&filter));
    }

    #[test]
    fn active_filter_count_none() {
        assert_eq!(active_filter_count(&SearchFilter::default()), 0);
    }

    #[test]
    fn active_filter_count_multiple() {
        let filter = SearchFilter {
            sender: Some("A".to_string()),
            project_id: Some(1),
            thread_id: Some("t".to_string()),
            doc_kind: Some(DocKind::Message),
            importance: Some(ImportanceFilter::High),
            date_range: Some(DateRange {
                start: Some(100),
                end: Some(200),
            }),
        };
        assert_eq!(active_filter_count(&filter), 6);
    }

    #[test]
    fn active_filter_count_importance_any_not_counted() {
        let filter = SearchFilter {
            importance: Some(ImportanceFilter::Any),
            ..SearchFilter::default()
        };
        assert_eq!(active_filter_count(&filter), 0);
    }

    // ── Tantivy integration tests ──

    #[cfg(feature = "tantivy-engine")]
    mod tantivy_tests {
        use super::super::*;
        use crate::query::DateRange;
        use crate::tantivy_schema::{build_schema, register_tokenizer};
        use tantivy::collector::TopDocs;
        use tantivy::doc;
        use tantivy::schema::Value;
        use tantivy::{Index, TantivyDocument};

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
                    handles.body => "Here is the plan for DB migration to v3",
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
                    handles.body => "Steps for deploying the new search engine",
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
                    handles.doc_kind => "agent",
                    handles.subject => "GreenCastle",
                    handles.body => "GreenCastle (claude-code/opus-4.6) compliance lead",
                    handles.sender => "GreenCastle",
                    handles.project_slug => "compliance",
                    handles.project_id => 2u64,
                    handles.thread_id => "TKT-789",
                    handles.importance => "urgent",
                    handles.created_ts => 1_700_200_000_000_000i64,
                    handles.program => "claude-code",
                    handles.model => "opus-4.6"
                ))
                .unwrap();
            writer.commit().unwrap();

            (index, handles)
        }

        fn search_with_filter(
            index: &Index,
            handles: &FieldHandles,
            filter: &SearchFilter,
        ) -> Vec<u64> {
            let compiled = compile_filters(filter, handles);
            let query = compiled.apply_to(Box::new(tantivy::query::AllQuery) as Box<dyn Query>);
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(100)).unwrap();
            hits.iter()
                .map(|(_score, addr)| {
                    let doc: TantivyDocument = searcher.doc(*addr).unwrap();
                    doc.get_first(handles.id).unwrap().as_u64().unwrap()
                })
                .collect()
        }

        #[test]
        fn no_filters_returns_all() {
            let (index, handles) = setup_index();
            let ids = search_with_filter(&index, &handles, &SearchFilter::default());
            assert_eq!(ids.len(), 3);
        }

        #[test]
        fn filter_by_sender() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                sender: Some("BlueLake".to_string()),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![1]);
        }

        #[test]
        fn filter_by_project_id() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                project_id: Some(2),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![3]);
        }

        #[test]
        fn filter_by_thread_id() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                thread_id: Some("br-456".to_string()),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![2]);
        }

        #[test]
        fn filter_by_doc_kind() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                doc_kind: Some(DocKind::Agent),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![3]);
        }

        #[test]
        fn filter_by_importance_urgent() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                importance: Some(ImportanceFilter::Urgent),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![3]);
        }

        #[test]
        fn filter_by_importance_high_includes_urgent() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                importance: Some(ImportanceFilter::High),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            // "High" includes urgent + high
            assert_eq!(ids.len(), 2); // doc 1 (high) + doc 3 (urgent)
            assert!(ids.contains(&1));
            assert!(ids.contains(&3));
        }

        #[test]
        fn filter_by_importance_normal() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                importance: Some(ImportanceFilter::Normal),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![2]);
        }

        #[test]
        fn filter_by_importance_any_returns_all() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                importance: Some(ImportanceFilter::Any),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids.len(), 3);
        }

        #[test]
        fn filter_by_date_range_start_only() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                date_range: Some(DateRange {
                    start: Some(1_700_100_000_000_000),
                    end: None,
                }),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids.len(), 2); // doc 2 + doc 3
            assert!(ids.contains(&2));
            assert!(ids.contains(&3));
        }

        #[test]
        fn filter_by_date_range_end_only() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                date_range: Some(DateRange {
                    start: None,
                    end: Some(1_700_000_000_000_000),
                }),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![1]); // only doc 1
        }

        #[test]
        fn filter_by_date_range_both_bounds() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                date_range: Some(DateRange {
                    start: Some(1_700_050_000_000_000),
                    end: Some(1_700_150_000_000_000),
                }),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![2]); // only doc 2 in range
        }

        #[test]
        fn filter_by_date_range_no_bounds_returns_all() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                date_range: Some(DateRange {
                    start: None,
                    end: None,
                }),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids.len(), 3);
        }

        #[test]
        fn combined_sender_and_project() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                sender: Some("BlueLake".to_string()),
                project_id: Some(1),
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert_eq!(ids, vec![1]);
        }

        #[test]
        fn combined_sender_wrong_project_empty() {
            let (index, handles) = setup_index();
            let filter = SearchFilter {
                sender: Some("BlueLake".to_string()),
                project_id: Some(2), // BlueLake is in project 1, not 2
                ..SearchFilter::default()
            };
            let ids = search_with_filter(&index, &handles, &filter);
            assert!(ids.is_empty());
        }

        #[test]
        fn compiled_filters_empty() {
            let (_, handles) = setup_index();
            let compiled = compile_filters(&SearchFilter::default(), &handles);
            assert!(compiled.is_empty());
            assert_eq!(compiled.len(), 0);
        }

        #[test]
        fn compiled_filters_len() {
            let (_, handles) = setup_index();
            let filter = SearchFilter {
                sender: Some("A".to_string()),
                project_id: Some(1),
                importance: Some(ImportanceFilter::Urgent),
                ..SearchFilter::default()
            };
            let compiled = compile_filters(&filter, &handles);
            assert!(!compiled.is_empty());
            assert_eq!(compiled.len(), 3);
        }

        #[test]
        fn apply_to_preserves_original_when_no_filters() {
            let (index, handles) = setup_index();
            let compiled = compile_filters(&SearchFilter::default(), &handles);
            let query = Box::new(tantivy::query::AllQuery) as Box<dyn Query>;
            let result = compiled.apply_to(query);

            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&result, &TopDocs::with_limit(100)).unwrap();
            assert_eq!(hits.len(), 3);
        }
    }
}
