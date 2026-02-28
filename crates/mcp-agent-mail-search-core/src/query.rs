//! Search query model — re-exported from `mcp-agent-mail-core`.

pub use mcp_agent_mail_core::search_types::{
    DateRange, ImportanceFilter, SearchFilter, SearchMode, SearchQuery,
};

// Re-export DocKind (used by SearchFilter)
pub use mcp_agent_mail_core::search_types::DocKind;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_builder_defaults() {
        let q = SearchQuery::new("hello world");
        assert_eq!(q.raw_query, "hello world");
        assert_eq!(q.mode, SearchMode::Auto);
        assert_eq!(q.limit, 20);
        assert_eq!(q.offset, 0);
        assert!(!q.explain);
    }

    #[test]
    fn query_builder_chained() {
        let q = SearchQuery::new("test")
            .with_mode(SearchMode::Lexical)
            .with_limit(50)
            .with_offset(10)
            .with_explain();
        assert_eq!(q.mode, SearchMode::Lexical);
        assert_eq!(q.limit, 50);
        assert_eq!(q.offset, 10);
        assert!(q.explain);
    }

    #[test]
    fn query_builder_with_filters() {
        let filter = SearchFilter {
            sender: Some("BlueLake".to_owned()),
            project_id: Some(42),
            doc_kind: Some(DocKind::Message),
            ..SearchFilter::default()
        };
        let q = SearchQuery::new("plan").with_filters(filter);
        assert_eq!(q.filters.sender.as_deref(), Some("BlueLake"));
        assert_eq!(q.filters.project_id, Some(42));
        assert_eq!(q.filters.doc_kind, Some(DocKind::Message));
        assert!(q.filters.thread_id.is_none());
    }

    #[test]
    fn search_mode_display() {
        assert_eq!(SearchMode::Lexical.to_string(), "lexical");
        assert_eq!(SearchMode::Semantic.to_string(), "semantic");
        assert_eq!(SearchMode::Hybrid.to_string(), "hybrid");
        assert_eq!(SearchMode::Auto.to_string(), "auto");
    }

    #[test]
    fn search_mode_default_is_auto() {
        assert_eq!(SearchMode::default(), SearchMode::Auto);
    }

    #[test]
    fn query_serde_roundtrip() {
        let q = SearchQuery::new("migration plan")
            .with_mode(SearchMode::Hybrid)
            .with_limit(5)
            .with_offset(2)
            .with_explain();
        let json = serde_json::to_string(&q).unwrap();
        let q2: SearchQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(q2.raw_query, "migration plan");
        assert_eq!(q2.mode, SearchMode::Hybrid);
        assert_eq!(q2.limit, 5);
        assert_eq!(q2.offset, 2);
        assert!(q2.explain);
    }

    #[test]
    fn search_filter_serde_skip_none() {
        let filter = SearchFilter::default();
        let json = serde_json::to_string(&filter).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn importance_filter_default() {
        assert_eq!(ImportanceFilter::default(), ImportanceFilter::Any);
    }

    #[test]
    fn date_range_serde() {
        let range = DateRange {
            start: Some(1_000_000),
            end: Some(2_000_000),
        };
        let json = serde_json::to_string(&range).unwrap();
        let range2: DateRange = serde_json::from_str(&json).unwrap();
        assert_eq!(range2.start, Some(1_000_000));
        assert_eq!(range2.end, Some(2_000_000));
    }

    #[test]
    fn search_mode_serde_all_variants() {
        for mode in [
            SearchMode::Lexical,
            SearchMode::Semantic,
            SearchMode::Hybrid,
            SearchMode::Auto,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: SearchMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn search_mode_serde_snake_case() {
        let json = serde_json::to_string(&SearchMode::Lexical).unwrap();
        assert_eq!(json, "\"lexical\"");
        let json = serde_json::to_string(&SearchMode::Auto).unwrap();
        assert_eq!(json, "\"auto\"");
    }

    #[test]
    fn search_mode_hash_distinct() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SearchMode::Lexical);
        set.insert(SearchMode::Semantic);
        set.insert(SearchMode::Hybrid);
        set.insert(SearchMode::Auto);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn importance_filter_serde_all_variants() {
        for filter in [
            ImportanceFilter::Any,
            ImportanceFilter::Urgent,
            ImportanceFilter::High,
            ImportanceFilter::Normal,
            ImportanceFilter::Low,
        ] {
            let json = serde_json::to_string(&filter).unwrap();
            let back: ImportanceFilter = serde_json::from_str(&json).unwrap();
            assert_eq!(back, filter);
        }
    }

    #[test]
    fn date_range_start_only() {
        let range = DateRange {
            start: Some(100),
            end: None,
        };
        let json = serde_json::to_string(&range).unwrap();
        let back: DateRange = serde_json::from_str(&json).unwrap();
        assert_eq!(back.start, Some(100));
        assert!(back.end.is_none());
    }

    #[test]
    fn date_range_end_only() {
        let range = DateRange {
            start: None,
            end: Some(200),
        };
        let json = serde_json::to_string(&range).unwrap();
        let back: DateRange = serde_json::from_str(&json).unwrap();
        assert!(back.start.is_none());
        assert_eq!(back.end, Some(200));
    }

    #[test]
    fn date_range_both_none() {
        let range = DateRange {
            start: None,
            end: None,
        };
        let json = serde_json::to_string(&range).unwrap();
        let back: DateRange = serde_json::from_str(&json).unwrap();
        assert!(back.start.is_none());
        assert!(back.end.is_none());
    }

    #[test]
    fn search_filter_all_fields_set() {
        let filter = SearchFilter {
            sender: Some("Agent".to_owned()),
            project_id: Some(1),
            date_range: Some(DateRange {
                start: Some(100),
                end: Some(200),
            }),
            importance: Some(ImportanceFilter::Urgent),
            thread_id: Some("thread-1".to_owned()),
            doc_kind: Some(DocKind::Message),
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sender.as_deref(), Some("Agent"));
        assert_eq!(back.project_id, Some(1));
        assert_eq!(back.importance, Some(ImportanceFilter::Urgent));
        assert_eq!(back.thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn query_deserialize_minimal_json() {
        let json = r#"{"raw_query": "test"}"#;
        let q: SearchQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.raw_query, "test");
        assert_eq!(q.mode, SearchMode::Auto);
        assert_eq!(q.limit, 20);
        assert_eq!(q.offset, 0);
        assert!(!q.explain);
    }

    #[test]
    fn query_with_mode_returns_correct_mode() {
        let q = SearchQuery::new("x").with_mode(SearchMode::Semantic);
        assert_eq!(q.mode, SearchMode::Semantic);
    }

    #[test]
    fn query_with_limit_returns_correct_limit() {
        let q = SearchQuery::new("x").with_limit(100);
        assert_eq!(q.limit, 100);
    }

    #[test]
    fn query_with_offset_returns_correct_offset() {
        let q = SearchQuery::new("x").with_offset(42);
        assert_eq!(q.offset, 42);
    }

    #[test]
    fn query_with_explain_sets_true() {
        let q = SearchQuery::new("x").with_explain();
        assert!(q.explain);
    }

    #[test]
    fn search_filter_doc_kind_agent() {
        let filter = SearchFilter {
            doc_kind: Some(DocKind::Agent),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc_kind, Some(DocKind::Agent));
    }

    #[test]
    fn search_filter_doc_kind_project() {
        let filter = SearchFilter {
            doc_kind: Some(DocKind::Project),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc_kind, Some(DocKind::Project));
    }

    #[test]
    fn search_mode_debug() {
        let debug = format!("{:?}", SearchMode::Lexical);
        assert!(debug.contains("Lexical"));
    }

    #[test]
    fn search_mode_clone_copy_eq() {
        let a = SearchMode::Hybrid;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(a, SearchMode::Lexical);
    }

    #[test]
    fn importance_filter_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ImportanceFilter::Urgent).unwrap(),
            "\"urgent\""
        );
        assert_eq!(
            serde_json::to_string(&ImportanceFilter::Any).unwrap(),
            "\"any\""
        );
    }

    #[test]
    fn importance_filter_debug_clone_copy() {
        let a = ImportanceFilter::High;
        let b = a;
        assert_eq!(a, b);
        let debug = format!("{a:?}");
        assert!(debug.contains("High"));
    }

    #[test]
    fn importance_filter_eq_ne() {
        assert_eq!(ImportanceFilter::Low, ImportanceFilter::Low);
        assert_ne!(ImportanceFilter::Low, ImportanceFilter::Normal);
    }

    #[test]
    fn date_range_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let range = DateRange {
            start: Some(100),
            end: Some(200),
        };
        let debug = format!("{range:?}");
        assert!(debug.contains("DateRange"));
        assert_clone(&range);
    }

    #[test]
    fn search_filter_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let filter = SearchFilter::default();
        let debug = format!("{filter:?}");
        assert!(debug.contains("SearchFilter"));
        assert_clone(&filter);
    }

    #[test]
    fn search_filter_importance_field() {
        let filter = SearchFilter {
            importance: Some(ImportanceFilter::Low),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.importance, Some(ImportanceFilter::Low));
    }

    #[test]
    fn search_query_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let q = SearchQuery::new("test");
        let debug = format!("{q:?}");
        assert!(debug.contains("SearchQuery"));
        assert_clone(&q);
    }

    #[test]
    fn search_query_new_from_string() {
        let q = SearchQuery::new(String::from("owned string"));
        assert_eq!(q.raw_query, "owned string");
    }

    #[test]
    fn search_query_default_limit_is_20() {
        let q = SearchQuery::new("x");
        assert_eq!(q.limit, 20);
    }

    #[test]
    fn search_query_chained_all_builders() {
        let filter = SearchFilter {
            sender: Some("Agent".to_owned()),
            ..SearchFilter::default()
        };
        let q = SearchQuery::new("hello")
            .with_mode(SearchMode::Semantic)
            .with_limit(10)
            .with_offset(5)
            .with_explain()
            .with_filters(filter);
        assert_eq!(q.mode, SearchMode::Semantic);
        assert_eq!(q.limit, 10);
        assert_eq!(q.offset, 5);
        assert!(q.explain);
        assert_eq!(q.filters.sender.as_deref(), Some("Agent"));
    }

    #[test]
    fn search_mode_invalid_deserialize() {
        let result = serde_json::from_str::<SearchMode>("\"invalid\"");
        assert!(result.is_err());
    }

    #[test]
    fn importance_filter_invalid_deserialize() {
        let result = serde_json::from_str::<ImportanceFilter>("\"critical\"");
        assert!(result.is_err());
    }

    #[test]
    fn search_filter_doc_kind_thread() {
        let filter = SearchFilter {
            doc_kind: Some(DocKind::Thread),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc_kind, Some(DocKind::Thread));
    }

    #[test]
    fn search_filter_thread_id_field() {
        let filter = SearchFilter {
            thread_id: Some("br-42".to_owned()),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        assert!(json.contains("br-42"));
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.thread_id.as_deref(), Some("br-42"));
    }

    #[test]
    fn search_query_empty_raw_query() {
        let q = SearchQuery::new("");
        assert!(q.raw_query.is_empty());
        let json = serde_json::to_string(&q).unwrap();
        let back: SearchQuery = serde_json::from_str(&json).unwrap();
        assert!(back.raw_query.is_empty());
    }
}
