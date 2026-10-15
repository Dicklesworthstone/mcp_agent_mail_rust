use super::*;

#[test]
fn query_classifies_identifier() {
    assert_eq!(QueryClass::classify("br-2tnl.5.1"), QueryClass::Identifier);
    assert_eq!(
        QueryClass::classify("thread:abc-123"),
        QueryClass::Identifier
    );
}

#[test]
fn query_classifies_short_keyword_and_natural_language() {
    assert_eq!(
        QueryClass::classify("search regression"),
        QueryClass::ShortKeyword
    );
    assert_eq!(
        QueryClass::classify("how do we tune hybrid candidate retrieval quality"),
        QueryClass::NaturalLanguage
    );
}

#[test]
fn budget_is_mode_aware() {
    let config = CandidateBudgetConfig::default();
    let hybrid =
        CandidateBudget::derive(20, CandidateMode::Hybrid, QueryClass::ShortKeyword, config);
    let auto =
        CandidateBudget::derive(20, CandidateMode::Auto, QueryClass::ShortKeyword, config);
    let fallback = CandidateBudget::derive(
        20,
        CandidateMode::LexicalFallback,
        QueryClass::ShortKeyword,
        config,
    );

    assert!(hybrid.semantic_limit > auto.semantic_limit);
    assert!(fallback.lexical_limit >= auto.lexical_limit);
    assert_eq!(fallback.semantic_limit, 0);
}

#[test]
fn derive_with_decision_keeps_derive_isomorphic() {
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
            let direct = CandidateBudget::derive(64, mode, class, config);
            let explained = CandidateBudget::derive_with_decision(64, mode, class, config);
            assert_eq!(direct, explained.budget);
        }
    }
}

#[test]
fn derive_with_decision_posterior_is_normalized() {
    let config = CandidateBudgetConfig::default();
    let derivation = CandidateBudget::derive_with_decision(
        50,
        CandidateMode::Hybrid,
        QueryClass::NaturalLanguage,
        config,
    );
    let posterior = derivation.decision.posterior;
    let sum = posterior.identifier
        + posterior.short_keyword
        + posterior.natural_language
        + posterior.empty;
    assert!(
        (sum - 1.0).abs() < 1e-9,
        "posterior should sum to 1.0, got {sum}"
    );
}

#[test]
fn derive_with_decision_reports_minimum_expected_loss() {
    let config = CandidateBudgetConfig::default();
    let derivation = CandidateBudget::derive_with_decision(
        100,
        CandidateMode::Auto,
        QueryClass::ShortKeyword,
        config,
    );
    let min_loss = derivation
        .decision
        .action_losses
        .iter()
        .map(|entry| entry.expected_loss)
        .fold(f64::INFINITY, f64::min);
    assert!(
        (derivation.decision.chosen_expected_loss - min_loss).abs() < 1e-9,
        "chosen expected loss should match minimum candidate loss"
    );
}

#[test]
fn prepare_candidates_dedupes_and_keeps_deterministic_order() {
    let lexical = vec![
        CandidateHit::new(10, 0.9),
        CandidateHit::new(20, 0.8),
        CandidateHit::new(30, 0.7),
    ];
    let semantic = vec![
        CandidateHit::new(20, 0.99),
        CandidateHit::new(40, 0.75),
        CandidateHit::new(30, 0.6),
    ];
    let budget = CandidateBudget {
        lexical_limit: 3,
        semantic_limit: 3,
        combined_limit: 10,
    };

    let prepared = prepare_candidates(&lexical, &semantic, budget);
    let doc_ids = prepared
        .candidates
        .iter()
        .map(|candidate| candidate.doc_id)
        .collect::<Vec<_>>();

    assert_eq!(doc_ids, vec![10, 20, 40, 30]);
    assert_eq!(prepared.counts.lexical_selected, 3);
    assert_eq!(prepared.counts.semantic_selected, 3);
    assert_eq!(prepared.counts.deduped_selected, 4);
    assert_eq!(prepared.counts.duplicates_removed, 2);
}

#[test]
fn prepare_candidates_respects_budget_trimming() {
    let lexical = (1..=10)
        .map(|id| CandidateHit::new(id, 1.0))
        .collect::<Vec<_>>();
    let semantic = (5..=14)
        .map(|id| CandidateHit::new(id, 0.5))
        .collect::<Vec<_>>();
    let budget = CandidateBudget {
        lexical_limit: 2,
        semantic_limit: 2,
        combined_limit: 2,
    };

    let prepared = prepare_candidates(&lexical, &semantic, budget);
    assert_eq!(prepared.counts.lexical_selected, 2);
    assert_eq!(prepared.counts.semantic_selected, 2);
    assert_eq!(prepared.candidates.len(), 2);
}

#[test]
fn deterministic_tie_break_uses_doc_id_last() {
    let lexical = vec![CandidateHit::new(2, 1.0), CandidateHit::new(1, 1.0)];
    let semantic = Vec::new();
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 0,
        combined_limit: 10,
    };
    let prepared = prepare_candidates(&lexical, &semantic, budget);
    let ids = prepared
        .candidates
        .iter()
        .map(|candidate| candidate.doc_id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![2, 1]);
}

// ── QueryClass classification edge cases ──────────────────────

#[test]
fn query_classifies_empty_and_whitespace() {
    assert_eq!(QueryClass::classify(""), QueryClass::Empty);
    assert_eq!(QueryClass::classify("   "), QueryClass::Empty);
    assert_eq!(QueryClass::classify("\t\n"), QueryClass::Empty);
}

#[test]
fn query_classifies_underscore_as_identifier() {
    assert_eq!(QueryClass::classify("my_variable"), QueryClass::Identifier);
}

#[test]
fn query_classifies_slash_path_as_identifier() {
    assert_eq!(QueryClass::classify("src/lib.rs"), QueryClass::Identifier);
}

#[test]
fn query_classifies_mixed_alphanumeric_token_as_identifier() {
    assert_eq!(QueryClass::classify("v3beta"), QueryClass::Identifier);
    assert_eq!(QueryClass::classify("abc123"), QueryClass::Identifier);
}

#[test]
fn query_classifies_single_word_as_short_keyword() {
    assert_eq!(QueryClass::classify("hello"), QueryClass::ShortKeyword);
}

#[test]
fn query_classifies_long_sentence_as_natural_language() {
    assert_eq!(
        QueryClass::classify("find all messages about database migration and rollback"),
        QueryClass::NaturalLanguage
    );
}

#[test]
fn query_classify_serde_roundtrip() {
    for class in [
        QueryClass::Identifier,
        QueryClass::ShortKeyword,
        QueryClass::NaturalLanguage,
        QueryClass::Empty,
    ] {
        let json = serde_json::to_string(&class).unwrap();
        let class2: QueryClass = serde_json::from_str(&json).unwrap();
        assert_eq!(class, class2);
    }
}

// ── CandidateMode serde ───────────────────────────────────────

#[test]
fn candidate_mode_serde_roundtrip() {
    for mode in [
        CandidateMode::Hybrid,
        CandidateMode::Auto,
        CandidateMode::LexicalFallback,
    ] {
        let json = serde_json::to_string(&mode).unwrap();
        let mode2: CandidateMode = serde_json::from_str(&json).unwrap();
        assert_eq!(mode, mode2);
    }
}

#[test]
fn candidate_source_serde_roundtrip() {
    for src in [CandidateSource::Lexical, CandidateSource::Semantic] {
        let json = serde_json::to_string(&src).unwrap();
        let src2: CandidateSource = serde_json::from_str(&json).unwrap();
        assert_eq!(src, src2);
    }
}

// ── CandidateBudgetConfig defaults ────────────────────────────

#[test]
fn budget_config_defaults_are_reasonable() {
    let config = CandidateBudgetConfig::default();
    assert!(config.min_lexical > 0, "min_lexical should be > 0");
    assert!(config.min_semantic > 0, "min_semantic should be > 0");
    assert!(
        config.max_lexical >= config.min_lexical,
        "max_lexical >= min_lexical"
    );
    assert!(
        config.max_semantic >= config.min_semantic,
        "max_semantic >= min_semantic"
    );
    assert!(
        config.max_combined >= config.max_lexical,
        "max_combined should accommodate lexical"
    );
}

// ── CandidateBudget::derive — comprehensive ───────────────────

#[test]
fn budget_lexical_fallback_produces_zero_semantic() {
    let config = CandidateBudgetConfig::default();
    for class in [
        QueryClass::Identifier,
        QueryClass::ShortKeyword,
        QueryClass::NaturalLanguage,
        QueryClass::Empty,
    ] {
        let budget = CandidateBudget::derive(50, CandidateMode::LexicalFallback, class, config);
        assert_eq!(
            budget.semantic_limit, 0,
            "LexicalFallback should always yield semantic_limit=0 for {class:?}"
        );
    }
}

#[test]
fn budget_empty_query_produces_zero_semantic() {
    let config = CandidateBudgetConfig::default();
    for mode in [
        CandidateMode::Hybrid,
        CandidateMode::Auto,
        CandidateMode::LexicalFallback,
    ] {
        let budget = CandidateBudget::derive(50, mode, QueryClass::Empty, config);
        assert_eq!(
            budget.semantic_limit, 0,
            "Empty query should yield semantic_limit=0 in {mode:?}"
        );
    }
}

#[test]
fn budget_clamps_requested_limit_to_minimum_one() {
    let config = CandidateBudgetConfig::default();
    let budget =
        CandidateBudget::derive(0, CandidateMode::Hybrid, QueryClass::ShortKeyword, config);
    // requested_limit is clamped to 1, so lexical/semantic should be at least min values
    assert!(budget.lexical_limit >= config.min_lexical);
}

#[test]
fn budget_clamps_requested_limit_to_maximum_thousand() {
    let config = CandidateBudgetConfig::default();
    let budget = CandidateBudget::derive(
        10_000,
        CandidateMode::Hybrid,
        QueryClass::ShortKeyword,
        config,
    );
    assert!(budget.lexical_limit <= config.max_lexical);
    assert!(budget.semantic_limit <= config.max_semantic);
    assert!(budget.combined_limit <= config.max_combined);
}

#[test]
fn budget_hybrid_natural_language_favors_semantic() {
    let config = CandidateBudgetConfig::default();
    let budget = CandidateBudget::derive(
        100,
        CandidateMode::Hybrid,
        QueryClass::NaturalLanguage,
        config,
    );
    // NL class multiplier for semantic (135) > lexical (90), and hybrid
    // base semantic (400) > lexical (300), so semantic should be larger.
    assert!(
        budget.semantic_limit > budget.lexical_limit,
        "NL hybrid should favor semantic: lex={}, sem={}",
        budget.lexical_limit,
        budget.semantic_limit
    );
}

#[test]
fn budget_hybrid_identifier_favors_lexical() {
    let config = CandidateBudgetConfig::default();
    let budget =
        CandidateBudget::derive(100, CandidateMode::Hybrid, QueryClass::Identifier, config);
    // Identifier class multiplier for lexical (150) > semantic (50), should tip balance.
    assert!(
        budget.lexical_limit > budget.semantic_limit,
        "Identifier hybrid should favor lexical: lex={}, sem={}",
        budget.lexical_limit,
        budget.semantic_limit
    );
}

#[test]
fn budget_combined_limit_is_at_least_requested_limit() {
    let config = CandidateBudgetConfig::default();
    for limit in [1, 10, 50, 100, 500] {
        let budget = CandidateBudget::derive(
            limit,
            CandidateMode::Hybrid,
            QueryClass::ShortKeyword,
            config,
        );
        assert!(
            budget.combined_limit >= limit,
            "combined_limit {} should be >= requested {}",
            budget.combined_limit,
            limit
        );
    }
}

// ── scaled_ceil_limit saturation ──────────────────────────────

#[test]
fn scaled_ceil_limit_handles_large_inputs_without_overflow() {
    // Use max multipliers to test saturation
    let result = scaled_ceil_limit(1_000, u32::MAX, u32::MAX, 100);
    // Should not panic and should be capped at usize::MAX or a large but finite value
    assert!(result > 0);
}

#[test]
fn scaled_ceil_limit_zero_requested_produces_zero() {
    assert_eq!(scaled_ceil_limit(0, 300, 150, 100), 0);
}

#[test]
fn scaled_ceil_limit_zero_multiplier_produces_zero() {
    assert_eq!(scaled_ceil_limit(50, 0, 150, 100), 0);
    assert_eq!(scaled_ceil_limit(50, 300, 0, 100), 0);
}

#[test]
fn scaled_ceil_limit_rounds_up() {
    // 1 * 300 * 150 / (100 * 100) = 45000 / 10000 = 4.5 → rounds up to 5
    assert_eq!(scaled_ceil_limit(1, 300, 150, 100), 5);
}

// ── prepare_candidates — edge cases ───────────────────────────

#[test]
fn prepare_candidates_both_empty() {
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 20,
    };
    let prepared = prepare_candidates(&[], &[], budget);
    assert_eq!(prepared.candidates, [] as [PreparedCandidate; 0]);
    assert_eq!(prepared.counts.lexical_considered, 0);
    assert_eq!(prepared.counts.semantic_considered, 0);
    assert_eq!(prepared.counts.deduped_selected, 0);
    assert_eq!(prepared.counts.duplicates_removed, 0);
}

#[test]
fn prepare_candidates_lexical_only() {
    let lexical = vec![CandidateHit::new(1, 0.9), CandidateHit::new(2, 0.8)];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 0,
        combined_limit: 10,
    };
    let prepared = prepare_candidates(&lexical, &[], budget);
    assert_eq!(prepared.candidates.len(), 2);
    assert_eq!(prepared.counts.lexical_selected, 2);
    assert_eq!(prepared.counts.semantic_selected, 0);
    assert_eq!(prepared.counts.duplicates_removed, 0);
    // All candidates should have lexical source
    for c in &prepared.candidates {
        assert_eq!(c.first_source, CandidateSource::Lexical);
        assert!(c.lexical_rank.is_some());
        assert!(c.semantic_rank.is_none());
    }
}

#[test]
fn prepare_candidates_semantic_only() {
    let semantic = vec![CandidateHit::new(5, 0.95), CandidateHit::new(6, 0.85)];
    let budget = CandidateBudget {
        lexical_limit: 0,
        semantic_limit: 10,
        combined_limit: 10,
    };
    let prepared = prepare_candidates(&[], &semantic, budget);
    assert_eq!(prepared.candidates.len(), 2);
    assert_eq!(prepared.counts.semantic_selected, 2);
    assert_eq!(prepared.counts.lexical_selected, 0);
    for c in &prepared.candidates {
        assert_eq!(c.first_source, CandidateSource::Semantic);
        assert!(c.semantic_rank.is_some());
        assert!(c.lexical_rank.is_none());
    }
}

#[test]
fn prepare_candidates_complete_overlap_dedupes_all() {
    let lexical = vec![CandidateHit::new(1, 0.9), CandidateHit::new(2, 0.8)];
    let semantic = vec![CandidateHit::new(1, 0.7), CandidateHit::new(2, 0.6)];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 10,
    };
    let prepared = prepare_candidates(&lexical, &semantic, budget);
    assert_eq!(prepared.candidates.len(), 2, "full overlap → 2 deduped");
    assert_eq!(prepared.counts.duplicates_removed, 2);
    // Both candidates should have both ranks
    for c in &prepared.candidates {
        assert!(c.lexical_rank.is_some());
        assert!(c.semantic_rank.is_some());
    }
}

#[test]
fn prepare_candidates_combined_limit_truncates() {
    #[allow(clippy::cast_precision_loss)]
    let lexical = (1..=5)
        .map(|id| CandidateHit::new(id, (id as f64).mul_add(-0.1, 1.0)))
        .collect::<Vec<_>>();
    #[allow(clippy::cast_precision_loss)]
    let semantic = (6..=10)
        .map(|id| CandidateHit::new(id, (id as f64).mul_add(-0.05, 0.9)))
        .collect::<Vec<_>>();
    let budget = CandidateBudget {
        lexical_limit: 5,
        semantic_limit: 5,
        combined_limit: 3,
    };
    let prepared = prepare_candidates(&lexical, &semantic, budget);
    assert_eq!(
        prepared.candidates.len(),
        3,
        "combined_limit=3 should truncate"
    );
}

#[test]
fn prepare_candidates_first_source_is_lexical_when_seen_in_lexical_first() {
    let lexical = vec![CandidateHit::new(42, 0.9)];
    let semantic = vec![CandidateHit::new(42, 0.95)];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 10,
    };
    let prepared = prepare_candidates(&lexical, &semantic, budget);
    assert_eq!(prepared.candidates.len(), 1);
    assert_eq!(
        prepared.candidates[0].first_source,
        CandidateSource::Lexical,
        "first_source should be Lexical since it was inserted first"
    );
}

// ── prepared_candidate_cmp — ordering edge cases ──────────────

#[test]
fn cmp_prefers_better_best_rank() {
    let a = PreparedCandidate {
        doc_id: 1,
        lexical_rank: Some(1),
        semantic_rank: None,
        lexical_score: Some(0.5),
        semantic_score: None,
        first_source: CandidateSource::Lexical,
    };
    let b = PreparedCandidate {
        doc_id: 2,
        lexical_rank: Some(2),
        semantic_rank: None,
        lexical_score: Some(0.9),
        semantic_score: None,
        first_source: CandidateSource::Lexical,
    };
    assert_eq!(prepared_candidate_cmp(&a, &b), Ordering::Less);
}

#[test]
fn cmp_breaks_rank_tie_with_doc_id() {
    let a = PreparedCandidate {
        doc_id: 10,
        lexical_rank: Some(1),
        semantic_rank: None,
        lexical_score: Some(0.5),
        semantic_score: None,
        first_source: CandidateSource::Lexical,
    };
    let b = PreparedCandidate {
        doc_id: 5,
        lexical_rank: Some(1),
        semantic_rank: None,
        lexical_score: Some(0.5),
        semantic_score: None,
        first_source: CandidateSource::Lexical,
    };
    // Same rank, same score → doc_id tiebreaker: 5 < 10
    assert_eq!(prepared_candidate_cmp(&a, &b), Ordering::Greater);
}

#[test]
fn cmp_both_ranks_none_falls_to_doc_id() {
    let a = PreparedCandidate {
        doc_id: 1,
        lexical_rank: None,
        semantic_rank: None,
        lexical_score: None,
        semantic_score: None,
        first_source: CandidateSource::Lexical,
    };
    let b = PreparedCandidate {
        doc_id: 2,
        lexical_rank: None,
        semantic_rank: None,
        lexical_score: None,
        semantic_score: None,
        first_source: CandidateSource::Semantic,
    };
    assert_eq!(prepared_candidate_cmp(&a, &b), Ordering::Less);
}

#[test]
fn cmp_semantic_rank_breaks_tie_when_lexical_ranks_equal() {
    let a = PreparedCandidate {
        doc_id: 1,
        lexical_rank: Some(1),
        semantic_rank: Some(3),
        lexical_score: Some(0.5),
        semantic_score: Some(0.4),
        first_source: CandidateSource::Lexical,
    };
    let b = PreparedCandidate {
        doc_id: 2,
        lexical_rank: Some(1),
        semantic_rank: Some(1),
        lexical_score: Some(0.5),
        semantic_score: Some(0.4),
        first_source: CandidateSource::Semantic,
    };
    // best_rank tie (both 1), lexical_rank tie (both 1), semantic_rank: 3 vs 1
    assert_eq!(prepared_candidate_cmp(&a, &b), Ordering::Greater);
}

// ── CandidateStageCounts default ──────────────────────────────

#[test]
fn stage_counts_default_all_zero() {
    let counts = CandidateStageCounts::default();
    assert_eq!(counts.lexical_considered, 0);
    assert_eq!(counts.semantic_considered, 0);
    assert_eq!(counts.lexical_selected, 0);
    assert_eq!(counts.semantic_selected, 0);
    assert_eq!(counts.deduped_selected, 0);
    assert_eq!(counts.duplicates_removed, 0);
}

// ── CandidatePreparation / CandidateBudget serde ──────────────

#[test]
fn candidate_budget_serde_roundtrip() {
    let budget = CandidateBudget {
        lexical_limit: 42,
        semantic_limit: 99,
        combined_limit: 200,
    };
    let json = serde_json::to_string(&budget).unwrap();
    let budget2: CandidateBudget = serde_json::from_str(&json).unwrap();
    assert_eq!(budget, budget2);
}

#[test]
fn budget_decision_serde_roundtrip() {
    let config = CandidateBudgetConfig::default();
    let derivation = CandidateBudget::derive_with_decision(
        40,
        CandidateMode::Hybrid,
        QueryClass::Identifier,
        config,
    );
    let json = serde_json::to_string(&derivation.decision).unwrap();
    let decision2: CandidateBudgetDecision = serde_json::from_str(&json).unwrap();
    assert_eq!(derivation.decision, decision2);
}

#[test]
fn budget_derivation_serde_roundtrip() {
    let config = CandidateBudgetConfig::default();
    let derivation = CandidateBudget::derive_with_decision(
        25,
        CandidateMode::Auto,
        QueryClass::NaturalLanguage,
        config,
    );
    let json = serde_json::to_string(&derivation).unwrap();
    let derivation2: CandidateBudgetDerivation = serde_json::from_str(&json).unwrap();
    assert_eq!(derivation.budget, derivation2.budget);
    assert_eq!(derivation.decision.mode, derivation2.decision.mode);
    assert_eq!(derivation.decision.query_class, derivation2.decision.query_class);
    assert_eq!(
        derivation.decision.chosen_action,
        derivation2.decision.chosen_action
    );
    assert!(
        (derivation.decision.chosen_expected_loss - derivation2.decision.chosen_expected_loss)
            .abs()
            < 1e-12
    );
}

// ── Trait coverage ──────────────────────────────────────────────

#[test]
fn candidate_mode_debug_clone_copy() {
    let mode = CandidateMode::Hybrid;
    let debug = format!("{mode:?}");
    assert!(debug.contains("Hybrid"));
    let copied = mode; // Copy
    assert_eq!(mode, copied);
}

#[test]
fn query_class_debug_clone_copy() {
    let qc = QueryClass::Identifier;
    let debug = format!("{qc:?}");
    assert!(debug.contains("Identifier"));
    let copied = qc;
    assert_eq!(qc, copied);
}

#[test]
fn candidate_source_debug_clone_copy() {
    let src = CandidateSource::Lexical;
    let debug = format!("{src:?}");
    assert!(debug.contains("Lexical"));
    let copied = src;
    assert_eq!(src, copied);
}

#[test]
fn candidate_budget_config_serde_roundtrip() {
    let config = CandidateBudgetConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let config2: CandidateBudgetConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(config.min_lexical, config2.min_lexical);
    assert_eq!(config.max_combined, config2.max_combined);
}

#[test]
fn candidate_budget_config_debug_clone() {
    fn assert_clone<T: Clone>(_: &T) {}
    let config = CandidateBudgetConfig::default();
    let debug = format!("{config:?}");
    assert!(debug.contains("CandidateBudgetConfig"));
    assert_clone(&config);
}

#[test]
fn candidate_hit_serde_roundtrip() {
    let hit = CandidateHit::new(42, 0.95);
    let json = serde_json::to_string(&hit).unwrap();
    let hit2: CandidateHit = serde_json::from_str(&json).unwrap();
    assert_eq!(hit, hit2);
}

#[test]
fn candidate_hit_debug_clone() {
    let hit = CandidateHit::new(1, 0.5);
    let debug = format!("{hit:?}");
    assert!(debug.contains("CandidateHit"));
    let cloned = hit;
    assert_eq!(hit.doc_id, cloned.doc_id);
}

#[test]
fn prepared_candidate_serde_roundtrip() {
    let candidate = PreparedCandidate {
        doc_id: 10,
        lexical_rank: Some(1),
        semantic_rank: Some(3),
        lexical_score: Some(0.9),
        semantic_score: Some(0.7),
        first_source: CandidateSource::Lexical,
    };
    let json = serde_json::to_string(&candidate).unwrap();
    let candidate2: PreparedCandidate = serde_json::from_str(&json).unwrap();
    assert_eq!(candidate, candidate2);
}

#[test]
fn stage_counts_serde_roundtrip() {
    let counts = CandidateStageCounts {
        lexical_considered: 100,
        semantic_considered: 50,
        lexical_selected: 20,
        semantic_selected: 15,
        deduped_selected: 30,
        duplicates_removed: 5,
    };
    let json = serde_json::to_string(&counts).unwrap();
    let counts2: CandidateStageCounts = serde_json::from_str(&json).unwrap();
    assert_eq!(counts, counts2);
}

#[test]
fn candidate_preparation_serde_roundtrip() {
    let prep = CandidatePreparation {
        budget: CandidateBudget {
            lexical_limit: 10,
            semantic_limit: 10,
            combined_limit: 20,
        },
        counts: CandidateStageCounts::default(),
        candidates: vec![PreparedCandidate {
            doc_id: 1,
            lexical_rank: Some(1),
            semantic_rank: None,
            lexical_score: Some(0.8),
            semantic_score: None,
            first_source: CandidateSource::Lexical,
        }],
    };
    let json = serde_json::to_string(&prep).unwrap();
    let prep2: CandidatePreparation = serde_json::from_str(&json).unwrap();
    assert_eq!(prep, prep2);
}

// ── QueryClass::classify additional edge cases ──────────────────

#[test]
fn query_classifies_hyphenated_only_as_identifier() {
    // All tokens hyphenated + alphanumeric only → identifier
    assert_eq!(
        QueryClass::classify("mcp-agent-mail"),
        QueryClass::Identifier
    );
}

#[test]
fn query_classifies_thread_prefix_as_identifier() {
    assert_eq!(QueryClass::classify("thread:main"), QueryClass::Identifier);
}

#[test]
fn query_classifies_three_short_words_as_natural_language() {
    // 3 tokens → not ShortKeyword (≤2), but short avg → NaturalLanguage
    assert_eq!(
        QueryClass::classify("fix the bug"),
        QueryClass::NaturalLanguage
    );
}

#[test]
fn query_classifies_long_single_word_as_short_keyword() {
    // Single token, 10 chars → ShortKeyword (avg_len ≤ 10)
    assert_eq!(QueryClass::classify("embeddings"), QueryClass::ShortKeyword);
}

#[test]
fn candidate_mode_serde_snake_case() {
    let json = serde_json::to_string(&CandidateMode::LexicalFallback).unwrap();
    assert_eq!(json, "\"lexical_fallback\"");
}

#[test]
fn query_class_serde_snake_case() {
    let json = serde_json::to_string(&QueryClass::ShortKeyword).unwrap();
    assert_eq!(json, "\"short_keyword\"");
    let json2 = serde_json::to_string(&QueryClass::NaturalLanguage).unwrap();
    assert_eq!(json2, "\"natural_language\"");
}

// ── best_rank ──────────────────────────────────────────────────

#[test]
fn best_rank_picks_minimum() {
    let candidate = PreparedCandidate {
        doc_id: 1,
        lexical_rank: Some(5),
        semantic_rank: Some(2),
        lexical_score: None,
        semantic_score: None,
        first_source: CandidateSource::Semantic,
    };
    assert_eq!(candidate.best_rank(), 2);
}

#[test]
fn best_rank_with_only_lexical() {
    let candidate = PreparedCandidate {
        doc_id: 1,
        lexical_rank: Some(3),
        semantic_rank: None,
        lexical_score: None,
        semantic_score: None,
        first_source: CandidateSource::Lexical,
    };
    assert_eq!(candidate.best_rank(), 3);
}

#[test]
fn best_rank_with_no_ranks() {
    let candidate = PreparedCandidate {
        doc_id: 1,
        lexical_rank: None,
        semantic_rank: None,
        lexical_score: None,
        semantic_score: None,
        first_source: CandidateSource::Lexical,
    };
    assert_eq!(candidate.best_rank(), usize::MAX);
}

// ── score_cmp_desc and rank_or_max ─────────────────────────────

#[test]
fn rank_or_max_some_returns_value() {
    assert_eq!(rank_or_max(Some(5)), 5);
}

#[test]
fn rank_or_max_none_returns_max() {
    assert_eq!(rank_or_max(None), usize::MAX);
}

#[test]
fn score_cmp_desc_both_some() {
    // Higher score should come first (descending)
    assert_eq!(score_cmp_desc(Some(0.9), Some(0.5)), Ordering::Less);
    assert_eq!(score_cmp_desc(Some(0.5), Some(0.9)), Ordering::Greater);
    assert_eq!(score_cmp_desc(Some(0.5), Some(0.5)), Ordering::Equal);
}

#[test]
fn score_cmp_desc_none_values() {
    // None treated as NEG_INFINITY; function does b.cmp(a) for descending
    // score_cmp_desc(a=None, b=Some(0.5)) = 0.5.cmp(NEG_INF) = Greater
    assert_eq!(score_cmp_desc(None, Some(0.5)), Ordering::Greater);
    // score_cmp_desc(a=Some(0.5), b=None) = NEG_INF.cmp(0.5) = Less
    assert_eq!(score_cmp_desc(Some(0.5), None), Ordering::Less);
    assert_eq!(score_cmp_desc(None, None), Ordering::Equal);
}

// ── prepare_candidates with negative scores ────────────────────

#[test]
fn prepare_candidates_negative_scores() {
    let lexical = vec![CandidateHit::new(1, -0.5), CandidateHit::new(2, -1.0)];
    let semantic = vec![CandidateHit::new(3, -0.1)];
    let budget = CandidateBudget {
        lexical_limit: 10,
        semantic_limit: 10,
        combined_limit: 10,
    };
    let prepared = prepare_candidates(&lexical, &semantic, budget);
    assert_eq!(prepared.candidates.len(), 3);
}

#[test]
fn prepare_candidates_single_hit_each() {
    let lexical = vec![CandidateHit::new(100, 0.9)];
    let semantic = vec![CandidateHit::new(200, 0.8)];
    let budget = CandidateBudget {
        lexical_limit: 1,
        semantic_limit: 1,
        combined_limit: 2,
    };
    let prepared = prepare_candidates(&lexical, &semantic, budget);
    assert_eq!(prepared.candidates.len(), 2);
    assert_eq!(prepared.counts.duplicates_removed, 0);
}
