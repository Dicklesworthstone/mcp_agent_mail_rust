//! Search results model — re-exported from `mcp-agent-mail-core`.

pub use mcp_agent_mail_core::search_types::{
    ExplainComposerConfig, ExplainReasonCode, ExplainReport, ExplainStage, ExplainVerbosity,
    HighlightRange, HitExplanation, ScoreFactor, SearchHit, SearchResults, StageExplanation,
    StageScoreInput, compose_explain_report, compose_hit_explanation, factor_sort_cmp,
    missing_stage, redact_hit_explanation, redact_report_for_docs,
};

// Re-export referenced types from sibling modules
pub use mcp_agent_mail_core::search_types::{DocId, DocKind, SearchMode};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};
    use std::time::Duration;

    fn sample_hit() -> SearchHit {
        SearchHit {
            doc_id: 42,
            doc_kind: DocKind::Message,
            score: 0.95,
            snippet: Some("...matched **term**...".to_owned()),
            highlight_ranges: vec![HighlightRange {
                field: "body".to_owned(),
                start: 11,
                end: 19,
            }],
            metadata: {
                let mut m = HashMap::new();
                m.insert("sender".to_owned(), serde_json::json!("BlueLake"));
                m
            },
        }
    }

    fn sample_explain_hit(config: &ExplainComposerConfig) -> HitExplanation {
        compose_hit_explanation(
            42,
            0.95,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: Some("BM25 dominant".to_owned()),
                stage_score: 0.95,
                stage_weight: 1.0,
                score_factors: vec![
                    ScoreFactor {
                        code: ExplainReasonCode::LexicalBm25,
                        key: "bm25".to_owned(),
                        contribution: 0.90,
                        detail: Some("raw_bm25=12.5000".to_owned()),
                    },
                    ScoreFactor {
                        code: ExplainReasonCode::LexicalTermCoverage,
                        key: "term_coverage".to_owned(),
                        contribution: 0.05,
                        detail: Some("matched=2/2".to_owned()),
                    },
                ],
            }],
            config,
        )
    }

    #[test]
    fn search_results_empty() {
        let results = SearchResults::empty(SearchMode::Auto, Duration::from_millis(1));
        assert!(results.is_empty());
        assert_eq!(results.total_count, 0);
        assert_eq!(results.mode_used, SearchMode::Auto);
        assert!(results.explain.is_none());
    }

    #[test]
    fn search_results_with_hits() {
        let results = SearchResults {
            hits: vec![sample_hit()],
            total_count: 1,
            mode_used: SearchMode::Lexical,
            explain: None,
            elapsed: Duration::from_millis(5),
        };
        assert!(!results.is_empty());
        assert_eq!(results.hits[0].doc_id, 42);
        assert!((results.hits[0].score - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn search_hit_serde_roundtrip() {
        let hit = sample_hit();
        let json = serde_json::to_string(&hit).unwrap();
        let hit2: SearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(hit2.doc_id, hit.doc_id);
        assert_eq!(hit2.doc_kind, hit.doc_kind);
        assert!((hit2.score - hit.score).abs() < f64::EPSILON);
        assert_eq!(hit2.snippet, hit.snippet);
        assert_eq!(hit2.highlight_ranges.len(), 1);
        assert_eq!(hit2.highlight_ranges[0].field, "body");
        assert_eq!(hit2.highlight_ranges[0].start, 11);
        assert_eq!(hit2.highlight_ranges[0].end, 19);
    }

    #[test]
    fn search_results_serde_roundtrip() {
        let results = SearchResults {
            hits: vec![sample_hit()],
            total_count: 100,
            mode_used: SearchMode::Hybrid,
            explain: None,
            elapsed: Duration::from_millis(42),
        };
        let json = serde_json::to_string(&results).unwrap();
        let results2: SearchResults = serde_json::from_str(&json).unwrap();
        assert_eq!(results2.total_count, 100);
        assert_eq!(results2.mode_used, SearchMode::Hybrid);
        assert_eq!(results2.hits.len(), 1);
    }

    #[test]
    fn explain_report_serde_roundtrip() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 8,
        };
        let report = ExplainReport {
            hits: vec![sample_explain_hit(&config)],
            mode_used: SearchMode::Lexical,
            candidates_evaluated: 500,
            phase_timings: {
                let mut m = HashMap::new();
                m.insert("retrieval".to_owned(), Duration::from_millis(3));
                m.insert("scoring".to_owned(), Duration::from_millis(1));
                m
            },
            taxonomy_version: 1,
            stage_order: ExplainStage::canonical_order().to_vec(),
            verbosity: ExplainVerbosity::Detailed,
        };
        let json = serde_json::to_string(&report).unwrap();
        let report2: ExplainReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report2.hits.len(), 1);
        assert_eq!(report2.hits[0].doc_id, 42);
        assert_eq!(report2.candidates_evaluated, 500);
        assert_eq!(report2.mode_used, SearchMode::Lexical);
        assert_eq!(report2.taxonomy_version, 1);
        assert_eq!(
            report2.stage_order,
            ExplainStage::canonical_order().to_vec()
        );
    }

    #[test]
    fn explain_composer_emits_canonical_stage_shape() {
        let config = ExplainComposerConfig::default();
        let hit = sample_explain_hit(&config);
        assert_eq!(
            hit.stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
            ExplainStage::canonical_order().to_vec()
        );
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
    fn explain_composer_deterministic_factor_sort_and_truncation() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 2,
        };

        let factors_a = vec![
            ScoreFactor {
                code: ExplainReasonCode::LexicalTermCoverage,
                key: "zeta".to_owned(),
                contribution: 0.10,
                detail: Some("z".to_owned()),
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "alpha".to_owned(),
                contribution: 0.80,
                detail: Some("a".to_owned()),
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalTermCoverage,
                key: "beta".to_owned(),
                contribution: 0.10,
                detail: Some("b".to_owned()),
            },
        ];
        let mut factors_b = factors_a.clone();
        factors_b.reverse();

        let hit_a = compose_hit_explanation(
            7,
            0.80,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.80,
                stage_weight: 1.0,
                score_factors: factors_a,
            }],
            &config,
        );
        let hit_b = compose_hit_explanation(
            7,
            0.80,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.80,
                stage_weight: 1.0,
                score_factors: factors_b,
            }],
            &config,
        );

        assert_eq!(
            serde_json::to_value(&hit_a).unwrap(),
            serde_json::to_value(&hit_b).unwrap()
        );
        let lexical = &hit_a.stages[0];
        assert_eq!(lexical.score_factors.len(), 2);
        assert_eq!(lexical.truncated_factor_count, 1);
    }

    #[test]
    fn explain_composer_aggregates_duplicate_stage_inputs() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 8,
        };
        let hit = compose_hit_explanation(
            10,
            0.84,
            vec![
                StageScoreInput {
                    stage: ExplainStage::Lexical,
                    reason_code: ExplainReasonCode::LexicalBm25,
                    summary: None,
                    stage_score: 0.60,
                    stage_weight: 0.8,
                    score_factors: vec![ScoreFactor {
                        code: ExplainReasonCode::LexicalBm25,
                        key: "bm25".to_owned(),
                        contribution: 0.60,
                        detail: None,
                    }],
                },
                StageScoreInput {
                    stage: ExplainStage::Lexical,
                    reason_code: ExplainReasonCode::LexicalTermCoverage,
                    summary: None,
                    stage_score: 0.24,
                    stage_weight: 1.0,
                    score_factors: vec![ScoreFactor {
                        code: ExplainReasonCode::LexicalTermCoverage,
                        key: "term_coverage".to_owned(),
                        contribution: 0.24,
                        detail: None,
                    }],
                },
            ],
            &config,
        );
        let lexical = &hit.stages[0];
        assert!((lexical.stage_score - 0.84).abs() < f64::EPSILON);
        assert!((lexical.stage_weight - 1.0).abs() < f64::EPSILON);
        assert!((lexical.weighted_score - 0.84).abs() < f64::EPSILON);
        assert_eq!(lexical.score_factors.len(), 2);
    }

    #[test]
    fn explain_verbosity_minimal_hides_factor_details() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Minimal,
            max_factors_per_stage: 4,
        };
        let hit = sample_explain_hit(&config);
        assert!(hit.stages[0].score_factors.is_empty());
        assert_eq!(hit.stages[0].truncated_factor_count, 2);
    }

    #[test]
    fn redact_report_for_docs_scrubs_stage_details() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 4,
        };
        let mut report = compose_explain_report(
            SearchMode::Lexical,
            1,
            HashMap::new(),
            vec![sample_explain_hit(&config)],
            &config,
        );
        let mut redacted_ids = BTreeSet::new();
        redacted_ids.insert(42);
        redact_report_for_docs(&mut report, &redacted_ids, ExplainReasonCode::ScopeRedacted);

        let hit = &report.hits[0];
        assert!((hit.final_score - 0.0).abs() < f64::EPSILON);
        assert_eq!(hit.reason_codes, vec![ExplainReasonCode::ScopeRedacted]);
        assert!(hit.stages.iter().all(|s| s.redacted));
        assert!(hit.stages.iter().all(|s| s.score_factors.is_empty()));
        assert!(
            hit.stages
                .iter()
                .all(|s| s.reason_code == ExplainReasonCode::ScopeRedacted)
        );
    }

    #[test]
    fn highlight_range_serde() {
        let range = HighlightRange {
            field: "title".to_owned(),
            start: 0,
            end: 5,
        };
        let json = serde_json::to_string(&range).unwrap();
        let range2: HighlightRange = serde_json::from_str(&json).unwrap();
        assert_eq!(range2.field, "title");
        assert_eq!(range2.start, 0);
        assert_eq!(range2.end, 5);
    }

    #[test]
    fn hit_metadata_empty_skipped_in_json() {
        let hit = SearchHit {
            doc_id: 1,
            doc_kind: DocKind::Agent,
            score: 0.5,
            snippet: None,
            highlight_ranges: Vec::new(),
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&hit).unwrap();
        assert!(!json.contains("metadata"));
        assert!(!json.contains("highlight_ranges"));
        assert!(!json.contains("snippet"));
    }

    #[test]
    fn explain_stage_canonical_order() {
        let order = ExplainStage::canonical_order();
        assert_eq!(order.len(), 4);
        assert_eq!(order[0], ExplainStage::Lexical);
        assert_eq!(order[1], ExplainStage::Semantic);
        assert_eq!(order[2], ExplainStage::Fusion);
        assert_eq!(order[3], ExplainStage::Rerank);
    }

    #[test]
    fn explain_stage_serde_roundtrip() {
        for stage in ExplainStage::canonical_order() {
            let json = serde_json::to_string(&stage).unwrap();
            let restored: ExplainStage = serde_json::from_str(&json).unwrap();
            assert_eq!(stage, restored);
        }
    }

    #[test]
    fn explain_stage_ordering() {
        assert!(ExplainStage::Lexical < ExplainStage::Semantic);
        assert!(ExplainStage::Semantic < ExplainStage::Fusion);
        assert!(ExplainStage::Fusion < ExplainStage::Rerank);
    }

    #[test]
    fn reason_code_summary_not_empty() {
        let codes = [
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
        for code in codes {
            let summary = code.summary();
            assert!(!summary.is_empty(), "summary empty for {code:?}");
        }
    }

    #[test]
    fn reason_code_serde_roundtrip() {
        let code = ExplainReasonCode::FusionWeightedBlend;
        let json = serde_json::to_string(&code).unwrap();
        let restored: ExplainReasonCode = serde_json::from_str(&json).unwrap();
        assert_eq!(code, restored);
        assert_eq!(json, "\"fusion_weighted_blend\"");
    }

    #[test]
    fn explain_verbosity_default_is_standard() {
        assert_eq!(ExplainVerbosity::default(), ExplainVerbosity::Standard);
    }

    #[test]
    fn explain_verbosity_serde_roundtrip() {
        for v in [
            ExplainVerbosity::Minimal,
            ExplainVerbosity::Standard,
            ExplainVerbosity::Detailed,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let restored: ExplainVerbosity = serde_json::from_str(&json).unwrap();
            assert_eq!(v, restored);
        }
    }

    #[test]
    fn composer_config_defaults() {
        let config = ExplainComposerConfig::default();
        assert_eq!(config.verbosity, ExplainVerbosity::Standard);
        assert_eq!(config.max_factors_per_stage, 4);
    }

    #[test]
    fn compose_hit_all_stages_missing() {
        let config = ExplainComposerConfig::default();
        let hit = compose_hit_explanation(1, 0.0, vec![], &config);

        assert_eq!(hit.doc_id, 1);
        assert!((hit.final_score).abs() < f64::EPSILON);
        assert_eq!(hit.stages.len(), 4);
        for stage in &hit.stages {
            assert_eq!(stage.reason_code, ExplainReasonCode::StageNotExecuted);
            assert!((stage.stage_score).abs() < f64::EPSILON);
            assert!((stage.weighted_score).abs() < f64::EPSILON);
        }
        assert_eq!(hit.reason_codes.len(), 1);
        assert_eq!(hit.reason_codes[0], ExplainReasonCode::StageNotExecuted);
    }

    #[test]
    fn compose_hit_multiple_stages() {
        let config = ExplainComposerConfig::default();
        let hit = compose_hit_explanation(
            5,
            0.75,
            vec![
                StageScoreInput {
                    stage: ExplainStage::Lexical,
                    reason_code: ExplainReasonCode::LexicalBm25,
                    summary: None,
                    stage_score: 0.50,
                    stage_weight: 0.6,
                    score_factors: vec![],
                },
                StageScoreInput {
                    stage: ExplainStage::Semantic,
                    reason_code: ExplainReasonCode::SemanticCosine,
                    summary: None,
                    stage_score: 0.80,
                    stage_weight: 0.4,
                    score_factors: vec![],
                },
            ],
            &config,
        );

        assert_eq!(hit.stages.len(), 4);
        assert_eq!(hit.stages[0].reason_code, ExplainReasonCode::LexicalBm25);
        assert_eq!(hit.stages[1].reason_code, ExplainReasonCode::SemanticCosine);
        assert_eq!(
            hit.stages[2].reason_code,
            ExplainReasonCode::StageNotExecuted
        );
        assert_eq!(
            hit.stages[3].reason_code,
            ExplainReasonCode::StageNotExecuted
        );
        assert!((hit.stages[0].weighted_score - 0.30).abs() < f64::EPSILON);
        assert!((hit.stages[1].weighted_score - 0.32).abs() < f64::EPSILON);
    }

    #[test]
    fn compose_hit_reason_codes_deduped() {
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

        assert!(hit.reason_codes.len() <= 4);
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
    fn compose_stage_standard_verbosity_strips_detail() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Standard,
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
                score_factors: vec![ScoreFactor {
                    code: ExplainReasonCode::LexicalBm25,
                    key: "bm25".to_owned(),
                    contribution: 0.5,
                    detail: Some("should be stripped".to_owned()),
                }],
            }],
            &config,
        );
        assert_eq!(hit.stages[0].score_factors.len(), 1);
        assert!(hit.stages[0].score_factors[0].detail.is_none());
    }

    #[test]
    fn compose_stage_detailed_verbosity_keeps_detail() {
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
                score_factors: vec![ScoreFactor {
                    code: ExplainReasonCode::LexicalBm25,
                    key: "bm25".to_owned(),
                    contribution: 0.5,
                    detail: Some("raw_bm25=12.5".to_owned()),
                }],
            }],
            &config,
        );
        assert_eq!(
            hit.stages[0].score_factors[0].detail.as_deref(),
            Some("raw_bm25=12.5")
        );
    }

    #[test]
    fn compose_stage_zero_max_factors_hides_all() {
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
                        key: "bm25".to_owned(),
                        contribution: 0.5,
                        detail: None,
                    },
                    ScoreFactor {
                        code: ExplainReasonCode::LexicalTermCoverage,
                        key: "tc".to_owned(),
                        contribution: 0.1,
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
    fn compose_stage_custom_summary_used() {
        let config = ExplainComposerConfig::default();
        let hit = compose_hit_explanation(
            1,
            0.5,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: Some("Custom summary".to_owned()),
                stage_score: 0.5,
                stage_weight: 1.0,
                score_factors: vec![],
            }],
            &config,
        );
        assert_eq!(hit.stages[0].summary, "Custom summary");
    }

    #[test]
    fn compose_stage_default_summary_from_reason_code() {
        let config = ExplainComposerConfig::default();
        let hit = compose_hit_explanation(
            1,
            0.5,
            vec![StageScoreInput {
                stage: ExplainStage::Semantic,
                reason_code: ExplainReasonCode::SemanticCosine,
                summary: None,
                stage_score: 0.5,
                stage_weight: 1.0,
                score_factors: vec![],
            }],
            &config,
        );
        assert_eq!(
            hit.stages[1].summary,
            ExplainReasonCode::SemanticCosine.summary()
        );
    }

    #[test]
    fn factor_sort_by_abs_contribution_desc() {
        let mut factors = [
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "a".to_owned(),
                contribution: 0.1,
                detail: None,
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "b".to_owned(),
                contribution: -0.5,
                detail: None,
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "c".to_owned(),
                contribution: 0.3,
                detail: None,
            },
        ];
        factors.sort_by(factor_sort_cmp);
        assert_eq!(factors[0].key, "b");
        assert_eq!(factors[1].key, "c");
        assert_eq!(factors[2].key, "a");
    }

    #[test]
    fn factor_sort_tiebreak_by_code_then_key() {
        let mut factors = [
            ScoreFactor {
                code: ExplainReasonCode::LexicalTermCoverage,
                key: "zeta".to_owned(),
                contribution: 0.5,
                detail: None,
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "alpha".to_owned(),
                contribution: 0.5,
                detail: None,
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "beta".to_owned(),
                contribution: 0.5,
                detail: None,
            },
        ];
        factors.sort_by(factor_sort_cmp);
        assert_eq!(factors[0].code, ExplainReasonCode::LexicalBm25);
        assert_eq!(factors[0].key, "alpha");
        assert_eq!(factors[1].code, ExplainReasonCode::LexicalBm25);
        assert_eq!(factors[1].key, "beta");
        assert_eq!(factors[2].code, ExplainReasonCode::LexicalTermCoverage);
        assert_eq!(factors[2].key, "zeta");
    }

    #[test]
    fn redact_hit_explanation_directly() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 10,
        };
        let mut hit = sample_explain_hit(&config);
        assert!(hit.final_score > 0.0);

        redact_hit_explanation(&mut hit, ExplainReasonCode::ScopeDenied);

        assert!((hit.final_score).abs() < f64::EPSILON);
        assert_eq!(hit.reason_codes, vec![ExplainReasonCode::ScopeDenied]);
        for stage in &hit.stages {
            assert!(stage.redacted);
            assert_eq!(stage.reason_code, ExplainReasonCode::ScopeDenied);
            assert!(stage.score_factors.is_empty());
            assert!((stage.stage_score).abs() < f64::EPSILON);
            assert!((stage.stage_weight).abs() < f64::EPSILON);
            assert!((stage.weighted_score).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn redact_report_non_matching_docs_preserved() {
        let config = ExplainComposerConfig::default();
        let hit42 = compose_hit_explanation(
            42,
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
        let hit99 = compose_hit_explanation(
            99,
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

        let mut report = compose_explain_report(
            SearchMode::Lexical,
            2,
            HashMap::new(),
            vec![hit42, hit99],
            &config,
        );

        let mut redact_ids = BTreeSet::new();
        redact_ids.insert(42);
        redact_report_for_docs(&mut report, &redact_ids, ExplainReasonCode::ScopeRedacted);

        assert!((report.hits[0].final_score).abs() < f64::EPSILON);
        assert!(report.hits[0].stages.iter().all(|s| s.redacted));

        assert!((report.hits[1].final_score - 0.5).abs() < f64::EPSILON);
        assert!(!report.hits[1].stages[0].redacted);
    }

    #[test]
    fn compose_report_metadata() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Minimal,
            max_factors_per_stage: 2,
        };
        let mut timings = HashMap::new();
        timings.insert("retrieval".to_owned(), Duration::from_millis(5));

        let report = compose_explain_report(SearchMode::Hybrid, 250, timings, vec![], &config);

        assert_eq!(report.mode_used, SearchMode::Hybrid);
        assert_eq!(report.candidates_evaluated, 250);
        assert_eq!(report.taxonomy_version, 1);
        assert_eq!(report.stage_order, ExplainStage::canonical_order().to_vec());
        assert_eq!(report.verbosity, ExplainVerbosity::Minimal);
        assert!(report.hits.is_empty());
        assert_eq!(
            report.phase_timings.get("retrieval"),
            Some(&Duration::from_millis(5))
        );
    }

    #[test]
    fn score_factor_serde_roundtrip() {
        let factor = ScoreFactor {
            code: ExplainReasonCode::SemanticCosine,
            key: "cosine".to_owned(),
            contribution: 0.72,
            detail: Some("raw=0.72".to_owned()),
        };
        let json = serde_json::to_string(&factor).unwrap();
        let restored: ScoreFactor = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.code, ExplainReasonCode::SemanticCosine);
        assert_eq!(restored.key, "cosine");
        assert!((restored.contribution - 0.72).abs() < f64::EPSILON);
        assert_eq!(restored.detail.as_deref(), Some("raw=0.72"));
    }

    #[test]
    fn score_factor_no_detail_skipped() {
        let factor = ScoreFactor {
            code: ExplainReasonCode::LexicalBm25,
            key: "bm25".to_owned(),
            contribution: 0.5,
            detail: None,
        };
        let json = serde_json::to_string(&factor).unwrap();
        assert!(!json.contains("detail"));
    }

    #[test]
    fn hit_explanation_serde_roundtrip() {
        let config = ExplainComposerConfig::default();
        let hit = sample_explain_hit(&config);
        let json = serde_json::to_string(&hit).unwrap();
        let restored: HitExplanation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.doc_id, 42);
        assert!((restored.final_score - 0.95).abs() < f64::EPSILON);
        assert_eq!(restored.stages.len(), 4);
    }

    #[test]
    fn stage_explanation_serde_roundtrip() {
        let stage = StageExplanation {
            stage: ExplainStage::Fusion,
            reason_code: ExplainReasonCode::FusionWeightedBlend,
            summary: "Weighted blend".to_owned(),
            stage_score: 0.7,
            stage_weight: 0.5,
            weighted_score: 0.35,
            score_factors: vec![],
            truncated_factor_count: 0,
            redacted: false,
        };
        let json = serde_json::to_string(&stage).unwrap();
        let restored: StageExplanation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.stage, ExplainStage::Fusion);
        assert!((restored.weighted_score - 0.35).abs() < f64::EPSILON);
        assert!(!restored.redacted);
    }

    #[test]
    fn search_results_empty_with_mode_variants() {
        for mode in [
            SearchMode::Lexical,
            SearchMode::Semantic,
            SearchMode::Hybrid,
            SearchMode::Auto,
        ] {
            let results = SearchResults::empty(mode, Duration::ZERO);
            assert!(results.is_empty());
            assert_eq!(results.mode_used, mode);
        }
    }

    #[test]
    fn search_hit_no_snippet_no_highlights() {
        let hit = SearchHit {
            doc_id: 7,
            doc_kind: DocKind::Project,
            score: 1.0,
            snippet: None,
            highlight_ranges: Vec::new(),
            metadata: HashMap::new(),
        };
        assert_eq!(hit.doc_id, 7);
        assert_eq!(hit.doc_kind, DocKind::Project);
        assert!(hit.snippet.is_none());
        assert!(hit.highlight_ranges.is_empty());
    }

    #[test]
    fn search_hit_with_multiple_highlights() {
        let hit = SearchHit {
            doc_id: 1,
            doc_kind: DocKind::Message,
            score: 0.5,
            snippet: Some("text".to_owned()),
            highlight_ranges: vec![
                HighlightRange {
                    field: "title".to_owned(),
                    start: 0,
                    end: 4,
                },
                HighlightRange {
                    field: "body".to_owned(),
                    start: 10,
                    end: 20,
                },
            ],
            metadata: HashMap::new(),
        };
        assert_eq!(hit.highlight_ranges.len(), 2);
        let json = serde_json::to_string(&hit).unwrap();
        let restored: SearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.highlight_ranges.len(), 2);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn highlight_range_debug_clone() {
        let range = HighlightRange {
            field: "body".to_owned(),
            start: 10,
            end: 20,
        };
        let debug = format!("{range:?}");
        assert!(debug.contains("body"));
        let cloned = range.clone();
        assert_eq!(cloned.start, 10);
        assert_eq!(cloned.end, 20);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn search_hit_debug_clone() {
        let hit = sample_hit();
        let debug = format!("{hit:?}");
        assert!(debug.contains("42"));
        let cloned = hit.clone();
        assert_eq!(cloned.doc_id, 42);
        assert_eq!(cloned.doc_kind, DocKind::Message);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn explain_report_debug_clone() {
        let config = ExplainComposerConfig::default();
        let report = compose_explain_report(SearchMode::Auto, 10, HashMap::new(), vec![], &config);
        let debug = format!("{report:?}");
        assert!(debug.contains("ExplainReport"));
        let cloned = report.clone();
        assert_eq!(cloned.candidates_evaluated, 10);
        assert_eq!(cloned.mode_used, SearchMode::Auto);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn search_results_debug_clone() {
        let results = SearchResults {
            hits: vec![sample_hit()],
            total_count: 1,
            mode_used: SearchMode::Lexical,
            explain: None,
            elapsed: Duration::from_millis(5),
        };
        let debug = format!("{results:?}");
        assert!(debug.contains("SearchResults"));
        let cloned = results.clone();
        assert_eq!(cloned.total_count, 1);
        assert_eq!(cloned.hits.len(), 1);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn stage_score_input_debug_clone() {
        let input = StageScoreInput {
            stage: ExplainStage::Fusion,
            reason_code: ExplainReasonCode::FusionWeightedBlend,
            summary: Some("blend".to_owned()),
            stage_score: 0.7,
            stage_weight: 0.5,
            score_factors: vec![],
        };
        let debug = format!("{input:?}");
        assert!(debug.contains("Fusion"));
        let cloned = input.clone();
        assert!((cloned.stage_score - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn explain_composer_config_debug_clone() {
        let config = ExplainComposerConfig::default();
        let debug = format!("{config:?}");
        assert!(debug.contains("verbosity"));
        let cloned = config.clone();
        assert_eq!(cloned.max_factors_per_stage, 4);
    }

    #[test]
    fn search_results_with_explain_serde() {
        let config = ExplainComposerConfig::default();
        let report = compose_explain_report(
            SearchMode::Hybrid,
            50,
            HashMap::new(),
            vec![sample_explain_hit(&config)],
            &config,
        );
        let results = SearchResults {
            hits: vec![sample_hit()],
            total_count: 1,
            mode_used: SearchMode::Hybrid,
            explain: Some(report),
            elapsed: Duration::from_millis(10),
        };
        let json = serde_json::to_string(&results).unwrap();
        assert!(json.contains("explain"));
        let restored: SearchResults = serde_json::from_str(&json).unwrap();
        assert!(restored.explain.is_some());
        let explain = restored.explain.unwrap();
        assert_eq!(explain.hits.len(), 1);
        assert_eq!(explain.candidates_evaluated, 50);
    }

    #[test]
    fn explain_reason_code_ordering() {
        assert!(ExplainReasonCode::LexicalBm25 < ExplainReasonCode::LexicalTermCoverage);
        assert!(ExplainReasonCode::SemanticCosine < ExplainReasonCode::FusionWeightedBlend);
        assert!(ExplainReasonCode::ScopeRedacted < ExplainReasonCode::ScopeDenied);
    }

    #[test]
    fn explain_stage_hash_works() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ExplainStage::Lexical);
        set.insert(ExplainStage::Semantic);
        set.insert(ExplainStage::Fusion);
        set.insert(ExplainStage::Rerank);
        set.insert(ExplainStage::Lexical);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn explain_reason_code_hash_works() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ExplainReasonCode::LexicalBm25);
        set.insert(ExplainReasonCode::SemanticCosine);
        set.insert(ExplainReasonCode::LexicalBm25);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn search_hit_thread_doc_kind() {
        let hit = SearchHit {
            doc_id: 1,
            doc_kind: DocKind::Thread,
            score: 0.5,
            snippet: None,
            highlight_ranges: Vec::new(),
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&hit).unwrap();
        let restored: SearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.doc_kind, DocKind::Thread);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn score_factor_debug_clone() {
        let factor = ScoreFactor {
            code: ExplainReasonCode::RerankPolicyPenalty,
            key: "penalty".to_owned(),
            contribution: -0.3,
            detail: None,
        };
        let debug = format!("{factor:?}");
        assert!(debug.contains("RerankPolicyPenalty"));
        let cloned = factor.clone();
        assert!((cloned.contribution - (-0.3)).abs() < f64::EPSILON);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn hit_explanation_debug_clone() {
        let config = ExplainComposerConfig::default();
        let hit = sample_explain_hit(&config);
        let debug = format!("{hit:?}");
        assert!(debug.contains("42"));
        let cloned = hit.clone();
        assert_eq!(cloned.doc_id, 42);
        assert_eq!(cloned.stages.len(), 4);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn stage_explanation_debug_clone() {
        let stage = missing_stage(ExplainStage::Rerank);
        let debug = format!("{stage:?}");
        assert!(debug.contains("Rerank"));
        let cloned = stage.clone();
        assert_eq!(cloned.reason_code, ExplainReasonCode::StageNotExecuted);
        assert!(!cloned.redacted);
    }

    #[test]
    fn factor_sort_cmp_nan_handling() {
        let a = ScoreFactor {
            code: ExplainReasonCode::LexicalBm25,
            key: "a".to_owned(),
            contribution: f64::NAN,
            detail: None,
        };
        let b = ScoreFactor {
            code: ExplainReasonCode::LexicalBm25,
            key: "b".to_owned(),
            contribution: 0.5,
            detail: None,
        };
        let _ = factor_sort_cmp(&a, &b);
        let _ = factor_sort_cmp(&b, &a);
    }
}
