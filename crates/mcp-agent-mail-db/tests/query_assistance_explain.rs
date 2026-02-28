//! Integration tests for query assistance, zero-result guidance, and explain reason codes.
//!
//! br-2tnl.7.13: Unit tests for query assistance, zero-result guidance, and explain reason codes
//!
//! Validates:
//! - Structured query hint parsing and typo recovery behavior
//! - Zero-result guidance generation and determinism checks
//! - Explain reason-code stability and payload-size guard behavior
//! - Scope/redaction-safe hint/explain output under restricted visibility

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use mcp_agent_mail_core::SearchMode;
use mcp_agent_mail_core::{
    ExplainComposerConfig, ExplainReasonCode, ExplainReport, ExplainStage, ExplainVerbosity,
    ScoreFactor, SearchResults, StageScoreInput, compose_explain_report,
    compose_hit_explanation, redact_hit_explanation, redact_report_for_docs,
};
use mcp_agent_mail_db::query_assistance::{
    AppliedFilterHint, DidYouMeanHint, QueryAssistance, parse_query_assistance,
};

// ── Section 1: Structured query hint parsing and typo recovery ──────

#[test]
fn hint_parsing_multiple_fields_interleaved_with_text() {
    let qa = parse_query_assistance("from:Alice deploy fix thread:br-99 urgent");
    assert_eq!(qa.query_text, "deploy fix urgent");
    assert_eq!(qa.applied_filter_hints.len(), 2);
    assert_eq!(qa.applied_filter_hints[0].field, "from");
    assert_eq!(qa.applied_filter_hints[0].value, "Alice");
    assert_eq!(qa.applied_filter_hints[1].field, "thread");
    assert_eq!(qa.applied_filter_hints[1].value, "br-99");
}

#[test]
fn hint_parsing_all_canonical_fields() {
    let qa = parse_query_assistance(
        "from:A thread:B project:C before:2026-01-01 after:2025-12-01 importance:high text",
    );
    assert_eq!(qa.query_text, "text");
    assert_eq!(qa.applied_filter_hints.len(), 6);
    let fields: Vec<&str> = qa
        .applied_filter_hints
        .iter()
        .map(|h| h.field.as_str())
        .collect();
    assert!(fields.contains(&"from"));
    assert!(fields.contains(&"thread"));
    assert!(fields.contains(&"project"));
    assert!(fields.contains(&"before"));
    assert!(fields.contains(&"after"));
    assert!(fields.contains(&"importance"));
}

#[test]
fn hint_parsing_all_aliases_resolve_to_canonical() {
    let qa = parse_query_assistance(
        "sender:X frm:Y thread_id:Z thr:W proj:V since:2026 until:2027 priority:high prio:low imp:normal",
    );
    // All aliases should resolve to canonical field names
    let fields: Vec<&str> = qa
        .applied_filter_hints
        .iter()
        .map(|h| h.field.as_str())
        .collect();
    for f in &fields {
        assert!(
            ["from", "thread", "project", "before", "after", "importance"].contains(f),
            "unexpected field: {f}"
        );
    }
    // sender → from, frm → from
    let from_count = fields.iter().filter(|f| **f == "from").count();
    assert_eq!(
        from_count, 2,
        "expected 2 'from' fields from sender/frm aliases"
    );
}

#[test]
fn typo_recovery_suggests_closest_field() {
    let qa = parse_query_assistance("fron:Alice migration");
    assert_eq!(qa.did_you_mean.len(), 1);
    assert_eq!(qa.did_you_mean[0].token, "fron:Alice");
    assert_eq!(qa.did_you_mean[0].suggested_field, "from");
    assert_eq!(qa.did_you_mean[0].value, "Alice");
    // Typo token stays in query_text
    assert_eq!(qa.query_text, "fron:Alice migration");
}

#[test]
fn typo_recovery_multiple_suggestions_ordered_deterministically() {
    let qa1 = parse_query_assistance("thred:br-1 imporance:high befre:2026-01-01 body");
    let qa2 = parse_query_assistance("thred:br-1 imporance:high befre:2026-01-01 body");

    // Same input → identical suggestions
    assert_eq!(qa1.did_you_mean.len(), qa2.did_you_mean.len());
    for (a, b) in qa1.did_you_mean.iter().zip(qa2.did_you_mean.iter()) {
        assert_eq!(a.suggested_field, b.suggested_field);
        assert_eq!(a.token, b.token);
        assert_eq!(a.value, b.value);
    }

    // Verify correct suggestions
    let suggestions: Vec<&str> = qa1
        .did_you_mean
        .iter()
        .map(|h| h.suggested_field.as_str())
        .collect();
    assert!(suggestions.contains(&"thread"));
    assert!(suggestions.contains(&"importance"));
    assert!(suggestions.contains(&"before"));
}

#[test]
fn typo_recovery_rejects_completely_unrelated_fields() {
    let qa = parse_query_assistance("zzzzzzz:val xyz:something");
    // No suggestions for completely unrelated tokens (distance > 2)
    assert!(
        qa.did_you_mean.is_empty(),
        "expected no suggestions for distant fields, got {:?}",
        qa.did_you_mean
    );
    // Both kept in query_text
    assert!(qa.query_text.contains("zzzzzzz:val"));
    assert!(qa.query_text.contains("xyz:something"));
}

#[test]
fn hint_parsing_case_insensitive() {
    let qa = parse_query_assistance("FROM:Alice THREAD:br-42 IMPORTANCE:high text");
    assert_eq!(qa.applied_filter_hints.len(), 3);
    assert_eq!(qa.applied_filter_hints[0].field, "from");
    assert_eq!(qa.applied_filter_hints[1].field, "thread");
    assert_eq!(qa.applied_filter_hints[2].field, "importance");
}

#[test]
fn hint_parsing_quoted_value_with_spaces() {
    let qa = parse_query_assistance("from:\"Blue Lake\" project:\"backend-api\" search");
    assert_eq!(qa.applied_filter_hints.len(), 2);
    assert_eq!(qa.applied_filter_hints[0].value, "Blue Lake");
    assert_eq!(qa.applied_filter_hints[1].value, "backend-api");
    assert_eq!(qa.query_text, "search");
}

#[test]
fn hint_parsing_colon_in_value_preserved() {
    let qa = parse_query_assistance("from:http://example.com body");
    assert_eq!(qa.applied_filter_hints.len(), 1);
    assert_eq!(qa.applied_filter_hints[0].value, "http://example.com");
}

#[test]
fn hint_parsing_empty_value_not_extracted() {
    // "from:" has empty value → treated as plain text
    let qa = parse_query_assistance("from: hello");
    assert!(qa.applied_filter_hints.is_empty());
    assert_eq!(qa.query_text, "from: hello");
}

// ── Section 2: Zero-result guidance and determinism ─────────────────

#[test]
fn zero_result_empty_search_results() {
    for mode in [
        SearchMode::Lexical,
        SearchMode::Semantic,
        SearchMode::Hybrid,
        SearchMode::Auto,
    ] {
        let results = SearchResults::empty(mode, Duration::from_millis(5));
        assert!(results.is_empty());
        assert_eq!(results.total_count, 0);
        assert_eq!(results.mode_used, mode);
        assert!(results.explain.is_none());
    }
}

#[test]
fn zero_result_guidance_no_assistance_for_plain_text() {
    // Plain text query → no hints → assistance should be None
    let qa = parse_query_assistance("just regular search terms");
    let is_empty = qa.applied_filter_hints.is_empty() && qa.did_you_mean.is_empty();
    assert!(is_empty, "plain text should produce no hints/suggestions");
}

#[test]
fn zero_result_guidance_assistance_present_for_hints() {
    let qa = parse_query_assistance("from:Alice deployment");
    // Has a hint → assistance should be non-empty
    assert!(!qa.applied_filter_hints.is_empty());
    assert_eq!(qa.query_text, "deployment");
}

#[test]
fn zero_result_guidance_assistance_present_for_typos() {
    let qa = parse_query_assistance("fron:Alice deployment");
    assert!(
        !qa.did_you_mean.is_empty(),
        "typo should produce suggestions"
    );
}

#[test]
fn zero_result_determinism_same_input_same_output() {
    let inputs = [
        "",
        "   ",
        "simple query",
        "from:Alice deploy",
        "fron:Alice typo",
        "from:\"Blue Lake\" thread:br-42 importance:high migration plan",
    ];
    for input in inputs {
        let a = parse_query_assistance(input);
        let b = parse_query_assistance(input);
        assert_eq!(
            a.query_text, b.query_text,
            "query_text diverged for: {input}"
        );
        assert_eq!(
            a.applied_filter_hints, b.applied_filter_hints,
            "hints diverged for: {input}"
        );
        assert_eq!(
            a.did_you_mean, b.did_you_mean,
            "suggestions diverged for: {input}"
        );
    }
}

#[test]
fn zero_result_empty_input_variations() {
    for input in ["", "   ", "\t", "\n", "  \t  \n  "] {
        let qa = parse_query_assistance(input);
        assert!(qa.query_text.is_empty() || qa.query_text.trim().is_empty());
        assert!(qa.applied_filter_hints.is_empty());
        assert!(qa.did_you_mean.is_empty());
    }
}

// ── Section 3: Explain reason-code stability and payload-size guards ─

#[test]
fn reason_code_serde_snake_case_stability() {
    // Verify snake_case serialization is stable — this is the wire format
    let test_cases = [
        (ExplainReasonCode::LexicalBm25, "\"lexical_bm25\""),
        (
            ExplainReasonCode::LexicalTermCoverage,
            "\"lexical_term_coverage\"",
        ),
        (ExplainReasonCode::SemanticCosine, "\"semantic_cosine\""),
        (
            ExplainReasonCode::SemanticNeighborhood,
            "\"semantic_neighborhood\"",
        ),
        (
            ExplainReasonCode::FusionWeightedBlend,
            "\"fusion_weighted_blend\"",
        ),
        (
            ExplainReasonCode::RerankPolicyBoost,
            "\"rerank_policy_boost\"",
        ),
        (
            ExplainReasonCode::RerankPolicyPenalty,
            "\"rerank_policy_penalty\"",
        ),
        (
            ExplainReasonCode::StageNotExecuted,
            "\"stage_not_executed\"",
        ),
        (ExplainReasonCode::ScopeRedacted, "\"scope_redacted\""),
        (ExplainReasonCode::ScopeDenied, "\"scope_denied\""),
    ];
    for (code, expected_json) in test_cases {
        let json = serde_json::to_string(&code).unwrap();
        assert_eq!(json, expected_json, "serde output changed for {code:?}");
    }
}

#[test]
fn reason_code_all_have_nonempty_summary() {
    let all_codes = [
        ExplainReasonCode::LexicalBm25,
        ExplainReasonCode::LexicalTermCoverage,
        ExplainReasonCode::SemanticCosine,
        ExplainReasonCode::SemanticNeighborhood,
        ExplainReasonCode::FusionWeightedBlend,
        ExplainReasonCode::RerankPolicyBoost,
        ExplainReasonCode::RerankPolicyPenalty,
        ExplainReasonCode::StageNotExecuted,
        ExplainReasonCode::ScopeRedacted,
        ExplainReasonCode::ScopeDenied,
    ];
    for code in all_codes {
        let summary = code.summary();
        assert!(!summary.is_empty(), "empty summary for {code:?}");
        assert!(
            summary.len() > 5,
            "summary too short for {code:?}: {summary}"
        );
    }
}

#[test]
fn explain_stage_serde_snake_case_stability() {
    let test_cases = [
        (ExplainStage::Lexical, "\"lexical\""),
        (ExplainStage::Semantic, "\"semantic\""),
        (ExplainStage::Fusion, "\"fusion\""),
        (ExplainStage::Rerank, "\"rerank\""),
    ];
    for (stage, expected_json) in test_cases {
        let json = serde_json::to_string(&stage).unwrap();
        assert_eq!(json, expected_json, "serde output changed for {stage:?}");
    }
}

#[test]
fn explain_verbosity_controls_factor_detail() {
    // Minimal → no factors
    let minimal_config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Minimal,
        max_factors_per_stage: 10,
    };
    let hit_minimal = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: vec![ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "bm25".to_owned(),
                contribution: 0.5,
                detail: Some("raw=12.5".to_owned()),
            }],
        }],
        &minimal_config,
    );
    // Minimal hides all factors
    assert!(
        hit_minimal.stages[0].score_factors.is_empty(),
        "Minimal verbosity should hide factors"
    );

    // Standard → factors present but detail stripped
    let standard_config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Standard,
        max_factors_per_stage: 10,
    };
    let hit_standard = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: vec![ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "bm25".to_owned(),
                contribution: 0.5,
                detail: Some("raw=12.5".to_owned()),
            }],
        }],
        &standard_config,
    );
    assert_eq!(hit_standard.stages[0].score_factors.len(), 1);
    assert!(
        hit_standard.stages[0].score_factors[0].detail.is_none(),
        "Standard verbosity should strip detail"
    );

    // Detailed → full factors with detail
    let detailed_config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Detailed,
        max_factors_per_stage: 10,
    };
    let hit_detailed = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: vec![ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "bm25".to_owned(),
                contribution: 0.5,
                detail: Some("raw=12.5".to_owned()),
            }],
        }],
        &detailed_config,
    );
    assert_eq!(
        hit_detailed.stages[0].score_factors[0].detail.as_deref(),
        Some("raw=12.5")
    );
}

#[test]
fn payload_size_guard_max_factors_truncation() {
    // Create 20 factors but limit to 3
    let factors: Vec<ScoreFactor> = (0..20)
        .map(|i| ScoreFactor {
            code: ExplainReasonCode::LexicalBm25,
            key: format!("factor_{i}"),
            contribution: 1.0 / (f64::from(i) + 1.0),
            detail: Some(format!("detail_{i}")),
        })
        .collect();

    let config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Standard,
        max_factors_per_stage: 3,
    };

    let hit = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: factors,
        }],
        &config,
    );

    assert_eq!(
        hit.stages[0].score_factors.len(),
        3,
        "should truncate to max_factors_per_stage"
    );
    assert_eq!(
        hit.stages[0].truncated_factor_count, 17,
        "should report truncated count"
    );
}

#[test]
fn payload_size_guard_zero_max_factors_hides_all() {
    let config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Standard,
        max_factors_per_stage: 0,
    };
    let hit = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: vec![
                ScoreFactor {
                    code: ExplainReasonCode::LexicalBm25,
                    key: "a".to_owned(),
                    contribution: 0.3,
                    detail: None,
                },
                ScoreFactor {
                    code: ExplainReasonCode::LexicalTermCoverage,
                    key: "b".to_owned(),
                    contribution: 0.2,
                    detail: None,
                },
            ],
        }],
        &config,
    );
    assert!(hit.stages[0].score_factors.is_empty());
    assert_eq!(hit.stages[0].truncated_factor_count, 2);
}

#[test]
fn payload_size_guard_serialized_size_bounded() {
    // Large explanation should still produce bounded JSON
    let factors: Vec<ScoreFactor> = (0..100)
        .map(|i| ScoreFactor {
            code: ExplainReasonCode::LexicalBm25,
            key: format!("factor_{i:04}"),
            contribution: 0.01 * f64::from(i),
            detail: Some("x".repeat(200)),
        })
        .collect();

    let config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Standard,
        max_factors_per_stage: 4, // guard
    };

    let hit = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: factors,
        }],
        &config,
    );

    let json = serde_json::to_string(&hit).unwrap();
    // With 4 factors max and detail stripped (Standard), should be reasonable
    assert!(
        json.len() < 10_000,
        "serialized hit too large: {} bytes",
        json.len()
    );
}

#[test]
fn explain_report_taxonomy_version_stable() {
    let config = ExplainComposerConfig::default();
    let report = compose_explain_report(SearchMode::Lexical, 0, HashMap::new(), vec![], &config);
    assert_eq!(
        report.taxonomy_version, 1,
        "taxonomy version must stay at 1"
    );
}

#[test]
fn explain_report_stage_order_canonical() {
    let config = ExplainComposerConfig::default();
    let report = compose_explain_report(SearchMode::Auto, 10, HashMap::new(), vec![], &config);
    assert_eq!(
        report.stage_order,
        vec![
            ExplainStage::Lexical,
            ExplainStage::Semantic,
            ExplainStage::Fusion,
            ExplainStage::Rerank,
        ]
    );
}

#[test]
fn explain_report_missing_stages_filled_with_not_executed() {
    let config = ExplainComposerConfig::default();
    // Only provide lexical stage — other 3 should be StageNotExecuted
    let hit = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: vec![],
        }],
        &config,
    );
    assert_eq!(hit.stages.len(), 4, "all 4 stages must be present");
    assert_eq!(hit.stages[0].reason_code, ExplainReasonCode::LexicalBm25);
    assert_eq!(
        hit.stages[1].reason_code,
        ExplainReasonCode::StageNotExecuted
    );
    assert_eq!(
        hit.stages[2].reason_code,
        ExplainReasonCode::StageNotExecuted
    );
    assert_eq!(
        hit.stages[3].reason_code,
        ExplainReasonCode::StageNotExecuted
    );
}

#[test]
fn explain_weighted_score_math() {
    let config = ExplainComposerConfig::default();
    let hit = compose_hit_explanation(
        1,
        0.75,
        vec![
            StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.6,
                stage_weight: 0.7,
                score_factors: vec![],
            },
            StageScoreInput {
                stage: ExplainStage::Semantic,
                reason_code: ExplainReasonCode::SemanticCosine,
                summary: None,
                stage_score: 0.8,
                stage_weight: 0.3,
                score_factors: vec![],
            },
        ],
        &config,
    );
    // Verify weighted_score = stage_score * stage_weight
    assert!(
        (hit.stages[0].weighted_score - 0.42).abs() < f64::EPSILON,
        "lexical: 0.6 * 0.7 = 0.42, got {}",
        hit.stages[0].weighted_score
    );
    assert!(
        (hit.stages[1].weighted_score - 0.24).abs() < f64::EPSILON,
        "semantic: 0.8 * 0.3 = 0.24, got {}",
        hit.stages[1].weighted_score
    );
}

// ── Section 4: Scope/redaction-safe hint/explain output ─────────────

#[test]
fn redaction_zeroes_scores_and_marks_redacted() {
    let config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Detailed,
        max_factors_per_stage: 10,
    };
    let mut hit = compose_hit_explanation(
        42,
        0.95,
        vec![
            StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.5,
                stage_weight: 0.6,
                score_factors: vec![ScoreFactor {
                    code: ExplainReasonCode::LexicalBm25,
                    key: "bm25".to_owned(),
                    contribution: 0.5,
                    detail: Some("sensitive detail".to_owned()),
                }],
            },
            StageScoreInput {
                stage: ExplainStage::Semantic,
                reason_code: ExplainReasonCode::SemanticCosine,
                summary: None,
                stage_score: 0.8,
                stage_weight: 0.4,
                score_factors: vec![],
            },
        ],
        &config,
    );

    // Pre-redaction: scores are non-zero
    assert!(hit.final_score > 0.0);
    assert!(!hit.stages[0].score_factors.is_empty());

    redact_hit_explanation(&mut hit, ExplainReasonCode::ScopeRedacted);

    // Post-redaction: everything zeroed and redacted
    assert!(
        (hit.final_score).abs() < f64::EPSILON,
        "final_score should be 0"
    );
    assert_eq!(
        hit.reason_codes,
        vec![ExplainReasonCode::ScopeRedacted],
        "only ScopeRedacted reason code"
    );
    for stage in &hit.stages {
        assert!(stage.redacted, "stage should be marked redacted");
        assert!((stage.stage_score).abs() < f64::EPSILON);
        assert!((stage.stage_weight).abs() < f64::EPSILON);
        assert!((stage.weighted_score).abs() < f64::EPSILON);
        assert!(stage.score_factors.is_empty(), "factors should be cleared");
        assert_eq!(stage.reason_code, ExplainReasonCode::ScopeRedacted);
    }
}

#[test]
fn redaction_scope_denied_vs_scope_redacted() {
    let config = ExplainComposerConfig::default();

    for redact_code in [
        ExplainReasonCode::ScopeRedacted,
        ExplainReasonCode::ScopeDenied,
    ] {
        let mut hit = compose_hit_explanation(
            1,
            0.7,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.7,
                stage_weight: 1.0,
                score_factors: vec![],
            }],
            &config,
        );

        redact_hit_explanation(&mut hit, redact_code);

        assert_eq!(hit.reason_codes, vec![redact_code]);
        for stage in &hit.stages {
            assert_eq!(stage.reason_code, redact_code);
        }
    }
}

#[test]
fn redact_report_selective_by_doc_id() {
    let config = ExplainComposerConfig::default();

    let hit_allowed = compose_hit_explanation(
        10,
        0.9,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.9,
            stage_weight: 1.0,
            score_factors: vec![],
        }],
        &config,
    );
    let hit_denied = compose_hit_explanation(
        20,
        0.8,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.8,
            stage_weight: 1.0,
            score_factors: vec![],
        }],
        &config,
    );
    let hit_also_denied = compose_hit_explanation(
        30,
        0.7,
        vec![StageScoreInput {
            stage: ExplainStage::Semantic,
            reason_code: ExplainReasonCode::SemanticCosine,
            summary: None,
            stage_score: 0.7,
            stage_weight: 1.0,
            score_factors: vec![],
        }],
        &config,
    );

    let mut report = compose_explain_report(
        SearchMode::Hybrid,
        3,
        HashMap::new(),
        vec![hit_allowed, hit_denied, hit_also_denied],
        &config,
    );

    let mut deny_set = BTreeSet::new();
    deny_set.insert(20i64);
    deny_set.insert(30i64);
    redact_report_for_docs(&mut report, &deny_set, ExplainReasonCode::ScopeRedacted);

    // Doc 10: NOT redacted
    assert!((report.hits[0].final_score - 0.9).abs() < f64::EPSILON);
    assert!(!report.hits[0].stages[0].redacted);

    // Doc 20: redacted
    assert!((report.hits[1].final_score).abs() < f64::EPSILON);
    assert!(report.hits[1].stages.iter().all(|s| s.redacted));

    // Doc 30: redacted
    assert!((report.hits[2].final_score).abs() < f64::EPSILON);
    assert!(report.hits[2].stages.iter().all(|s| s.redacted));
}

#[test]
fn redacted_explanation_serializes_cleanly() {
    let config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Detailed,
        max_factors_per_stage: 10,
    };
    let mut hit = compose_hit_explanation(
        1,
        0.9,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.9,
            stage_weight: 1.0,
            score_factors: vec![ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "bm25".to_owned(),
                contribution: 0.9,
                detail: Some("secret".to_owned()),
            }],
        }],
        &config,
    );

    redact_hit_explanation(&mut hit, ExplainReasonCode::ScopeDenied);

    let json = serde_json::to_string(&hit).unwrap();
    // Should NOT contain any secret data
    assert!(!json.contains("secret"), "redacted JSON leaks detail");
    assert!(!json.contains("\"bm25\""), "redacted JSON leaks factor key");
    // Should contain the redaction reason
    assert!(json.contains("scope_denied"));
}

#[test]
fn query_assistance_serde_roundtrip_with_all_fields() {
    let qa = QueryAssistance {
        query_text: "migration plan".to_string(),
        applied_filter_hints: vec![
            AppliedFilterHint {
                field: "from".to_string(),
                value: "Alice".to_string(),
            },
            AppliedFilterHint {
                field: "importance".to_string(),
                value: "high".to_string(),
            },
        ],
        did_you_mean: vec![DidYouMeanHint {
            token: "thred:br-42".to_string(),
            suggested_field: "thread".to_string(),
            value: "br-42".to_string(),
        }],
    };

    let json = serde_json::to_string(&qa).unwrap();
    let restored: QueryAssistance = serde_json::from_str(&json).unwrap();
    assert_eq!(qa, restored);

    // Verify key field names in JSON
    assert!(json.contains("\"query_text\""));
    assert!(json.contains("\"applied_filter_hints\""));
    assert!(json.contains("\"did_you_mean\""));
    assert!(json.contains("\"suggested_field\""));
}

#[test]
fn query_assistance_empty_fields_skipped_in_json() {
    let qa = QueryAssistance {
        query_text: "test".to_string(),
        applied_filter_hints: vec![],
        did_you_mean: vec![],
    };
    let json = serde_json::to_string(&qa).unwrap();
    assert!(
        !json.contains("applied_filter_hints"),
        "empty vec should be skipped"
    );
    assert!(
        !json.contains("did_you_mean"),
        "empty vec should be skipped"
    );
}

#[test]
fn full_explain_report_serde_roundtrip() {
    let config = ExplainComposerConfig::default();
    let mut timings = HashMap::new();
    timings.insert("retrieval".to_owned(), Duration::from_millis(5));
    timings.insert("rerank".to_owned(), Duration::from_millis(2));

    let hit = compose_hit_explanation(
        42,
        0.85,
        vec![
            StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: Some("BM25 match".to_owned()),
                stage_score: 0.7,
                stage_weight: 0.6,
                score_factors: vec![ScoreFactor {
                    code: ExplainReasonCode::LexicalBm25,
                    key: "bm25".to_owned(),
                    contribution: 0.7,
                    detail: None,
                }],
            },
            StageScoreInput {
                stage: ExplainStage::Rerank,
                reason_code: ExplainReasonCode::RerankPolicyBoost,
                summary: Some("Freshness boost".to_owned()),
                stage_score: 0.15,
                stage_weight: 1.0,
                score_factors: vec![],
            },
        ],
        &config,
    );

    let report = compose_explain_report(SearchMode::Hybrid, 100, timings, vec![hit], &config);

    let json = serde_json::to_string(&report).unwrap();
    let restored: ExplainReport = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.mode_used, SearchMode::Hybrid);
    assert_eq!(restored.candidates_evaluated, 100);
    assert_eq!(restored.taxonomy_version, 1);
    assert_eq!(restored.hits.len(), 1);
    assert_eq!(restored.hits[0].doc_id, 42);
}

// ── Section 5: Edge cases and robustness ────────────────────────────

#[test]
fn hint_with_duplicate_fields_preserved() {
    // Two "from" hints — both should be extracted
    let qa = parse_query_assistance("from:Alice from:Bob body");
    assert_eq!(qa.applied_filter_hints.len(), 2);
    assert_eq!(qa.applied_filter_hints[0].value, "Alice");
    assert_eq!(qa.applied_filter_hints[1].value, "Bob");
}

#[test]
fn unicode_in_hint_values() {
    let qa = parse_query_assistance("from:Müller project:日本語 search");
    assert_eq!(qa.applied_filter_hints.len(), 2);
    assert_eq!(qa.applied_filter_hints[0].value, "Müller");
    assert_eq!(qa.applied_filter_hints[1].value, "日本語");
    assert_eq!(qa.query_text, "search");
}

#[test]
fn reason_code_ordering_total() {
    // Verify all 10 codes have a total ordering
    let all_codes = [
        ExplainReasonCode::LexicalBm25,
        ExplainReasonCode::LexicalTermCoverage,
        ExplainReasonCode::SemanticCosine,
        ExplainReasonCode::SemanticNeighborhood,
        ExplainReasonCode::FusionWeightedBlend,
        ExplainReasonCode::RerankPolicyBoost,
        ExplainReasonCode::RerankPolicyPenalty,
        ExplainReasonCode::StageNotExecuted,
        ExplainReasonCode::ScopeRedacted,
        ExplainReasonCode::ScopeDenied,
    ];
    // Every pair should have a defined ordering
    for i in 0..all_codes.len() {
        for j in (i + 1)..all_codes.len() {
            assert!(
                all_codes[i] < all_codes[j],
                "{:?} should be < {:?}",
                all_codes[i],
                all_codes[j]
            );
        }
    }
}

#[test]
fn empty_report_serializes_compactly() {
    let config = ExplainComposerConfig::default();
    let report = compose_explain_report(SearchMode::Lexical, 0, HashMap::new(), vec![], &config);
    let json = serde_json::to_string(&report).unwrap();
    // Empty report should be well under 1KB
    assert!(
        json.len() < 1024,
        "empty report too large: {} bytes",
        json.len()
    );
}

#[test]
fn factors_sorted_by_abs_contribution_in_composed_hit() {
    let config = ExplainComposerConfig {
        verbosity: ExplainVerbosity::Detailed,
        max_factors_per_stage: 10,
    };
    let hit = compose_hit_explanation(
        1,
        0.5,
        vec![StageScoreInput {
            stage: ExplainStage::Lexical,
            reason_code: ExplainReasonCode::LexicalBm25,
            summary: None,
            stage_score: 0.5,
            stage_weight: 1.0,
            score_factors: vec![
                ScoreFactor {
                    code: ExplainReasonCode::LexicalBm25,
                    key: "small".to_owned(),
                    contribution: 0.1,
                    detail: None,
                },
                ScoreFactor {
                    code: ExplainReasonCode::LexicalTermCoverage,
                    key: "large_neg".to_owned(),
                    contribution: -0.8,
                    detail: None,
                },
                ScoreFactor {
                    code: ExplainReasonCode::LexicalBm25,
                    key: "medium".to_owned(),
                    contribution: 0.5,
                    detail: None,
                },
            ],
        }],
        &config,
    );
    let factors = &hit.stages[0].score_factors;
    assert_eq!(factors.len(), 3);
    // Sorted by |contribution| descending
    assert!(
        factors[0].contribution.abs() >= factors[1].contribution.abs(),
        "factors not sorted: {} vs {}",
        factors[0].contribution.abs(),
        factors[1].contribution.abs()
    );
    assert!(
        factors[1].contribution.abs() >= factors[2].contribution.abs(),
        "factors not sorted: {} vs {}",
        factors[1].contribution.abs(),
        factors[2].contribution.abs()
    );
}
