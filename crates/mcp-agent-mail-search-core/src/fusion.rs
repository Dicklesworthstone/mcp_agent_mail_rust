//! Reciprocal Rank Fusion (RRF) for hybrid search.
//!
//! This module implements deterministic RRF fusion with explainable score contributions:
//! - Configurable RRF constant k (default 60)
//! - Deterministic tie-breaking chain: `rrf_score` desc → `lexical_score` desc → `doc_id` asc
//! - Per-hit explain payload with source contributions
//! - Pagination applied after fusion

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::hybrid_candidates::{CandidateSource, PreparedCandidate};

/// Default RRF constant (k).
///
/// Standard value from the original RRF paper. Higher k reduces the impact of
/// top-ranked documents relative to lower-ranked ones.
pub const DEFAULT_RRF_K: f64 = 60.0;

/// Environment variable for overriding the RRF constant.
pub const RRF_K_ENV_VAR: &str = "AM_SEARCH_RRF_K";

/// Configuration for RRF fusion.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RrfConfig {
    /// RRF constant k. Score contribution from source s is `1/(k + rank_in_source_s)`.
    pub k: f64,
    /// Epsilon for floating-point score comparison (determines "near-tie" threshold).
    pub epsilon: f64,
}

impl Default for RrfConfig {
    fn default() -> Self {
        Self {
            k: DEFAULT_RRF_K,
            epsilon: 1e-9,
        }
    }
}

impl RrfConfig {
    /// Load RRF config from environment, falling back to defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let k = std::env::var(RRF_K_ENV_VAR)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&v| v > 0.0)
            .unwrap_or(DEFAULT_RRF_K);

        Self {
            k,
            ..Default::default()
        }
    }
}

/// Source contribution to the RRF score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceContribution {
    /// Source name ("lexical" or "semantic").
    pub source: String,
    /// Contribution value: 1/(k + rank) or 0 if absent.
    pub contribution: f64,
    /// Original rank in this source (None if doc not present in source).
    pub rank: Option<usize>,
}

/// Explain payload for a fused hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusionExplain {
    /// Lexical rank (1-based) if present in lexical pool.
    pub lexical_rank: Option<usize>,
    /// Lexical score if present.
    pub lexical_score: Option<f64>,
    /// Semantic rank (1-based) if present in semantic pool.
    pub semantic_rank: Option<usize>,
    /// Semantic score if present.
    pub semantic_score: Option<f64>,
    /// Final fused RRF score.
    pub rrf_score: f64,
    /// Per-source contributions to the RRF score.
    pub source_contributions: Vec<SourceContribution>,
}

/// A fused search result with RRF score and explain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusedHit {
    /// Document identifier.
    pub doc_id: i64,
    /// Fused RRF score.
    pub rrf_score: f64,
    /// Which source first introduced this document.
    pub first_source: CandidateSource,
    /// Detailed explain for debugging and transparency.
    pub explain: FusionExplain,
}

/// Result of RRF fusion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusionResult {
    /// RRF config used for fusion.
    pub config: RrfConfig,
    /// Number of candidates before fusion.
    pub input_count: usize,
    /// Total fused hits (before pagination).
    pub total_fused: usize,
    /// Fused and paginated hits.
    pub hits: Vec<FusedHit>,
    /// Number of hits skipped due to offset.
    pub offset_applied: usize,
    /// Maximum hits returned (limit).
    pub limit_applied: usize,
}

/// Compute RRF contribution for a given rank.
///
/// Score = 1 / (k + rank), where rank is 1-based.
#[inline]
#[allow(clippy::cast_precision_loss)] // Rank values are small enough that precision loss is negligible
fn rrf_contribution(k: f64, rank: Option<usize>) -> f64 {
    rank.map_or(0.0, |r| 1.0 / (k + r as f64))
}

/// Fuse prepared candidates using RRF.
///
/// # Arguments
/// * `candidates` - Deduplicated candidates from [`prepare_candidates`](crate::hybrid_candidates::prepare_candidates)
/// * `config` - RRF configuration
/// * `offset` - Number of results to skip (for pagination)
/// * `limit` - Maximum number of results to return
///
/// # Returns
/// Fused results with deterministic ordering and explain payloads.
#[must_use]
pub fn fuse_rrf(
    candidates: &[PreparedCandidate],
    config: RrfConfig,
    offset: usize,
    limit: usize,
) -> FusionResult {
    let mut fused: Vec<FusedHit> = candidates
        .iter()
        .map(|c| {
            let lexical_contrib = rrf_contribution(config.k, c.lexical_rank);
            let semantic_contrib = rrf_contribution(config.k, c.semantic_rank);
            let rrf_score = lexical_contrib + semantic_contrib;

            let source_contributions = vec![
                SourceContribution {
                    source: "lexical".to_string(),
                    contribution: lexical_contrib,
                    rank: c.lexical_rank,
                },
                SourceContribution {
                    source: "semantic".to_string(),
                    contribution: semantic_contrib,
                    rank: c.semantic_rank,
                },
            ];

            FusedHit {
                doc_id: c.doc_id,
                rrf_score,
                first_source: c.first_source,
                explain: FusionExplain {
                    lexical_rank: c.lexical_rank,
                    lexical_score: c.lexical_score,
                    semantic_rank: c.semantic_rank,
                    semantic_score: c.semantic_score,
                    rrf_score,
                    source_contributions,
                },
            }
        })
        .collect();

    // Sort by deterministic tie-breaking chain
    fused.sort_by(|a, b| fused_hit_cmp(a, b, config.epsilon));

    let total_fused = fused.len();

    // Apply pagination after fusion
    let paginated: Vec<FusedHit> = fused
        .into_iter()
        .skip(offset)
        .take(limit.max(1))
        .collect();

    FusionResult {
        config,
        input_count: candidates.len(),
        total_fused,
        hits: paginated,
        offset_applied: offset,
        limit_applied: limit,
    }
}

/// Deterministic comparison for fused hits.
///
/// Tie-breaking chain:
/// 1. RRF score descending (with epsilon comparison for near-ties)
/// 2. Lexical score descending (favor lexical matches on tie)
/// 3. Doc ID ascending (absolute determinism)
fn fused_hit_cmp(a: &FusedHit, b: &FusedHit, epsilon: f64) -> Ordering {
    // 1. RRF score descending (with epsilon for near-ties)
    let rrf_diff = b.rrf_score - a.rrf_score;
    if rrf_diff.abs() > epsilon {
        return if rrf_diff > 0.0 {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }

    // 2. Lexical score descending (favor lexical matches)
    let a_lex = a.explain.lexical_score.unwrap_or(f64::NEG_INFINITY);
    let b_lex = b.explain.lexical_score.unwrap_or(f64::NEG_INFINITY);
    let lex_diff = b_lex - a_lex;
    if lex_diff.abs() > epsilon {
        return if lex_diff > 0.0 {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }

    // 3. Doc ID ascending (absolute determinism)
    a.doc_id.cmp(&b.doc_id)
}

/// Convenience function to fuse with default config and no pagination.
#[must_use]
pub fn fuse_rrf_default(candidates: &[PreparedCandidate]) -> FusionResult {
    fuse_rrf(candidates, RrfConfig::default(), 0, usize::MAX)
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::suboptimal_flops
)]
mod tests {
    use super::*;
    use crate::hybrid_candidates::{CandidateHit, CandidateBudget, prepare_candidates};

    fn make_candidate(
        doc_id: i64,
        lexical_rank: Option<usize>,
        semantic_rank: Option<usize>,
        lexical_score: Option<f64>,
        semantic_score: Option<f64>,
    ) -> PreparedCandidate {
        PreparedCandidate {
            doc_id,
            lexical_rank,
            semantic_rank,
            lexical_score,
            semantic_score,
            first_source: if lexical_rank.is_some() {
                CandidateSource::Lexical
            } else {
                CandidateSource::Semantic
            },
        }
    }

    #[test]
    fn test_rrf_contribution() {
        let k = 60.0;
        // Rank 1: 1/(60+1) = 1/61 ≈ 0.0164
        assert!((rrf_contribution(k, Some(1)) - 1.0 / 61.0).abs() < 1e-10);
        // Rank 10: 1/(60+10) = 1/70 ≈ 0.0143
        assert!((rrf_contribution(k, Some(10)) - 1.0 / 70.0).abs() < 1e-10);
        // None: 0
        assert_eq!(rrf_contribution(k, None), 0.0);
    }

    #[test]
    fn test_overlapping_pools_dedup() {
        // Two pools of 10 docs each, 3 overlapping (docs 5, 6, 7)
        let lexical: Vec<_> = (1..=10).map(|i| CandidateHit::new(i, 1.0 - i as f64 * 0.1)).collect();
        let semantic: Vec<_> = (5..=14).map(|i| CandidateHit::new(i, 0.9 - (i - 5) as f64 * 0.1)).collect();

        let budget = CandidateBudget {
            lexical_limit: 10,
            semantic_limit: 10,
            combined_limit: 100,
        };

        let prepared = prepare_candidates(&lexical, &semantic, budget);
        let result = fuse_rrf_default(&prepared.candidates);

        // Should have 14 unique docs (1-14), not 20
        assert_eq!(result.total_fused, 14);

        // Overlapping docs (5, 6, 7) should have contributions from both sources
        for hit in &result.hits {
            if hit.doc_id >= 5 && hit.doc_id <= 7 {
                let lexical_contrib = hit.explain.source_contributions
                    .iter()
                    .find(|c| c.source == "lexical")
                    .unwrap();
                let semantic_contrib = hit.explain.source_contributions
                    .iter()
                    .find(|c| c.source == "semantic")
                    .unwrap();
                assert!(lexical_contrib.contribution > 0.0);
                assert!(semantic_contrib.contribution > 0.0);
            }
        }
    }

    #[test]
    fn test_tie_breaking_deterministic() {
        // Two docs with identical RRF scores but different lexical scores
        let candidates = vec![
            make_candidate(100, Some(1), None, Some(0.5), None),
            make_candidate(200, Some(1), None, Some(0.9), None), // Higher lexical score
        ];

        let result = fuse_rrf_default(&candidates);

        // Doc 200 should come first due to higher lexical score
        assert_eq!(result.hits[0].doc_id, 200);
        assert_eq!(result.hits[1].doc_id, 100);
    }

    #[test]
    fn test_single_source_not_penalized() {
        let k = 60.0;
        let config = RrfConfig { k, epsilon: 1e-9 };

        // Doc only in lexical pool at rank 1
        let candidates = vec![make_candidate(42, Some(1), None, Some(0.9), None)];

        let result = fuse_rrf(&candidates, config, 0, 100);

        // Score should be 1/(k+1) = 1/61, NOT penalized for missing semantic
        let expected_score = 1.0 / (k + 1.0);
        assert!((result.hits[0].rrf_score - expected_score).abs() < 1e-10);
        assert_eq!(result.hits[0].explain.semantic_rank, None);
    }

    #[test]
    fn test_empty_pool_passthrough() {
        let lexical: Vec<CandidateHit> = vec![
            CandidateHit::new(1, 0.9),
            CandidateHit::new(2, 0.8),
        ];
        let semantic: Vec<CandidateHit> = vec![];

        let budget = CandidateBudget {
            lexical_limit: 10,
            semantic_limit: 10,
            combined_limit: 100,
        };

        let prepared = prepare_candidates(&lexical, &semantic, budget);
        let result = fuse_rrf_default(&prepared.candidates);

        // All lexical docs should pass through
        assert_eq!(result.total_fused, 2);
        assert!(result.hits.iter().all(|h| h.explain.semantic_rank.is_none()));
    }

    #[test]
    fn test_explain_has_both_contributions() {
        // Doc in both pools
        let candidates = vec![make_candidate(42, Some(1), Some(2), Some(0.9), Some(0.8))];

        let result = fuse_rrf_default(&candidates);
        let explain = &result.hits[0].explain;

        assert_eq!(explain.lexical_rank, Some(1));
        assert_eq!(explain.semantic_rank, Some(2));
        assert_eq!(explain.source_contributions.len(), 2);

        let lexical_contrib = explain.source_contributions
            .iter()
            .find(|c| c.source == "lexical")
            .unwrap();
        assert!(lexical_contrib.contribution > 0.0);
        assert_eq!(lexical_contrib.rank, Some(1));
    }

    #[test]
    fn test_pagination_after_fusion() {
        let candidates: Vec<_> = (1..=10)
            .map(|i| make_candidate(i, Some(i as usize), None, Some(1.0 - i as f64 * 0.1), None))
            .collect();

        // Page 2: offset=3, limit=3
        let result = fuse_rrf(&candidates, RrfConfig::default(), 3, 3);

        assert_eq!(result.total_fused, 10);
        assert_eq!(result.hits.len(), 3);
        assert_eq!(result.offset_applied, 3);
        assert_eq!(result.limit_applied, 3);

        // Should be docs 4, 5, 6 (0-indexed after sort by RRF descending)
        // Doc 1 has highest RRF (rank 1), doc 10 has lowest (rank 10)
        assert_eq!(result.hits[0].doc_id, 4);
        assert_eq!(result.hits[1].doc_id, 5);
        assert_eq!(result.hits[2].doc_id, 6);
    }

    #[test]
    fn test_determinism_100_runs() {
        let candidates: Vec<_> = (1..=20)
            .map(|i| {
                make_candidate(
                    i,
                    if i % 2 == 0 { Some((i / 2) as usize) } else { None },
                    if i % 3 == 0 { Some((i / 3) as usize) } else { None },
                    if i % 2 == 0 { Some(1.0 / i as f64) } else { None },
                    if i % 3 == 0 { Some(0.5 / i as f64) } else { None },
                )
            })
            .collect();

        let first_result = fuse_rrf_default(&candidates);
        let first_order: Vec<i64> = first_result.hits.iter().map(|h| h.doc_id).collect();

        for _ in 0..100 {
            let result = fuse_rrf_default(&candidates);
            let order: Vec<i64> = result.hits.iter().map(|h| h.doc_id).collect();
            assert_eq!(order, first_order, "Ordering should be deterministic across runs");
        }
    }

    #[test]
    fn test_config_default() {
        let config = RrfConfig::default();
        assert_eq!(config.k, DEFAULT_RRF_K);
        assert!(config.epsilon > 0.0);
    }

    #[test]
    fn test_custom_k_affects_scores() {
        let candidates = vec![
            make_candidate(1, Some(1), Some(2), Some(0.9), Some(0.8)),
        ];

        // With default k=60
        let default_result = fuse_rrf(&candidates, RrfConfig::default(), 0, 100);
        let default_score = default_result.hits[0].rrf_score;

        // With smaller k=10 (higher scores for top ranks)
        let small_k_config = RrfConfig { k: 10.0, epsilon: 1e-9 };
        let small_k_result = fuse_rrf(&candidates, small_k_config, 0, 100);
        let small_k_score = small_k_result.hits[0].rrf_score;

        // Smaller k should give higher scores (1/(10+1) > 1/(60+1))
        assert!(small_k_score > default_score);
    }

    #[test]
    fn test_doc_id_tiebreaker_when_all_else_equal() {
        // Two docs with exactly the same scores
        let candidates = vec![
            make_candidate(200, Some(1), Some(1), Some(0.9), Some(0.9)),
            make_candidate(100, Some(1), Some(1), Some(0.9), Some(0.9)),
        ];

        let result = fuse_rrf_default(&candidates);

        // Doc 100 should come first (lower doc_id wins on tie)
        assert_eq!(result.hits[0].doc_id, 100);
        assert_eq!(result.hits[1].doc_id, 200);
    }
}
