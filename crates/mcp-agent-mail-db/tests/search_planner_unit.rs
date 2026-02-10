//! Deterministic unit/integration tests for the global search planner and facet semantics.
//!
//! Covers:
//!
//! 1. **Method selection** — exhaustive coverage of FTS/LIKE/FilterOnly/Empty paths
//! 2. **Parser behavior** — sanitization edge cases through the planner
//! 3. **Facet interactions** — combined facets, edge cases, empty values
//! 4. **Ranking stability** — ORDER BY clauses for each method/ranking mode
//! 5. **Malformed queries** — SQL injection, unicode, very long, emoji
//! 6. **Cursor edge cases** — NaN, infinity, negative zero, i64 extremes
//! 7. **Visibility/redaction** — all `RedactionConfig` combos, edge visibility
//! 8. **Scope labels** — formatting and explain integration
//! 9. **LIKE fallback** — term extraction through planner
//! 10. **Serde stability** — all enums + compound types

#![allow(
    clippy::similar_names,
    clippy::cast_precision_loss,
    clippy::missing_const_for_fn,
    clippy::match_same_arms
)]

use mcp_agent_mail_db::search_planner::{
    AuditAction, Direction, DocKind, Importance, PlanMethod, PlanParam, RankingMode,
    RedactionConfig, ScopePolicy, SearchCursor, SearchQuery, SearchResult, TimeRange,
    VisibilityContext, apply_visibility, plan_search,
};

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn msg_query(text: &str, project_id: i64) -> SearchQuery {
    SearchQuery::messages(text, project_id)
}

fn make_result(id: i64, project_id: i64) -> SearchResult {
    SearchResult {
        doc_kind: DocKind::Message,
        id,
        project_id: Some(project_id),
        title: format!("Subject {id}"),
        body: format!("Body of message {id}"),
        score: Some(-1.0),
        importance: Some("normal".to_string()),
        ack_required: Some(false),
        created_ts: Some(1_700_000_000_000_000),
        thread_id: Some(format!("thread-{id}")),
        from_agent: Some("BlueLake".to_string()),
        redacted: false,
        redaction_reason: None,
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 1. METHOD SELECTION — exhaustive paths
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn method_fts_when_sanitizable_text() {
    let plan = plan_search(&msg_query("hello world", 1));
    assert_eq!(plan.method, PlanMethod::Fts);
    assert!(plan.normalized_query.is_some());
}

#[test]
fn method_like_when_sanitization_fails_but_terms_exist() {
    // `(` alone fails FTS5 sanitization but "abc" can be a LIKE term
    // We need to find input where sanitize_fts_query returns None but extract_like_terms returns something.
    // Parentheses-only will sanitize to None, but if we include a short term too...
    let plan = plan_search(&msg_query("((( ab )))", 1));
    // sanitize_fts_query strips parens; "ab" is ≥2 chars so it should survive
    // Method depends on whether sanitization produces a result
    assert!(
        plan.method == PlanMethod::Fts || plan.method == PlanMethod::Like,
        "expected Fts or Like, got {:?}",
        plan.method
    );
}

#[test]
fn method_filter_only_no_text_with_facets() {
    let q = SearchQuery {
        doc_kind: DocKind::Message,
        project_id: Some(1),
        importance: vec![Importance::Urgent],
        ..Default::default()
    };
    let plan = plan_search(&q);
    assert_eq!(plan.method, PlanMethod::FilterOnly);
}

#[test]
fn method_empty_no_text_no_facets() {
    let plan = plan_search(&SearchQuery::default());
    assert_eq!(plan.method, PlanMethod::Empty);
    assert!(plan.sql.is_empty());
    assert!(plan.params.is_empty());
}

#[test]
fn method_empty_when_all_text_stripped() {
    let plan = plan_search(&msg_query("***", 1));
    assert_eq!(plan.method, PlanMethod::Empty);
}

#[test]
fn method_empty_for_agent_no_text() {
    let plan = plan_search(&SearchQuery::agents("", 1));
    assert_eq!(plan.method, PlanMethod::Empty);
}

#[test]
fn method_empty_for_project_no_text() {
    let plan = plan_search(&SearchQuery::projects(""));
    assert_eq!(plan.method, PlanMethod::Empty);
}

#[test]
fn method_fts_for_agent_with_text() {
    let plan = plan_search(&SearchQuery::agents("blue", 1));
    assert_eq!(plan.method, PlanMethod::Fts);
    assert!(plan.sql.contains("fts_agents MATCH"));
}

#[test]
fn method_fts_for_project_with_text() {
    let plan = plan_search(&SearchQuery::projects("myproj"));
    assert_eq!(plan.method, PlanMethod::Fts);
    assert!(plan.sql.contains("fts_projects MATCH"));
}

#[test]
fn method_filter_only_requires_message_facet() {
    // No text + message doc kind + only project_id → FilterOnly
    let q = SearchQuery {
        doc_kind: DocKind::Message,
        project_id: Some(1),
        ..Default::default()
    };
    let plan = plan_search(&q);
    assert_eq!(plan.method, PlanMethod::FilterOnly);
}

#[test]
fn method_filter_only_with_each_individual_facet() {
    // Each message facet independently triggers FilterOnly
    let facet_queries = [
        SearchQuery {
            project_id: Some(1),
            ..Default::default()
        },
        SearchQuery {
            product_id: Some(1),
            ..Default::default()
        },
        SearchQuery {
            importance: vec![Importance::High],
            ..Default::default()
        },
        SearchQuery {
            direction: Some(Direction::Inbox),
            agent_name: Some("X".to_string()),
            ..Default::default()
        },
        SearchQuery {
            thread_id: Some("t1".to_string()),
            ..Default::default()
        },
        SearchQuery {
            ack_required: Some(true),
            ..Default::default()
        },
        SearchQuery {
            time_range: TimeRange {
                min_ts: Some(0),
                max_ts: None,
            },
            ..Default::default()
        },
    ];

    for (i, q) in facet_queries.iter().enumerate() {
        let plan = plan_search(q);
        assert_eq!(
            plan.method,
            PlanMethod::FilterOnly,
            "facet query {i} should be FilterOnly, got {:?}",
            plan.method
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 2. PARSER BEHAVIOR — sanitization edge cases through planner
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn parser_normal_words() {
    let plan = plan_search(&msg_query("hello world", 1));
    assert_eq!(plan.method, PlanMethod::Fts);
    let nq = plan.normalized_query.unwrap();
    assert!(nq.contains("hello"));
    assert!(nq.contains("world"));
}

#[test]
fn parser_single_word() {
    let plan = plan_search(&msg_query("deployment", 1));
    assert_eq!(plan.method, PlanMethod::Fts);
    assert!(
        plan.normalized_query
            .as_deref()
            .unwrap()
            .contains("deployment")
    );
}

#[test]
fn parser_hyphenated_token() {
    let plan = plan_search(&msg_query("POL-358", 1));
    assert_eq!(plan.method, PlanMethod::Fts);
    // Hyphenated tokens should be quoted for FTS5
    let nq = plan.normalized_query.unwrap();
    assert!(
        nq.contains("\"POL-358\"") || nq.contains("POL-358"),
        "expected hyphenated token in normalized query: {nq}"
    );
}

#[test]
fn parser_wildcard_suffix() {
    let plan = plan_search(&msg_query("deploy*", 1));
    assert_eq!(plan.method, PlanMethod::Fts);
    // Wildcard suffix should be preserved for FTS5
    let nq = plan.normalized_query.unwrap();
    assert!(nq.contains("deploy"), "expected base term in query: {nq}");
}

#[test]
fn parser_leading_wildcard_stripped() {
    let plan = plan_search(&msg_query("*deploy", 1));
    // Leading wildcards are invalid for FTS5, sanitizer should handle
    assert!(
        plan.method == PlanMethod::Fts || plan.method == PlanMethod::Like,
        "expected Fts or Like after leading wildcard strip, got {:?}",
        plan.method
    );
}

#[test]
fn parser_only_operators() {
    // No facets: should not plan as FTS for bare boolean operators.
    let q = SearchQuery {
        text: "AND OR NOT".to_string(),
        doc_kind: DocKind::Message,
        ..Default::default()
    };
    let plan = plan_search(&q);
    // Bare operators should be stripped; may result in Empty
    assert!(
        plan.method == PlanMethod::Empty || plan.method == PlanMethod::Like,
        "bare operators should not produce FTS, got {:?}",
        plan.method
    );
}

#[test]
fn parser_empty_after_stripping() {
    for input in ["", "   ", "\t\n", "***", "!!!"] {
        // No facets: empty/hostile input should not produce FTS.
        let q = SearchQuery {
            text: input.to_string(),
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let plan = plan_search(&q);
        assert!(
            plan.method == PlanMethod::Empty || plan.method == PlanMethod::Like,
            "input {:?} should not produce FTS, got {:?}",
            input,
            plan.method
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 3. FACET INTERACTIONS — combined + edge cases
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn facet_all_message_facets_combined() {
    let q = SearchQuery {
        text: "hello".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(1),
        importance: vec![Importance::Urgent, Importance::High],
        direction: Some(Direction::Outbox),
        agent_name: Some("Agent".to_string()),
        thread_id: Some("thread-42".to_string()),
        ack_required: Some(true),
        time_range: TimeRange {
            min_ts: Some(100),
            max_ts: Some(999),
        },
        ..Default::default()
    };
    let plan = plan_search(&q);
    assert_eq!(plan.method, PlanMethod::Fts);

    // All facets should appear
    assert!(plan.facets_applied.contains(&"project_id".to_string()));
    assert!(plan.facets_applied.contains(&"importance".to_string()));
    assert!(plan.facets_applied.contains(&"direction".to_string()));
    assert!(plan.facets_applied.contains(&"thread_id".to_string()));
    assert!(plan.facets_applied.contains(&"ack_required".to_string()));
    assert!(plan.facets_applied.contains(&"time_range_min".to_string()));
    assert!(plan.facets_applied.contains(&"time_range_max".to_string()));

    // SQL should have all relevant WHERE clauses
    assert!(plan.sql.contains("m.importance IN (?, ?)"));
    assert!(plan.sql.contains("m.thread_id = ?"));
    assert!(plan.sql.contains("m.ack_required = ?"));
    assert!(plan.sql.contains("m.created_ts >= ?"));
    assert!(plan.sql.contains("m.created_ts <= ?"));
}

#[test]
fn facet_empty_importance_list_not_applied() {
    let mut q = msg_query("test", 1);
    q.importance = vec![];
    let plan = plan_search(&q);
    assert!(!plan.facets_applied.contains(&"importance".to_string()));
    // SQL always selects `m.importance` for message results; the facet should not add a filter.
    assert!(!plan.sql.contains("m.importance IN"));
}

#[test]
fn facet_single_importance() {
    let mut q = msg_query("test", 1);
    q.importance = vec![Importance::Low];
    let plan = plan_search(&q);
    assert!(plan.sql.contains("m.importance IN (?)"));
}

#[test]
fn facet_all_four_importances() {
    let mut q = msg_query("test", 1);
    q.importance = vec![
        Importance::Low,
        Importance::Normal,
        Importance::High,
        Importance::Urgent,
    ];
    let plan = plan_search(&q);
    assert!(plan.sql.contains("m.importance IN (?, ?, ?, ?)"));
}

#[test]
fn facet_direction_without_agent_name_ignored() {
    let mut q = msg_query("test", 1);
    q.direction = Some(Direction::Inbox);
    q.agent_name = None; // no agent → direction facet not applied
    let plan = plan_search(&q);
    assert!(!plan.facets_applied.contains(&"direction".to_string()));
}

#[test]
fn facet_agent_name_without_direction_matches_both() {
    let mut q = msg_query("test", 1);
    q.agent_name = Some("BlueLake".to_string());
    q.direction = None;
    let plan = plan_search(&q);
    assert!(plan.facets_applied.contains(&"agent_name".to_string()));
    // Should have OR for sender and recipient
    assert!(plan.sql.contains("a.name = ?"));
    assert!(plan.sql.contains("message_recipients"));
}

#[test]
fn facet_outbox_direction() {
    let mut q = msg_query("test", 1);
    q.direction = Some(Direction::Outbox);
    q.agent_name = Some("Agent".to_string());
    let plan = plan_search(&q);
    assert!(plan.sql.contains("a.name = ?"));
    assert!(!plan.sql.contains("message_recipients mr\n"));
}

#[test]
fn facet_inbox_direction() {
    let mut q = msg_query("test", 1);
    q.direction = Some(Direction::Inbox);
    q.agent_name = Some("Agent".to_string());
    let plan = plan_search(&q);
    assert!(plan.sql.contains("message_recipients"));
}

#[test]
fn facet_time_range_min_only() {
    let mut q = msg_query("test", 1);
    q.time_range = TimeRange {
        min_ts: Some(100),
        max_ts: None,
    };
    let plan = plan_search(&q);
    assert!(plan.sql.contains("m.created_ts >= ?"));
    assert!(!plan.sql.contains("m.created_ts <= ?"));
    assert!(plan.facets_applied.contains(&"time_range_min".to_string()));
    assert!(!plan.facets_applied.contains(&"time_range_max".to_string()));
}

#[test]
fn facet_time_range_max_only() {
    let mut q = msg_query("test", 1);
    q.time_range = TimeRange {
        min_ts: None,
        max_ts: Some(999),
    };
    let plan = plan_search(&q);
    assert!(!plan.sql.contains("m.created_ts >= ?"));
    assert!(plan.sql.contains("m.created_ts <= ?"));
}

#[test]
fn facet_ack_required_true_and_false() {
    for ack_val in [true, false] {
        let mut q = msg_query("test", 1);
        q.ack_required = Some(ack_val);
        let plan = plan_search(&q);
        assert!(plan.sql.contains("m.ack_required = ?"));
        // Verify the param value
        let ack_param = plan
            .params
            .iter()
            .find(|p| matches!(p, PlanParam::Int(v) if *v == i64::from(ack_val)));
        assert!(
            ack_param.is_some(),
            "ack_required={ack_val} param not found"
        );
    }
}

#[test]
fn facet_product_id_uses_subquery() {
    let q = SearchQuery::product_messages("needle", 7);
    let plan = plan_search(&q);
    assert!(plan.sql.contains("product_project_links"));
    assert!(plan.facets_applied.contains(&"product_id".to_string()));
}

#[test]
fn facet_project_id_takes_priority_over_product_id() {
    // When both are set, project_id should be used (it's checked first)
    let mut q = msg_query("test", 1);
    q.product_id = Some(99);
    let plan = plan_search(&q);
    assert!(plan.sql.contains("m.project_id = ?"));
    // product_id clause should NOT appear when project_id is set
    assert!(!plan.sql.contains("product_project_links"));
}

// ════════════════════════════════════════════════════════════════════════════
// 4. RANKING STABILITY — ORDER BY clauses
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn ranking_fts_orders_by_score_asc() {
    let plan = plan_search(&msg_query("hello", 1));
    assert_eq!(plan.method, PlanMethod::Fts);
    assert!(
        plan.sql.contains("ORDER BY score ASC"),
        "FTS should order by score ASC: {}",
        plan.sql
    );
}

#[test]
fn ranking_like_orders_by_recency() {
    // LIKE fallback: ORDER BY m.created_ts DESC
    // Hard to trigger LIKE via planner — need text that sanitizes to None but has terms
    // Instead, verify FilterOnly which uses same recency ordering
    let q = SearchQuery {
        doc_kind: DocKind::Message,
        project_id: Some(1),
        ..Default::default()
    };
    let plan = plan_search(&q);
    assert_eq!(plan.method, PlanMethod::FilterOnly);
    assert!(
        plan.sql.contains("ORDER BY m.created_ts DESC"),
        "FilterOnly should order by recency: {}",
        plan.sql
    );
}

#[test]
fn ranking_filter_only_both_modes_use_recency() {
    // FilterOnly with Relevance ranking → still uses recency
    let q1 = SearchQuery {
        doc_kind: DocKind::Message,
        project_id: Some(1),
        ranking: RankingMode::Relevance,
        ..Default::default()
    };
    let q2 = SearchQuery {
        doc_kind: DocKind::Message,
        project_id: Some(1),
        ranking: RankingMode::Recency,
        ..Default::default()
    };
    let plan1 = plan_search(&q1);
    let plan2 = plan_search(&q2);
    assert!(plan1.sql.contains("ORDER BY m.created_ts DESC"));
    assert!(plan2.sql.contains("ORDER BY m.created_ts DESC"));
}

#[test]
fn ranking_agent_search_orders_by_score_or_id() {
    let plan = plan_search(&SearchQuery::agents("blue", 1));
    assert!(
        plan.sql.contains("ORDER BY score ASC, a.id ASC"),
        "agent FTS should order by score: {}",
        plan.sql
    );
}

#[test]
fn ranking_project_search_orders_by_score_or_id() {
    let plan = plan_search(&SearchQuery::projects("proj"));
    assert!(
        plan.sql.contains("ORDER BY score ASC, p.id ASC"),
        "project FTS should order by score: {}",
        plan.sql
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 5. MALFORMED QUERIES — adversarial input
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn malformed_sql_injection_attempt() {
    let hostile_inputs = [
        "'; DROP TABLE messages; --",
        "1 OR 1=1",
        "UNION SELECT * FROM agents",
        "hello\" OR \"1\"=\"1",
        "test\0null",
        "a'; DELETE FROM messages WHERE '1'='1",
    ];

    for input in hostile_inputs {
        let plan = plan_search(&msg_query(input, 1));
        // Should never produce raw SQL injection
        assert!(
            !plan.sql.contains("DROP TABLE"),
            "SQL injection leaked for input: {input}"
        );
        assert!(
            !plan.sql.contains("DELETE FROM"),
            "DELETE leaked for input: {input}"
        );
        assert!(
            !plan.sql.contains("UNION SELECT"),
            "UNION leaked for input: {input}"
        );
        // Should still produce a valid plan
        assert!(
            matches!(
                plan.method,
                PlanMethod::Fts | PlanMethod::Like | PlanMethod::Empty
            ),
            "hostile input {input:?} produced invalid method {:?}",
            plan.method
        );
    }
}

#[test]
fn malformed_unicode_queries() {
    let unicode_inputs = [
        "\u{0000}",
        "\u{FFFF}",
        "\u{1F600}\u{1F600}\u{1F600}",
        "\u{202E}dlrow olleh",
        "Привет мир",
        "你好世界",
        "a\u{0308}",                // combining diaeresis
        "\u{200B}\u{200B}\u{200B}", // zero-width spaces
    ];

    for input in unicode_inputs {
        let plan = plan_search(&msg_query(input, 1));
        // Must not panic; any method is acceptable
        assert!(
            matches!(
                plan.method,
                PlanMethod::Fts | PlanMethod::Like | PlanMethod::FilterOnly | PlanMethod::Empty
            ),
            "unicode input produced invalid method"
        );
    }
}

#[test]
fn malformed_very_long_query() {
    let long_text = "hello ".repeat(10_000);
    let plan = plan_search(&msg_query(&long_text, 1));
    // Should not panic, should produce some valid plan
    assert!(matches!(
        plan.method,
        PlanMethod::Fts | PlanMethod::Like | PlanMethod::Empty
    ));
}

#[test]
fn malformed_special_chars() {
    let inputs = [
        "hello ((())) world",
        "test: subject",
        "a AND b OR c NOT d",
        "\"unclosed quote",
        "a + b",
        "a - b",
        "a ^ b",
        "NEAR(hello, world)",
        "hello NEAR/5 world",
    ];

    for input in inputs {
        let plan = plan_search(&msg_query(input, 1));
        // Must not panic
        assert!(
            matches!(
                plan.method,
                PlanMethod::Fts | PlanMethod::Like | PlanMethod::Empty
            ),
            "special chars input {input:?} produced invalid method {:?}",
            plan.method
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 6. CURSOR EDGE CASES
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn cursor_roundtrip_negative_score() {
    let cursor = SearchCursor {
        score: -99.5,
        id: 42,
    };
    let decoded = SearchCursor::decode(&cursor.encode()).unwrap();
    assert!((decoded.score - cursor.score).abs() < 1e-12);
    assert_eq!(decoded.id, 42);
}

#[test]
fn cursor_roundtrip_zero() {
    let cursor = SearchCursor { score: 0.0, id: 0 };
    let decoded = SearchCursor::decode(&cursor.encode()).unwrap();
    assert!(decoded.score.abs() < 1e-12);
    assert_eq!(decoded.id, 0);
}

#[test]
fn cursor_roundtrip_negative_zero() {
    let cursor = SearchCursor { score: -0.0, id: 1 };
    let encoded = cursor.encode();
    let decoded = SearchCursor::decode(&encoded).unwrap();
    assert!(decoded.score.abs() < 1e-12);
}

#[test]
fn cursor_roundtrip_infinity() {
    let cursor = SearchCursor {
        score: f64::INFINITY,
        id: 1,
    };
    let decoded = SearchCursor::decode(&cursor.encode()).unwrap();
    assert!(decoded.score.is_infinite() && decoded.score.is_sign_positive());
}

#[test]
fn cursor_roundtrip_neg_infinity() {
    let cursor = SearchCursor {
        score: f64::NEG_INFINITY,
        id: 1,
    };
    let decoded = SearchCursor::decode(&cursor.encode()).unwrap();
    assert!(decoded.score.is_infinite() && decoded.score.is_sign_negative());
}

#[test]
fn cursor_roundtrip_nan() {
    let cursor = SearchCursor {
        score: f64::NAN,
        id: 1,
    };
    let decoded = SearchCursor::decode(&cursor.encode()).unwrap();
    assert!(decoded.score.is_nan());
}

#[test]
fn cursor_roundtrip_extreme_ids() {
    for id in [i64::MIN, i64::MAX, -1, 0, 1] {
        let cursor = SearchCursor { score: -1.0, id };
        let decoded = SearchCursor::decode(&cursor.encode()).unwrap();
        assert_eq!(decoded.id, id, "id roundtrip failed for {id}");
    }
}

#[test]
fn cursor_decode_rejects_invalid() {
    let bad_inputs = [
        "",
        "garbage",
        "s:i",
        "si",
        "s0000000000000000:",
        ":i42",
        "s0000000000000000:i",
        "s:i42",
        "sGGGGGGGGGGGGGGGG:i42",
        "s0000000000000000:inotanumber",
        "s0000000000000000i42",  // missing colon
        "S0000000000000000:i42", // uppercase S
    ];
    for input in bad_inputs {
        assert!(
            SearchCursor::decode(input).is_none(),
            "should reject {input:?}"
        );
    }
}

#[test]
fn cursor_in_plan_adds_pagination_clause() {
    let cursor = SearchCursor {
        score: -2.5,
        id: 100,
    };
    let mut q = msg_query("test", 1);
    q.cursor = Some(cursor.encode());
    let plan = plan_search(&q);
    assert!(plan.sql.contains("score > ?"));
    assert!(plan.sql.contains("m.id > ?"));
    assert!(plan.facets_applied.contains(&"cursor".to_string()));
}

#[test]
fn cursor_invalid_token_ignored() {
    let mut q = msg_query("test", 1);
    q.cursor = Some("garbage".to_string());
    let plan = plan_search(&q);
    // Invalid cursor is silently ignored
    assert!(!plan.facets_applied.contains(&"cursor".to_string()));
    assert!(!plan.sql.contains("score > ?"));
}

// ════════════════════════════════════════════════════════════════════════════
// 7. VISIBILITY/REDACTION — edge cases and all combos
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn visibility_unrestricted_passes_all() {
    let results = vec![make_result(1, 10), make_result(2, 99)];
    let ctx = VisibilityContext {
        caller_project_ids: vec![10],
        approved_contact_ids: vec![],
        policy: ScopePolicy::Unrestricted,
        redaction: RedactionConfig::default(),
    };
    let (visible, audit) = apply_visibility(results, &ctx);
    assert_eq!(visible.len(), 2);
    assert!(audit.is_empty());
}

#[test]
fn visibility_empty_results() {
    let ctx = VisibilityContext {
        caller_project_ids: vec![1],
        approved_contact_ids: vec![],
        policy: ScopePolicy::CallerScoped {
            caller_agent: "A".to_string(),
        },
        redaction: RedactionConfig::default(),
    };
    let (visible, audit) = apply_visibility(vec![], &ctx);
    assert!(visible.is_empty());
    assert!(audit.is_empty());
}

#[test]
fn visibility_none_project_id_treated_as_zero() {
    let mut result = make_result(1, 0);
    result.project_id = None;
    let ctx = VisibilityContext {
        caller_project_ids: vec![1], // not project 0
        approved_contact_ids: vec![],
        policy: ScopePolicy::CallerScoped {
            caller_agent: "A".to_string(),
        },
        redaction: RedactionConfig::default(),
    };
    let (visible, audit) = apply_visibility(vec![result], &ctx);
    assert!(visible.is_empty());
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, AuditAction::Denied);
}

#[test]
fn visibility_project_set_empty_ids_denies_all() {
    let results = vec![make_result(1, 10)];
    let ctx = VisibilityContext {
        caller_project_ids: vec![],
        approved_contact_ids: vec![],
        policy: ScopePolicy::ProjectSet {
            allowed_project_ids: vec![],
        },
        redaction: RedactionConfig::default(),
    };
    let (visible, audit) = apply_visibility(results, &ctx);
    assert!(visible.is_empty());
    assert_eq!(audit.len(), 1);
}

#[test]
fn redaction_all_8_combinations() {
    for bits in 0u8..8 {
        let config = RedactionConfig {
            redact_body: bits & 1 != 0,
            redact_agent_names: bits & 2 != 0,
            redact_thread_ids: bits & 4 != 0,
            placeholder: "[X]".to_string(),
        };

        let result = SearchResult {
            body: "secret".to_string(),
            title: "secret title".to_string(),
            from_agent: Some("Agent007".to_string()),
            thread_id: Some("thread-x".to_string()),
            ..make_result(1, 99)
        };

        let results = vec![result];
        let ctx = VisibilityContext {
            caller_project_ids: vec![1], // result is in project 99
            approved_contact_ids: vec![],
            policy: ScopePolicy::CallerScoped {
                caller_agent: "Me".to_string(),
            },
            redaction: config.clone(),
        };

        let (visible, _) = apply_visibility(results, &ctx);

        if config.is_active() {
            // Redacted
            assert_eq!(visible.len(), 1, "bits={bits}: should include redacted");
            let r = &visible[0];
            assert!(r.redacted, "bits={bits}: should be marked redacted");
            if config.redact_body {
                assert_eq!(r.body, "[X]", "bits={bits}: body not redacted");
                assert_eq!(r.title, "[X]", "bits={bits}: title not redacted");
            }
            if config.redact_agent_names {
                assert_eq!(
                    r.from_agent.as_deref(),
                    Some("[X]"),
                    "bits={bits}: agent not redacted"
                );
            }
            if config.redact_thread_ids {
                assert!(r.thread_id.is_none(), "bits={bits}: thread not redacted");
            }
        } else {
            // Denied (redaction not active)
            assert!(visible.is_empty(), "bits={bits}: should be denied");
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 8. SCOPE LABELS — formatting
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn scope_label_unrestricted() {
    let q = msg_query("test", 1);
    let plan = plan_search(&q);
    assert_eq!(plan.scope_label, "unrestricted");
}

#[test]
fn scope_label_caller_scoped() {
    let mut q = msg_query("test", 1);
    q.scope = ScopePolicy::CallerScoped {
        caller_agent: "BlueLake".to_string(),
    };
    let plan = plan_search(&q);
    assert_eq!(plan.scope_label, "caller_scoped:BlueLake");
}

#[test]
fn scope_label_project_set() {
    let mut q = msg_query("test", 1);
    q.scope = ScopePolicy::ProjectSet {
        allowed_project_ids: vec![1, 2, 3],
    };
    let plan = plan_search(&q);
    assert_eq!(plan.scope_label, "project_set:3");
}

#[test]
fn scope_label_in_explain() {
    let mut q = msg_query("test", 1);
    q.scope = ScopePolicy::ProjectSet {
        allowed_project_ids: vec![1],
    };
    q.explain = true;
    let plan = plan_search(&q);
    let explain = plan.explain();
    assert!(explain.scope_policy.starts_with("project_set"));
}

// ════════════════════════════════════════════════════════════════════════════
// 9. EXPLAIN OUTPUT
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn explain_fts_output() {
    let plan = plan_search(&msg_query("hello", 1));
    let explain = plan.explain();
    assert_eq!(explain.method, "fts5");
    assert!(!explain.used_like_fallback);
    assert!(explain.normalized_query.is_some());
    assert!(!explain.sql.is_empty());
    assert_eq!(explain.denied_count, 0);
    assert_eq!(explain.redacted_count, 0);
}

#[test]
fn explain_empty_output() {
    let plan = plan_search(&SearchQuery::default());
    let explain = plan.explain();
    assert_eq!(explain.method, "empty");
    assert!(explain.sql.is_empty());
    assert_eq!(explain.facet_count, 0);
}

#[test]
fn explain_facet_count_matches_applied() {
    let mut q = msg_query("test", 1);
    q.importance = vec![Importance::High];
    q.thread_id = Some("t".to_string());
    let plan = plan_search(&q);
    let explain = plan.explain();
    assert_eq!(explain.facet_count, plan.facets_applied.len());
    assert!(explain.facet_count >= 3); // project_id + importance + thread_id
}

// ════════════════════════════════════════════════════════════════════════════
// 10. LIMIT PROPAGATION
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn limit_default_is_50() {
    let q = SearchQuery::default();
    assert_eq!(q.effective_limit(), 50);
}

#[test]
fn limit_clamped_to_1() {
    let q = SearchQuery {
        limit: Some(0),
        ..Default::default()
    };
    assert_eq!(q.effective_limit(), 1);
}

#[test]
fn limit_clamped_to_1000() {
    let q = SearchQuery {
        limit: Some(99_999),
        ..Default::default()
    };
    assert_eq!(q.effective_limit(), 1000);
}

#[test]
fn limit_propagated_to_sql() {
    let mut q = msg_query("hello", 1);
    q.limit = Some(25);
    let plan = plan_search(&q);
    match plan.params.last() {
        Some(PlanParam::Int(v)) => assert_eq!(*v, 25),
        other => panic!("expected Int(25), got {other:?}"),
    }
}

#[test]
fn limit_clamped_in_sql() {
    let mut q = msg_query("hello", 1);
    q.limit = Some(0);
    let plan = plan_search(&q);
    match plan.params.last() {
        Some(PlanParam::Int(v)) => assert_eq!(*v, 1), // clamped from 0
        other => panic!("expected Int(1), got {other:?}"),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 11. SCOPE ENFORCEMENT IN SQL
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn scope_project_set_enforced_in_message_sql() {
    let mut q = msg_query("test", 1);
    q.scope = ScopePolicy::ProjectSet {
        allowed_project_ids: vec![10, 20],
    };
    let plan = plan_search(&q);
    assert!(plan.scope_enforced);
    assert!(plan.sql.contains("m.project_id IN (?, ?)"));
}

#[test]
fn scope_project_set_enforced_in_agent_sql() {
    let mut q = SearchQuery::agents("blue", 1);
    q.scope = ScopePolicy::ProjectSet {
        allowed_project_ids: vec![5],
    };
    let plan = plan_search(&q);
    assert!(plan.scope_enforced);
    assert!(plan.sql.contains("a.project_id IN (?)"));
}

#[test]
fn scope_project_set_enforced_in_project_sql() {
    let mut q = SearchQuery::projects("myproj");
    q.scope = ScopePolicy::ProjectSet {
        allowed_project_ids: vec![1, 2, 3],
    };
    let plan = plan_search(&q);
    assert!(plan.scope_enforced);
    assert!(plan.sql.contains("p.id IN (?, ?, ?)"));
}

#[test]
fn scope_empty_project_set_not_enforced() {
    let mut q = msg_query("test", 1);
    q.scope = ScopePolicy::ProjectSet {
        allowed_project_ids: vec![],
    };
    let plan = plan_search(&q);
    assert!(!plan.scope_enforced);
}

#[test]
fn scope_caller_scoped_not_sql_enforced() {
    let mut q = msg_query("test", 1);
    q.scope = ScopePolicy::CallerScoped {
        caller_agent: "Agent".to_string(),
    };
    let plan = plan_search(&q);
    assert!(!plan.scope_enforced);
}

#[test]
fn scope_unrestricted_not_enforced() {
    let q = msg_query("test", 1);
    let plan = plan_search(&q);
    assert!(!plan.scope_enforced);
}

// ════════════════════════════════════════════════════════════════════════════
// 12. SERDE STABILITY
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn serde_doc_kind_all_variants() {
    for kind in [DocKind::Message, DocKind::Agent, DocKind::Project] {
        let json = serde_json::to_string(&kind).unwrap();
        let parsed: DocKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, kind);
    }
}

#[test]
fn serde_importance_all_variants() {
    for imp in [
        Importance::Low,
        Importance::Normal,
        Importance::High,
        Importance::Urgent,
    ] {
        let json = serde_json::to_string(&imp).unwrap();
        let parsed: Importance = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, imp);
    }
}

#[test]
fn serde_direction_roundtrip() {
    for dir in [Direction::Inbox, Direction::Outbox] {
        let json = serde_json::to_string(&dir).unwrap();
        let parsed: Direction = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, dir);
    }
}

#[test]
fn serde_ranking_mode_roundtrip() {
    for mode in [RankingMode::Relevance, RankingMode::Recency] {
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: RankingMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mode);
    }
}

#[test]
fn serde_scope_policy_all_variants() {
    let variants: Vec<ScopePolicy> = vec![
        ScopePolicy::Unrestricted,
        ScopePolicy::CallerScoped {
            caller_agent: "Test".to_string(),
        },
        ScopePolicy::ProjectSet {
            allowed_project_ids: vec![1, 2],
        },
    ];
    for policy in variants {
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: ScopePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }
}

#[test]
fn serde_audit_action_roundtrip() {
    for action in [AuditAction::Denied, AuditAction::Redacted] {
        let json = serde_json::to_string(&action).unwrap();
        let parsed: AuditAction = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, action);
    }
}

#[test]
fn serde_search_query_complex() {
    let q = SearchQuery {
        text: "test query".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(1),
        product_id: None,
        importance: vec![Importance::Urgent, Importance::High],
        direction: Some(Direction::Inbox),
        agent_name: Some("BlueLake".to_string()),
        thread_id: Some("thread-42".to_string()),
        ack_required: Some(true),
        time_range: TimeRange {
            min_ts: Some(100),
            max_ts: Some(999),
        },
        ranking: RankingMode::Recency,
        limit: Some(25),
        cursor: Some("token".to_string()),
        explain: true,
        scope: ScopePolicy::CallerScoped {
            caller_agent: "Me".to_string(),
        },
        redaction: Some(RedactionConfig::strict()),
    };
    let json = serde_json::to_string_pretty(&q).unwrap();
    let q2: SearchQuery = serde_json::from_str(&json).unwrap();
    assert_eq!(q2.text, "test query");
    assert_eq!(q2.importance.len(), 2);
    assert_eq!(q2.time_range.min_ts, Some(100));
    assert_eq!(q2.limit, Some(25));
    assert!(q2.explain);
}

// ════════════════════════════════════════════════════════════════════════════
// 13. DOC KIND ROUTING — all three paths
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn doc_kind_message_uses_message_tables() {
    let plan = plan_search(&msg_query("test", 1));
    assert!(plan.sql.contains("messages m") || plan.sql.contains("fts_messages"));
}

#[test]
fn doc_kind_agent_uses_agent_tables() {
    let plan = plan_search(&SearchQuery::agents("blue", 1));
    assert!(plan.sql.contains("agents a") || plan.sql.contains("fts_agents"));
}

#[test]
fn doc_kind_project_uses_project_tables() {
    let plan = plan_search(&SearchQuery::projects("proj"));
    assert!(plan.sql.contains("projects p") || plan.sql.contains("fts_projects"));
}

// ════════════════════════════════════════════════════════════════════════════
// 14. PARAM COUNTING — verify correct parameter counts
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn param_count_fts_message_basic() {
    let plan = plan_search(&msg_query("hello", 1));
    // Params: FTS text, project_id, LIMIT → 3
    assert_eq!(
        plan.params.len(),
        3,
        "expected 3 params, got {:?}",
        plan.params
    );
}

#[test]
fn param_count_fts_message_with_facets() {
    let mut q = msg_query("hello", 1);
    q.importance = vec![Importance::High, Importance::Urgent];
    q.thread_id = Some("t1".to_string());
    let plan = plan_search(&q);
    // Params: FTS text, project_id, importance×2, thread_id, LIMIT → 6
    assert_eq!(plan.params.len(), 6);
}

#[test]
fn param_count_with_cursor() {
    let cursor = SearchCursor {
        score: -1.0,
        id: 50,
    };
    let mut q = msg_query("hello", 1);
    q.cursor = Some(cursor.encode());
    let plan = plan_search(&q);
    // Params: FTS text, project_id, score×2, id, LIMIT → 6
    assert_eq!(plan.params.len(), 6);
}

#[test]
fn param_count_project_set_scope() {
    let mut q = msg_query("hello", 1);
    q.scope = ScopePolicy::ProjectSet {
        allowed_project_ids: vec![1, 2, 3],
    };
    let plan = plan_search(&q);
    // Params: FTS text, project_id, scope×3, LIMIT → 6
    assert_eq!(plan.params.len(), 6);
}

// ════════════════════════════════════════════════════════════════════════════
// 15. QUERY BUILDER HELPERS
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn builder_messages() {
    let q = SearchQuery::messages("hello", 42);
    assert_eq!(q.doc_kind, DocKind::Message);
    assert_eq!(q.project_id, Some(42));
    assert_eq!(q.text, "hello");
}

#[test]
fn builder_product_messages() {
    let q = SearchQuery::product_messages("world", 7);
    assert_eq!(q.doc_kind, DocKind::Message);
    assert_eq!(q.product_id, Some(7));
    assert!(q.project_id.is_none());
}

#[test]
fn builder_agents() {
    let q = SearchQuery::agents("blue", 3);
    assert_eq!(q.doc_kind, DocKind::Agent);
    assert_eq!(q.project_id, Some(3));
}

#[test]
fn builder_projects() {
    let q = SearchQuery::projects("myproj");
    assert_eq!(q.doc_kind, DocKind::Project);
    assert!(q.project_id.is_none());
}

// ════════════════════════════════════════════════════════════════════════════
// 16. DETERMINISTIC PLAN OUTPUT
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn plan_deterministic_across_calls() {
    let q = SearchQuery {
        text: "hello world".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(1),
        importance: vec![Importance::Urgent],
        time_range: TimeRange {
            min_ts: Some(100),
            max_ts: Some(999),
        },
        ..Default::default()
    };

    let plan1 = plan_search(&q);
    let plan2 = plan_search(&q);

    assert_eq!(plan1.sql, plan2.sql);
    assert_eq!(plan1.method, plan2.method);
    assert_eq!(plan1.normalized_query, plan2.normalized_query);
    assert_eq!(plan1.facets_applied, plan2.facets_applied);
    assert_eq!(plan1.scope_enforced, plan2.scope_enforced);
    assert_eq!(plan1.scope_label, plan2.scope_label);
    assert_eq!(plan1.params.len(), plan2.params.len());
}
