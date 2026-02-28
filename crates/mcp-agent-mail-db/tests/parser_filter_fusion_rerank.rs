#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
//! Integration tests for parser, filter, fusion, and rerank math.
//!
//! br-2tnl.7.1: Build unit test suite for parser, filters, fusion, and rerank math
//!
//! Validates cross-module behavior:
//! - Parser edge cases (sanitization, hyphenated tokens, special chars)
//! - Filter composition correctness (active count, `has_active`, importance semantics)
//! - Fusion deterministic ordering and tie-break behavior
//! - Budget derivation math (query class × mode interactions)
//! - Explain payload invariants across composed reports

use std::collections::HashMap;
use std::time::Duration;

use mcp_agent_mail_search_core::fusion::{RrfConfig, fuse_rrf, fuse_rrf_default};
use mcp_agent_mail_search_core::hybrid_candidates::{
    PreparedCandidate, CandidateSource,
};
use mcp_agent_mail_db::search_candidates::{
    CandidateBudget, CandidateBudgetConfig, CandidateHit, CandidateMode,
    QueryClass, prepare_candidates,
};
use mcp_agent_mail_core::{DateRange, ImportanceFilter, SearchFilter, SearchMode};
use mcp_agent_mail_core::{
    ExplainComposerConfig, ExplainReasonCode, ExplainStage, ExplainVerbosity, ScoreFactor,
    StageScoreInput, compose_explain_report, compose_hit_explanation, redact_hit_explanation,
};
use mcp_agent_mail_db::query_assistance::{SanitizedQuery, extract_terms, sanitize_query};
use mcp_agent_mail_db::search_filter_compiler::{active_filter_count, has_active_filters};

// ═══════════════════════════════════════════════════════════════════════
// Section 1: Parser edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn sanitize_preserves_normal_text() {
    match sanitize_query("hello world") {
        SanitizedQuery::Valid(q) => assert_eq!(q, "hello world"),
        SanitizedQuery::Empty => panic!("expected Valid, got Empty"),
    }
}

#[test]
fn sanitize_strips_fts_special_chars() {
    let specials = ["^", "~", "\\", "{", "}", "[", "]"];
    for s in specials {
        let input = format!("test{s}query");
        let result = sanitize_query(&input);
        match &result {
            SanitizedQuery::Valid(q) => {
                assert!(!q.contains(s), "special char {s:?} not stripped from: {q}");
            }
            SanitizedQuery::Empty => {} // also acceptable
        }
    }
}

#[test]
fn sanitize_all_specials_produces_empty() {
    let result = sanitize_query("[]{}^~\\");
    assert!(result.is_empty(), "all-special input should be empty");
}

#[test]
fn sanitize_unicode_preserved() {
    match sanitize_query("Müller Lokalisierungsdaten 日本語") {
        SanitizedQuery::Valid(q) => {
            assert!(q.contains("Müller"));
            assert!(q.contains("日本語"));
        }
        SanitizedQuery::Empty => panic!("expected Valid, got Empty"),
    }
}

#[test]
fn sanitize_empty_and_whitespace() {
    assert!(sanitize_query("").is_empty());
    assert!(sanitize_query("   ").is_empty());
    assert!(sanitize_query("\t\n").is_empty());
}

#[test]
fn extract_terms_basic() {
    let terms = extract_terms("hello world foo");
    assert_eq!(terms.len(), 3);
    assert!(terms.contains(&"hello".to_string()));
    assert!(terms.contains(&"world".to_string()));
    assert!(terms.contains(&"foo".to_string()));
}

#[test]
fn extract_terms_preserves_duplicates() {
    let terms = extract_terms("hello hello hello");
    assert_eq!(terms.len(), 3, "extract_terms does not deduplicate");
    assert!(terms.iter().all(|t| t == "hello"));
}

#[test]
fn extract_terms_filters_boolean_operators() {
    let terms = extract_terms("migration AND plan OR deploy NOT test");
    // AND, OR, NOT should be filtered as operators
    for t in &terms {
        assert!(
            !["AND", "OR", "NOT"].contains(&t.as_str()),
            "operator {t} should be filtered"
        );
    }
}

#[test]
fn extract_terms_empty_input() {
    assert!(extract_terms("").is_empty());
    assert!(extract_terms("   ").is_empty());
}

// ═══════════════════════════════════════════════════════════════════════
// Section 2: Filter composition correctness
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn filter_default_has_no_active_fields() {
    let filter = SearchFilter::default();
    assert!(!has_active_filters(&filter));
    assert_eq!(active_filter_count(&filter), 0);
}

#[test]
fn filter_importance_any_is_not_active() {
    let filter = SearchFilter {
        importance: Some(ImportanceFilter::Any),
        ..Default::default()
    };
    assert!(!has_active_filters(&filter));
    assert_eq!(active_filter_count(&filter), 0);
}

#[test]
fn filter_importance_urgent_is_active() {
    let filter = SearchFilter {
        importance: Some(ImportanceFilter::Urgent),
        ..Default::default()
    };
    assert!(has_active_filters(&filter));
    assert_eq!(active_filter_count(&filter), 1);
}

#[test]
fn filter_importance_high_is_active() {
    let filter = SearchFilter {
        importance: Some(ImportanceFilter::High),
        ..Default::default()
    };
    assert!(has_active_filters(&filter));
    assert_eq!(active_filter_count(&filter), 1);
}

#[test]
fn filter_importance_normal_is_active() {
    let filter = SearchFilter {
        importance: Some(ImportanceFilter::Normal),
        ..Default::default()
    };
    assert!(has_active_filters(&filter));
    assert_eq!(active_filter_count(&filter), 1);
}

#[test]
fn filter_all_six_fields_active() {
    let filter = SearchFilter {
        sender: Some("Alice".to_string()),
        project_id: Some(42),
        thread_id: Some("br-123".to_string()),
        doc_kind: Some(mcp_agent_mail_core::DocKind::Message),
        importance: Some(ImportanceFilter::Urgent),
        date_range: Some(DateRange {
            start: Some(1_000_000),
            end: Some(2_000_000),
        }),
    };
    assert!(has_active_filters(&filter));
    assert_eq!(active_filter_count(&filter), 6);
}

#[test]
fn filter_count_each_field_individually() {
    // Test each field independently
    let tests: Vec<SearchFilter> = vec![
        SearchFilter {
            sender: Some("x".into()),
            ..Default::default()
        },
        SearchFilter {
            project_id: Some(1),
            ..Default::default()
        },
        SearchFilter {
            thread_id: Some("t".into()),
            ..Default::default()
        },
        SearchFilter {
            doc_kind: Some(mcp_agent_mail_core::DocKind::Agent),
            ..Default::default()
        },
        SearchFilter {
            date_range: Some(DateRange {
                start: Some(1),
                end: None,
            }),
            ..Default::default()
        },
    ];
    for (i, filter) in tests.iter().enumerate() {
        assert_eq!(
            active_filter_count(filter),
            1,
            "filter {i} should have count=1"
        );
        assert!(has_active_filters(filter), "filter {i} should be active");
    }
}

#[test]
fn filter_date_range_both_none_not_counted() {
    let filter = SearchFilter {
        date_range: Some(DateRange {
            start: None,
            end: None,
        }),
        ..Default::default()
    };
    // A date range with both bounds None is technically set but meaningless
    // active_filter_count should still count it as 1 (the field is Some)
    assert_eq!(active_filter_count(&filter), 1);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 3: Fusion deterministic ordering and tie-break
// ═══════════════════════════════════════════════════════════════════════

const fn make_prepared(
    doc_id: i64,
    lex_rank: Option<usize>,
    sem_rank: Option<usize>,
    lex_score: Option<f64>,
    sem_score: Option<f64>,
) -> PreparedCandidate {
    PreparedCandidate {
        doc_id,
        lexical_rank: lex_rank,
        semantic_rank: sem_rank,
        lexical_score: lex_score,
        semantic_score: sem_score,
        first_source: if lex_rank.is_some() {
            CandidateSource::Lexical
        } else {
            CandidateSource::Semantic
        },
    }
}

#[test]
fn fusion_rrf_score_formula_exact() {
    let config = RrfConfig::default(); // k=60
    let candidates = vec![make_prepared(1, Some(1), Some(2), Some(1.0), Some(0.9))];
    let result = fuse_rrf(&candidates, config, 0, 100);

    let hit = &result.hits[0];
    // RRF score = 1/(60+1) + 1/(60+2) = 1/61 + 1/62
    let expected = 1.0 / 61.0 + 1.0 / 62.0;
    assert!(
        (hit.rrf_score - expected).abs() < 1e-10,
        "expected {expected}, got {}",
        hit.rrf_score
    );
}

#[test]
fn fusion_dual_source_outranks_single() {
    let config = RrfConfig::default();
    let candidates = vec![
        make_prepared(1, Some(1), None, Some(1.0), None), // lexical only
        make_prepared(2, Some(2), Some(1), Some(0.8), Some(1.0)), // both sources
    ];
    let result = fuse_rrf(&candidates, config, 0, 100);

    assert_eq!(result.hits.len(), 2);
    // Doc 2 should rank first (dual source contribution)
    assert_eq!(
        result.hits[0].doc_id, 2,
        "dual-source doc should rank first"
    );
    assert!(result.hits[0].rrf_score > result.hits[1].rrf_score);
}

#[test]
fn fusion_deterministic_across_runs() {
    let config = RrfConfig::default();
    let candidates: Vec<PreparedCandidate> = (1_i64..=20)
        .map(|i| {
            let u = i as usize;
            let f = i as f64;
            make_prepared(i, Some(u), Some(21 - u), Some(1.0 / f), Some(f / 20.0))
        })
        .collect();

    let baseline = fuse_rrf(&candidates, config, 0, 20);
    for _ in 0..50 {
        let run = fuse_rrf(&candidates, config, 0, 20);
        assert_eq!(run.hits.len(), baseline.hits.len());
        for (a, b) in baseline.hits.iter().zip(run.hits.iter()) {
            assert_eq!(a.doc_id, b.doc_id, "ordering diverged");
            assert!((a.rrf_score - b.rrf_score).abs() < 1e-15, "score diverged");
        }
    }
}

#[test]
fn fusion_doc_id_tiebreaker() {
    let config = RrfConfig::default();
    // Two docs with identical ranks → identical RRF scores → lower doc_id wins
    let candidates = vec![
        make_prepared(99, Some(1), None, Some(1.0), None),
        make_prepared(1, Some(1), None, Some(1.0), None),
    ];
    let result = fuse_rrf(&candidates, config, 0, 100);
    assert_eq!(result.hits[0].doc_id, 1, "lower doc_id should win on tie");
    assert_eq!(result.hits[1].doc_id, 99);
}

#[test]
fn fusion_pagination_offset_and_limit() {
    let config = RrfConfig::default();
    let candidates: Vec<PreparedCandidate> = (1_i64..=10)
        .map(|i| make_prepared(i, Some(i as usize), None, Some(1.0), None))
        .collect();

    let full = fuse_rrf(&candidates, config, 0, 10);
    let page = fuse_rrf(&candidates, config, 3, 3);

    assert_eq!(page.hits.len(), 3);
    assert_eq!(page.offset_applied, 3);
    assert_eq!(page.limit_applied, 3);
    // Pages should match slices of full results
    for (i, hit) in page.hits.iter().enumerate() {
        assert_eq!(hit.doc_id, full.hits[i + 3].doc_id);
    }
}

#[test]
fn fusion_empty_candidates() {
    let result = fuse_rrf_default(&[]);
    assert!(result.hits.is_empty());
    assert_eq!(result.total_fused, 0);
    assert_eq!(result.input_count, 0);
}

#[test]
fn fusion_explain_has_source_contributions() {
    let config = RrfConfig::default();
    let candidates = vec![make_prepared(1, Some(1), Some(3), Some(1.0), Some(0.8))];
    let result = fuse_rrf(&candidates, config, 0, 100);

    let explain = &result.hits[0].explain;
    assert_eq!(explain.lexical_rank, Some(1));
    assert_eq!(explain.semantic_rank, Some(3));
    assert!(explain.lexical_score.is_some());
    assert!(explain.semantic_score.is_some());
    assert_eq!(explain.source_contributions.len(), 2);
}

#[test]
fn fusion_custom_k_changes_scores() {
    let low_k = RrfConfig {
        k: 10.0,
        ..Default::default()
    };
    let high_k = RrfConfig {
        k: 100.0,
        ..Default::default()
    };
    let candidates = vec![make_prepared(1, Some(1), Some(1), Some(1.0), Some(1.0))];

    let low = fuse_rrf(&candidates, low_k, 0, 10);
    let high = fuse_rrf(&candidates, high_k, 0, 10);

    // Lower k → higher individual scores → higher RRF
    assert!(
        low.hits[0].rrf_score > high.hits[0].rrf_score,
        "lower k should produce higher scores"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Section 4: Budget derivation math
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn query_class_identifier_patterns() {
    assert_eq!(QueryClass::classify("br-123"), QueryClass::Identifier);
    assert_eq!(QueryClass::classify("thread:abc"), QueryClass::Identifier);
    assert_eq!(QueryClass::classify("TKT-789"), QueryClass::Identifier);
    assert_eq!(QueryClass::classify("abc_def_ghi"), QueryClass::Identifier);
    assert_eq!(QueryClass::classify("path/to/file"), QueryClass::Identifier);
}

#[test]
fn query_class_short_keyword() {
    assert_eq!(QueryClass::classify("deploy"), QueryClass::ShortKeyword);
    assert_eq!(
        QueryClass::classify("migration plan"),
        QueryClass::ShortKeyword
    );
}

#[test]
fn query_class_natural_language() {
    assert_eq!(
        QueryClass::classify("what is the best deployment strategy for microservices"),
        QueryClass::NaturalLanguage
    );
}

#[test]
fn query_class_empty() {
    assert_eq!(QueryClass::classify(""), QueryClass::Empty);
    assert_eq!(QueryClass::classify("   "), QueryClass::Empty);
}

#[test]
fn budget_hybrid_mode_allocates_both_pools() {
    let config = CandidateBudgetConfig::default();
    let budget =
        CandidateBudget::derive(100, CandidateMode::Hybrid, QueryClass::ShortKeyword, config);
    assert!(budget.lexical_limit > 0, "hybrid should have lexical");
    assert!(budget.semantic_limit > 0, "hybrid should have semantic");
}

#[test]
fn budget_lexical_fallback_has_zero_semantic() {
    let config = CandidateBudgetConfig::default();
    let budget = CandidateBudget::derive(
        100,
        CandidateMode::LexicalFallback,
        QueryClass::ShortKeyword,
        config,
    );
    assert!(budget.lexical_limit > 0);
    assert_eq!(budget.semantic_limit, 0, "fallback should have 0 semantic");
}

#[test]
fn budget_empty_query_has_zero_semantic() {
    let config = CandidateBudgetConfig::default();
    let budget = CandidateBudget::derive(100, CandidateMode::Hybrid, QueryClass::Empty, config);
    assert!(budget.lexical_limit > 0);
    assert_eq!(
        budget.semantic_limit, 0,
        "empty query should have 0 semantic"
    );
}

#[test]
fn budget_identifier_query_favors_lexical() {
    let config = CandidateBudgetConfig::default();
    let budget =
        CandidateBudget::derive(100, CandidateMode::Hybrid, QueryClass::Identifier, config);
    assert!(
        budget.lexical_limit >= budget.semantic_limit,
        "identifier query should favor lexical: lex={}, sem={}",
        budget.lexical_limit,
        budget.semantic_limit
    );
}

#[test]
fn budget_natural_language_favors_semantic() {
    let config = CandidateBudgetConfig::default();
    let budget = CandidateBudget::derive(
        100,
        CandidateMode::Hybrid,
        QueryClass::NaturalLanguage,
        config,
    );
    assert!(
        budget.semantic_limit >= budget.lexical_limit,
        "NL query should favor semantic: lex={}, sem={}",
        budget.lexical_limit,
        budget.semantic_limit
    );
}

#[test]
fn budget_combined_limit_at_least_requested() {
    let config = CandidateBudgetConfig::default();
    for requested in [1, 10, 50, 100, 500] {
        let budget = CandidateBudget::derive(
            requested,
            CandidateMode::Hybrid,
            QueryClass::ShortKeyword,
            config,
        );
        assert!(
            budget.combined_limit >= requested,
            "combined_limit {} < requested {requested}",
            budget.combined_limit
        );
    }
}

#[test]
fn budget_derive_with_decision_gives_same_budget() {
    let config = CandidateBudgetConfig::default();
    let budget =
        CandidateBudget::derive(50, CandidateMode::Hybrid, QueryClass::ShortKeyword, config);
    let derivation = CandidateBudget::derive_with_decision(
        50,
        CandidateMode::Hybrid,
        QueryClass::ShortKeyword,
        config,
    );
    assert_eq!(budget.lexical_limit, derivation.budget.lexical_limit);
    assert_eq!(budget.semantic_limit, derivation.budget.semantic_limit);
    assert_eq!(budget.combined_limit, derivation.budget.combined_limit);
}

#[test]
fn budget_decision_posterior_sums_to_one() {
    let config = CandidateBudgetConfig::default();
    for mode in [
        CandidateMode::Hybrid,
        CandidateMode::Auto,
        CandidateMode::LexicalFallback,
    ] {
        for class in [
            QueryClass::Identifier,
            QueryClass::ShortKeyword,
            QueryClass::NaturalLanguage,
            QueryClass::Empty,
        ] {
            let derivation = CandidateBudget::derive_with_decision(100, mode, class, config);
            let p = &derivation.decision.posterior;
            let sum = p.identifier + p.short_keyword + p.natural_language + p.empty;
            assert!(
                (sum - 1.0).abs() < 1e-6,
                "posterior sum={sum} for {mode:?}/{class:?}"
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Section 5: Candidate preparation and dedup
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn prepare_deduplicates_overlapping_hits() {
    let lexical = vec![
        CandidateHit::new(1, 0.9),
        CandidateHit::new(2, 0.8),
        CandidateHit::new(3, 0.7),
    ];
    let semantic = vec![
        CandidateHit::new(2, 0.85), // overlap with lexical
        CandidateHit::new(4, 0.6),
    ];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 10,
    };

    let prep = prepare_candidates(&lexical, &semantic, budget);
    assert_eq!(prep.candidates.len(), 4, "should have 4 unique docs");
    assert_eq!(prep.counts.duplicates_removed, 1, "1 duplicate removed");

    // Doc 2 should have both ranks
    let doc2 = prep.candidates.iter().find(|c| c.doc_id == 2).unwrap();
    assert!(doc2.lexical_rank.is_some());
    assert!(doc2.semantic_rank.is_some());
}

#[test]
fn prepare_respects_budget_limits() {
    let lexical: Vec<CandidateHit> = (1..=100)
        .map(|i| CandidateHit::new(i, 1.0 / i as f64))
        .collect();
    let semantic: Vec<CandidateHit> = (101..=200)
        .map(|i| CandidateHit::new(i, 1.0 / i as f64))
        .collect();
    let budget = CandidateBudget {
        lexical_limit: 5,
        semantic_limit: 5,
        combined_limit: 8,
    };

    let prep = prepare_candidates(&lexical, &semantic, budget);
    assert!(
        prep.candidates.len() <= 8,
        "should be capped at combined_limit"
    );
    assert!(prep.counts.lexical_selected <= 5, "lexical capped at 5");
    assert!(prep.counts.semantic_selected <= 5, "semantic capped at 5");
}

#[test]
fn prepare_deterministic_ordering() {
    let lexical = vec![
        CandidateHit::new(3, 0.5),
        CandidateHit::new(1, 0.9),
        CandidateHit::new(2, 0.7),
    ];
    let semantic = vec![CandidateHit::new(4, 0.6)];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 10,
    };

    let prep1 = prepare_candidates(&lexical, &semantic, budget);
    let prep2 = prepare_candidates(&lexical, &semantic, budget);

    assert_eq!(prep1.candidates.len(), prep2.candidates.len());
    for (a, b) in prep1.candidates.iter().zip(prep2.candidates.iter()) {
        assert_eq!(a.doc_id, b.doc_id, "ordering should be deterministic");
    }
}

#[test]
fn prepare_empty_both_pools() {
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 10,
    };
    let prep = prepare_candidates(&[], &[], budget);
    assert!(prep.candidates.is_empty());
    assert_eq!(prep.counts.lexical_considered, 0);
    assert_eq!(prep.counts.semantic_considered, 0);
}

#[test]
fn prepare_first_source_attribution() {
    let lexical = vec![CandidateHit::new(1, 0.9)];
    let semantic = vec![CandidateHit::new(2, 0.8)];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 10,
    };

    let prep = prepare_candidates(&lexical, &semantic, budget);
    let doc1 = prep.candidates.iter().find(|c| c.doc_id == 1).unwrap();
    let doc2 = prep.candidates.iter().find(|c| c.doc_id == 2).unwrap();
    assert_eq!(doc1.first_source, CandidateSource::Lexical);
    assert_eq!(doc2.first_source, CandidateSource::Semantic);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 6: Explain payload invariants
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn explain_all_stages_present_in_composed_hit() {
    let config = ExplainComposerConfig::default();
    // Even with only one stage input, all 4 stages appear in output
    let hit = compose_hit_explanation(1, 0.5, vec![], &config);
    assert_eq!(hit.stages.len(), 4, "all 4 canonical stages required");
    assert_eq!(hit.stages[0].stage, ExplainStage::Lexical);
    assert_eq!(hit.stages[1].stage, ExplainStage::Semantic);
    assert_eq!(hit.stages[2].stage, ExplainStage::Fusion);
    assert_eq!(hit.stages[3].stage, ExplainStage::Rerank);
}

#[test]
fn explain_reason_codes_are_sorted_and_deduped() {
    let config = ExplainComposerConfig::default();
    let hit = compose_hit_explanation(
        1,
        0.5,
        vec![
            StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.3,
                stage_weight: 1.0,
                score_factors: vec![],
            },
            StageScoreInput {
                stage: ExplainStage::Rerank,
                reason_code: ExplainReasonCode::RerankPolicyBoost,
                summary: None,
                stage_score: 0.2,
                stage_weight: 1.0,
                score_factors: vec![],
            },
        ],
        &config,
    );
    // reason_codes should be sorted
    for w in hit.reason_codes.windows(2) {
        assert!(
            w[0] <= w[1],
            "reason_codes not sorted: {:?} > {:?}",
            w[0],
            w[1]
        );
    }
    // Should include provided codes plus StageNotExecuted for missing stages
    assert!(hit.reason_codes.contains(&ExplainReasonCode::LexicalBm25));
    assert!(
        hit.reason_codes
            .contains(&ExplainReasonCode::RerankPolicyBoost)
    );
    assert!(
        hit.reason_codes
            .contains(&ExplainReasonCode::StageNotExecuted)
    );
}

#[test]
fn explain_report_preserves_timings() {
    let config = ExplainComposerConfig::default();
    let mut timings = HashMap::new();
    timings.insert("retrieval".to_owned(), Duration::from_millis(5));
    timings.insert("rerank".to_owned(), Duration::from_millis(2));
    timings.insert("total".to_owned(), Duration::from_millis(10));

    let report = compose_explain_report(SearchMode::Hybrid, 100, timings, vec![], &config);
    assert_eq!(report.phase_timings.len(), 3);
    assert_eq!(
        report.phase_timings.get("retrieval"),
        Some(&Duration::from_millis(5))
    );
}

#[test]
fn explain_redaction_clears_all_sensitive_data() {
    let config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Detailed,
        max_factors_per_stage: 10,
    };
    let mut hit = compose_hit_explanation(
        1,
        0.95,
        vec![
            StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: Some("BM25 match on 'deployment'".to_owned()),
                stage_score: 0.7,
                stage_weight: 0.6,
                score_factors: vec![
                    ScoreFactor {
                        code: ExplainReasonCode::LexicalBm25,
                        key: "bm25_raw".to_owned(),
                        contribution: 0.5,
                        detail: Some("idf=3.2, tf=2".to_owned()),
                    },
                    ScoreFactor {
                        code: ExplainReasonCode::LexicalTermCoverage,
                        key: "coverage".to_owned(),
                        contribution: 0.2,
                        detail: Some("3/4 terms matched".to_owned()),
                    },
                ],
            },
            StageScoreInput {
                stage: ExplainStage::Semantic,
                reason_code: ExplainReasonCode::SemanticCosine,
                summary: Some("Vector similarity 0.87".to_owned()),
                stage_score: 0.87,
                stage_weight: 0.4,
                score_factors: vec![ScoreFactor {
                    code: ExplainReasonCode::SemanticCosine,
                    key: "cosine".to_owned(),
                    contribution: 0.87,
                    detail: Some("dim=384, model=minilm".to_owned()),
                }],
            },
        ],
        &config,
    );

    redact_hit_explanation(&mut hit, ExplainReasonCode::ScopeDenied);

    // Verify no sensitive data remains
    let json = serde_json::to_string(&hit).unwrap();
    assert!(!json.contains("idf="));
    assert!(!json.contains("tf="));
    assert!(!json.contains("minilm"));
    assert!(!json.contains("cosine"));
    assert!(!json.contains("bm25_raw"));
    assert!(!json.contains("BM25 match"));
    assert!(!json.contains("Vector similarity"));
}

#[test]
fn explain_report_serde_stable() {
    let config = ExplainComposerConfig::default();
    let hit = compose_hit_explanation(
        42,
        0.75,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.75,
            stage_weight: 1.0,
            score_factors: vec![],
        }],
        &config,
    );

    let report =
        compose_explain_report(SearchMode::Lexical, 50, HashMap::new(), vec![hit], &config);

    // Round-trip through JSON
    let json = serde_json::to_string(&report).unwrap();
    let restored: mcp_agent_mail_core::ExplainReport = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.mode_used, SearchMode::Lexical);
    assert_eq!(restored.candidates_evaluated, 50);
    assert_eq!(restored.taxonomy_version, 1);
    assert_eq!(restored.hits.len(), 1);
    assert_eq!(restored.hits[0].doc_id, 42);
    assert!((restored.hits[0].final_score - 0.75).abs() < f64::EPSILON);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 7: End-to-end pipeline integration
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn pipeline_candidates_to_fusion_to_explain() {
    // Simulate: lexical retrieval → semantic retrieval → prepare → fuse → explain
    let lexical_hits = vec![
        CandidateHit::new(1, 10.0),
        CandidateHit::new(2, 8.0),
        CandidateHit::new(3, 5.0),
    ];
    let semantic_hits = vec![
        CandidateHit::new(2, 0.95), // overlap
        CandidateHit::new(4, 0.90),
        CandidateHit::new(5, 0.80),
    ];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 20,
    };

    // Step 1: Prepare candidates
    let prep = prepare_candidates(&lexical_hits, &semantic_hits, budget);
    assert_eq!(prep.candidates.len(), 5);
    assert_eq!(prep.counts.duplicates_removed, 1);

    // Step 2: Fuse with RRF
    // Convert db-crate PreparedCandidate to search-core PreparedCandidate
    // (identical struct layout but different crate origins).
    let fuse_candidates: Vec<PreparedCandidate> = prep
        .candidates
        .iter()
        .map(|c| PreparedCandidate {
            doc_id: c.doc_id,
            lexical_rank: c.lexical_rank,
            semantic_rank: c.semantic_rank,
            lexical_score: c.lexical_score,
            semantic_score: c.semantic_score,
            first_source: match c.first_source {
                mcp_agent_mail_db::search_candidates::CandidateSource::Lexical => {
                    CandidateSource::Lexical
                }
                mcp_agent_mail_db::search_candidates::CandidateSource::Semantic => {
                    CandidateSource::Semantic
                }
            },
        })
        .collect();
    let config = RrfConfig::default();
    let fusion = fuse_rrf(&fuse_candidates, config, 0, 10);
    assert_eq!(fusion.hits.len(), 5);

    // Doc 2 should rank highest (dual source)
    assert_eq!(
        fusion.hits[0].doc_id, 2,
        "dual-source doc should rank first"
    );

    // Step 3: Compose explain for top hit
    let explain_config = ExplainComposerConfig::default();
    let top = &fusion.hits[0];
    let explanation = compose_hit_explanation(
        top.doc_id,
        top.rrf_score,
        vec![
            StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: top.explain.lexical_score.unwrap_or(0.0),
                stage_weight: 0.6,
                score_factors: vec![],
            },
            StageScoreInput {
                stage: ExplainStage::Semantic,
                reason_code: ExplainReasonCode::SemanticCosine,
                summary: None,
                stage_score: top.explain.semantic_score.unwrap_or(0.0),
                stage_weight: 0.4,
                score_factors: vec![],
            },
        ],
        &explain_config,
    );

    assert_eq!(explanation.doc_id, 2);
    assert_eq!(explanation.stages.len(), 4);
    assert!(explanation.final_score > 0.0);
}
