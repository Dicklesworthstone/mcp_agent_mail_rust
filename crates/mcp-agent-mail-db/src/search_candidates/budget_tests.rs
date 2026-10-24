use super::*;

const MODES: [CandidateMode; 3] = [
    CandidateMode::Hybrid,
    CandidateMode::Auto,
    CandidateMode::LexicalFallback,
];
const CLASSES: [QueryClass; 4] = [
    QueryClass::Identifier,
    QueryClass::ShortKeyword,
    QueryClass::NaturalLanguage,
    QueryClass::Empty,
];

#[test]
fn default_budgets_are_unchanged() {
    let expected = [
        [(90, 40, 130), (75, 60, 135), (54, 108, 162), (80, 0, 80)],
        [(90, 30, 120), (75, 45, 120), (54, 81, 135), (80, 0, 80)],
        [(120, 0, 120), (100, 0, 100), (72, 0, 72), (80, 0, 80)],
    ];
    for (mode, expected) in MODES.into_iter().zip(expected) {
        for (class, (lexical, semantic, combined)) in CLASSES.into_iter().zip(expected) {
            let budget = CandidateBudget::derive(20, mode, class, CandidateBudgetConfig::default());
            assert_eq!(
                (
                    budget.lexical_limit,
                    budget.semantic_limit,
                    budget.combined_limit
                ),
                (lexical, semantic, combined),
                "{mode:?}, {class:?}"
            );
        }
    }
}

#[test]
fn zero_caps_disable_lanes_despite_positive_minima() {
    let config = CandidateBudgetConfig {
        max_lexical: 0,
        max_semantic: 0,
        ..CandidateBudgetConfig::default()
    };
    for mode in MODES {
        for class in CLASSES {
            let budget = CandidateBudget::derive(20, mode, class, config);
            assert_eq!(budget.lexical_limit, 0);
            assert_eq!(budget.semantic_limit, 0);
            assert_eq!(budget.combined_limit, 0);
        }
    }
}

#[test]
fn inverted_bounds_obey_hard_maxima() {
    let config = CandidateBudgetConfig {
        min_lexical: usize::MAX,
        max_lexical: 3,
        min_semantic: usize::MAX,
        max_semantic: 2,
        ..CandidateBudgetConfig::default()
    };
    let budget = CandidateBudget::derive(20, CandidateMode::Hybrid, QueryClass::Identifier, config);
    assert_eq!(budget.lexical_limit, 3);
    assert_eq!(budget.semantic_limit, 2);
    assert_eq!(budget.combined_limit, 5);
}

#[test]
fn lexical_fallback_cannot_resurrect_semantic_retrieval() {
    let config = CandidateBudgetConfig {
        lexical_fallback_bps: 0,
        ..CandidateBudgetConfig::default()
    };
    for class in CLASSES {
        let budget = CandidateBudget::derive(20, CandidateMode::LexicalFallback, class, config);
        assert_eq!(budget.lexical_limit, 0);
        assert_eq!(budget.semantic_limit, 0);
    }
}

#[test]
fn disabled_mode_multipliers_cannot_be_revived_by_fallback_settings() {
    let config = CandidateBudgetConfig {
        hybrid_lexical_bps: 0,
        hybrid_semantic_bps: 0,
        auto_lexical_bps: 0,
        auto_semantic_bps: 0,
        ..CandidateBudgetConfig::default()
    };
    for mode in [CandidateMode::Hybrid, CandidateMode::Auto] {
        for class in CLASSES {
            let budget = CandidateBudget::derive(20, mode, class, config);
            assert_eq!(budget.lexical_limit, 0);
            assert_eq!(budget.semantic_limit, 0);
        }
    }
}

#[test]
fn smaller_lexical_fallback_never_erases_or_shrinks_a_healthy_lane() {
    for fallback in [0, 100, 400] {
        let config = CandidateBudgetConfig {
            hybrid_semantic_bps: 0,
            lexical_fallback_bps: fallback,
            ..CandidateBudgetConfig::default()
        };
        let budget =
            CandidateBudget::derive(20, CandidateMode::Hybrid, QueryClass::Identifier, config);
        assert_eq!(budget.lexical_limit, if fallback == 400 { 120 } else { 90 });
        assert_eq!(budget.semantic_limit, 0);
    }
}

#[test]
fn smaller_hybrid_multiplier_never_erases_or_shrinks_auto_semantic_lane() {
    for hybrid_semantic in [0, 100, 400] {
        let config = CandidateBudgetConfig {
            auto_lexical_bps: 0,
            hybrid_semantic_bps: hybrid_semantic,
            ..CandidateBudgetConfig::default()
        };
        let budget =
            CandidateBudget::derive(20, CandidateMode::Auto, QueryClass::NaturalLanguage, config);
        assert_eq!(budget.lexical_limit, 0);
        assert_eq!(
            budget.semantic_limit,
            if hybrid_semantic == 400 { 108 } else { 81 }
        );
    }
}

#[test]
fn zero_combined_cap_avoids_retrieval_but_positive_cap_preserves_source_overfetch() {
    for combined in [0, 1] {
        let config = CandidateBudgetConfig {
            max_combined: combined,
            ..CandidateBudgetConfig::default()
        };
        let budget =
            CandidateBudget::derive(20, CandidateMode::Hybrid, QueryClass::Identifier, config);
        assert_eq!(budget.combined_limit, combined);
        assert_eq!(budget.lexical_limit, if combined == 0 { 0 } else { 90 });
        assert_eq!(budget.semantic_limit, if combined == 0 { 0 } else { 40 });
    }
}

#[test]
fn generated_configurations_preserve_caps_and_disabled_lanes() {
    let bounds = [
        (0, 0),
        (20, 0),
        (2, 1),
        (usize::MAX, 1),
        (0, usize::MAX),
        (usize::MAX, usize::MAX),
    ];
    let mut checked = 0;
    for mode in MODES {
        for class in CLASSES {
            for requested in [0, 1, 20, usize::MAX] {
                for (min_lexical, max_lexical) in bounds {
                    for (min_semantic, max_semantic) in bounds {
                        for lexical_bps in [0, 1, u32::MAX] {
                            for semantic_bps in [0, 1, u32::MAX] {
                                for max_combined in [0, 7, usize::MAX] {
                                    let config = CandidateBudgetConfig {
                                        hybrid_lexical_bps: lexical_bps,
                                        hybrid_semantic_bps: semantic_bps,
                                        auto_lexical_bps: lexical_bps,
                                        auto_semantic_bps: semantic_bps,
                                        lexical_fallback_bps: lexical_bps,
                                        min_lexical,
                                        min_semantic,
                                        max_lexical,
                                        max_semantic,
                                        max_combined,
                                    };
                                    let budget =
                                        CandidateBudget::derive(requested, mode, class, config);
                                    assert!(budget.lexical_limit <= max_lexical);
                                    assert!(budget.semantic_limit <= max_semantic);
                                    assert_eq!(
                                        budget.combined_limit,
                                        budget
                                            .lexical_limit
                                            .saturating_add(budget.semantic_limit)
                                            .min(max_combined)
                                    );
                                    if lexical_bps == 0 || max_combined == 0 {
                                        assert_eq!(budget.lexical_limit, 0);
                                    }
                                    if semantic_bps == 0
                                        || max_combined == 0
                                        || mode == CandidateMode::LexicalFallback
                                        || class == QueryClass::Empty
                                    {
                                        assert_eq!(budget.semantic_limit, 0);
                                    }
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(checked, 46_656);
}
