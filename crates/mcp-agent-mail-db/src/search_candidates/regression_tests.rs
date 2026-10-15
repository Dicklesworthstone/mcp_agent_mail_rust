use super::*;

fn budget(lexical_limit: usize, semantic_limit: usize, combined_limit: usize) -> CandidateBudget {
    CandidateBudget {
        lexical_limit,
        semantic_limit,
        combined_limit,
    }
}

#[test]
fn repeated_lexical_hit_keeps_best_rank_and_that_occurrences_score() {
    let hits = [
        CandidateHit::new(10, 0.25),
        CandidateHit::new(20, 0.9),
        CandidateHit::new(10, 99.0),
    ];
    let prepared = prepare_candidates(&hits, &[], budget(3, 0, 1));
    assert_eq!(prepared.candidates.len(), 1);
    let winner = &prepared.candidates[0];
    assert_eq!(winner.doc_id, 10);
    assert_eq!(winner.lexical_rank, Some(1));
    assert_eq!(winner.lexical_score, Some(0.25));
    assert_eq!(prepared.counts.duplicates_removed, 1);
}

#[test]
fn repeated_semantic_hit_keeps_best_rank_with_and_without_lexical_overlap() {
    let semantic = [
        CandidateHit::new(10, 0.25),
        CandidateHit::new(20, 0.9),
        CandidateHit::new(10, 99.0),
    ];
    for lexical in [vec![], vec![CandidateHit::new(10, 0.5)]] {
        let prepared = prepare_candidates(&lexical, &semantic, budget(1, 3, 1));
        let winner = &prepared.candidates[0];
        assert_eq!(winner.doc_id, 10);
        assert_eq!(winner.semantic_rank, Some(1));
        assert_eq!(winner.semantic_score, Some(0.25));
        assert_eq!(
            winner.lexical_rank,
            if lexical.is_empty() { None } else { Some(1) }
        );
    }
}

#[test]
fn unique_hits_discarded_by_combined_cap_are_not_duplicates() {
    let lexical = [CandidateHit::new(1, 1.0), CandidateHit::new(2, 0.5)];
    let semantic = [CandidateHit::new(3, 1.0), CandidateHit::new(4, 0.5)];
    for cap in 0..=4 {
        let prepared = prepare_candidates(&lexical, &semantic, budget(2, 2, cap));
        assert_eq!(prepared.counts.duplicates_removed, 0);
        assert_eq!(prepared.counts.deduped_selected, cap);
    }
}

#[test]
fn duplicate_count_is_independent_of_combined_cap() {
    let lexical = [
        CandidateHit::new(1, 1.0),
        CandidateHit::new(1, 0.5),
        CandidateHit::new(2, 0.25),
    ];
    let semantic = [
        CandidateHit::new(1, 1.0),
        CandidateHit::new(3, 0.5),
        CandidateHit::new(3, 0.25),
    ];
    for cap in 0..=6 {
        let prepared = prepare_candidates(&lexical, &semantic, budget(3, 3, cap));
        assert_eq!(prepared.counts.duplicates_removed, 3);
        assert_eq!(prepared.candidates.len(), cap.min(3));
    }
}

#[test]
fn hits_outside_source_budget_do_not_change_rank_or_duplicate_count() {
    let lexical = [
        CandidateHit::new(10, 1.0),
        CandidateHit::new(20, 0.5),
        CandidateHit::new(10, 0.25),
    ];
    let prepared = prepare_candidates(&lexical, &[], budget(2, 0, usize::MAX));
    assert_eq!(prepared.candidates[0].lexical_rank, Some(1));
    assert_eq!(prepared.counts.lexical_considered, 3);
    assert_eq!(prepared.counts.lexical_selected, 2);
    assert_eq!(prepared.counts.duplicates_removed, 0);
}

#[test]
fn generated_source_sequences_preserve_first_occurrences_and_exact_accounting() {
    fn hits(mut encoded: i32) -> Vec<CandidateHit> {
        (0..3)
            .map(|rank| {
                let hit = CandidateHit::new(i64::from(encoded % 3), f64::from(3 - rank));
                encoded /= 3;
                hit
            })
            .collect()
    }

    for lexical_code in 0..27 {
        let lexical = hits(lexical_code);
        for semantic_code in 0..27 {
            let semantic = hits(semantic_code);
            for lexical_limit in 0..=3 {
                for semantic_limit in 0..=3 {
                    let selected_lexical = &lexical[..lexical_limit];
                    let selected_semantic = &semantic[..semantic_limit];
                    let unique = selected_lexical
                        .iter()
                        .chain(selected_semantic)
                        .map(|hit| hit.doc_id)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len();
                    for combined_limit in 0..=3 {
                        let prepared = prepare_candidates(
                            &lexical,
                            &semantic,
                            budget(lexical_limit, semantic_limit, combined_limit),
                        );
                        assert_eq!(
                            prepared.counts.duplicates_removed,
                            lexical_limit + semantic_limit - unique
                        );
                        assert_eq!(prepared.candidates.len(), combined_limit.min(unique));
                        for candidate in &prepared.candidates {
                            for (source, rank, score) in [
                                (
                                    selected_lexical,
                                    candidate.lexical_rank,
                                    candidate.lexical_score,
                                ),
                                (
                                    selected_semantic,
                                    candidate.semantic_rank,
                                    candidate.semantic_score,
                                ),
                            ] {
                                let first = source
                                    .iter()
                                    .position(|hit| hit.doc_id == candidate.doc_id);
                                assert_eq!(rank, first.map(|idx| idx + 1));
                                assert_eq!(score, first.map(|idx| source[idx].score));
                            }
                        }
                    }
                }
            }
        }
    }
}
