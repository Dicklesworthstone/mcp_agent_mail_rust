//! Hybrid candidate orchestration for lexical + semantic retrieval.
//!
//! This module provides the pre-fusion stage used by Search V3 hybrid mode:
//! - mode-aware candidate budget sizing
//! - query-class-aware multiplier adjustment
//! - deterministic dedupe + merge preparation
//!
//! RRF fusion and reranking are intentionally out-of-scope here and are built on
//! top of the `PreparedCandidate` stream produced by this module.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Retrieval mode for candidate orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateMode {
    /// Explicit hybrid mode (lexical + semantic).
    Hybrid,
    /// Adaptive mode (balances lexical and semantic pools).
    Auto,
    /// Degraded lexical-only fallback.
    LexicalFallback,
}

/// Coarse query classification for budget shaping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    /// Thread/issue IDs and other structured identifiers (`br-123`, `thread:abc`).
    Identifier,
    /// Short keyword-like query (typically 1-2 compact tokens).
    ShortKeyword,
    /// Longer natural-language query.
    NaturalLanguage,
    /// Empty/whitespace query.
    Empty,
}

impl QueryClass {
    /// Classify a query for mode-aware candidate budgeting.
    #[must_use]
    pub fn classify(raw_query: &str) -> Self {
        let trimmed = raw_query.trim();
        if trimmed.is_empty() {
            return Self::Empty;
        }

        let lower = trimmed.to_ascii_lowercase();
        let token_count = lower.split_whitespace().count();
        let avg_token_len = lower
            .split_whitespace()
            .map(str::len)
            .sum::<usize>()
            .checked_div(token_count.max(1))
            .unwrap_or(0);

        let looks_like_identifier = lower.starts_with("br-")
            || lower.starts_with("thread:")
            || lower.contains('_')
            || lower.contains('/')
            || lower.split_whitespace().any(|token| {
                let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
                let has_digit = token.chars().any(|c| c.is_ascii_digit());
                has_alpha && has_digit
            })
            || lower.split_whitespace().all(|token| {
                token.contains('-')
                    && token
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
            });

        if looks_like_identifier {
            Self::Identifier
        } else if token_count <= 2 && avg_token_len <= 10 {
            Self::ShortKeyword
        } else {
            Self::NaturalLanguage
        }
    }
}

/// Tunables for candidate budget derivation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CandidateBudgetConfig {
    /// Base lexical multiplier in explicit hybrid mode, scaled by 100.
    pub hybrid_lexical_bps: u32,
    /// Base semantic multiplier in explicit hybrid mode, scaled by 100.
    pub hybrid_semantic_bps: u32,
    /// Base lexical multiplier in auto mode, scaled by 100.
    pub auto_lexical_bps: u32,
    /// Base semantic multiplier in auto mode, scaled by 100.
    pub auto_semantic_bps: u32,
    /// Base lexical multiplier in lexical fallback mode, scaled by 100.
    pub lexical_fallback_bps: u32,
    /// Minimum lexical candidates to request.
    pub min_lexical: usize,
    /// Minimum semantic candidates to request when semantic tier is active.
    pub min_semantic: usize,
    /// Maximum lexical candidate request cap.
    pub max_lexical: usize,
    /// Maximum semantic candidate request cap.
    pub max_semantic: usize,
    /// Maximum combined candidate set size before fusion.
    pub max_combined: usize,
}

impl Default for CandidateBudgetConfig {
    fn default() -> Self {
        Self {
            // Mirrors existing design docs and keeps headroom for downstream RRF/rerank stages.
            hybrid_lexical_bps: 300,
            hybrid_semantic_bps: 400,
            auto_lexical_bps: 300,
            auto_semantic_bps: 300,
            lexical_fallback_bps: 400,
            min_lexical: 20,
            min_semantic: 20,
            max_lexical: 1_000,
            max_semantic: 1_000,
            max_combined: 2_000,
        }
    }
}

/// Candidate limits allocated to each retrieval stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateBudget {
    /// Lexical retrieval limit.
    pub lexical_limit: usize,
    /// Semantic retrieval limit.
    pub semantic_limit: usize,
    /// Combined candidate cap before fusion/rerank.
    pub combined_limit: usize,
}

/// Action chosen by the decision-theoretic budget controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateBudgetAction {
    /// Favor lexical recall/precision and limit semantic spend.
    LexicalDominant,
    /// Keep lexical and semantic pools close to parity.
    Balanced,
    /// Favor semantic retrieval for natural-language intent.
    SemanticDominant,
    /// Disable semantic candidate retrieval.
    LexicalOnly,
}

/// Posterior belief over coarse query-intent states.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CandidateStatePosterior {
    /// Probability query intent is identifier-driven.
    pub identifier: f64,
    /// Probability query intent is short keyword-driven.
    pub short_keyword: f64,
    /// Probability query intent is natural-language driven.
    pub natural_language: f64,
    /// Probability query intent is effectively empty.
    pub empty: f64,
}

impl CandidateStatePosterior {
    fn normalized(identifier: f64, short_keyword: f64, natural_language: f64, empty: f64) -> Self {
        let total = identifier + short_keyword + natural_language + empty;
        if total <= f64::EPSILON {
            return Self {
                identifier: 0.25,
                short_keyword: 0.25,
                natural_language: 0.25,
                empty: 0.25,
            };
        }
        Self {
            identifier: identifier / total,
            short_keyword: short_keyword / total,
            natural_language: natural_language / total,
            empty: empty / total,
        }
    }
}

/// Expected loss for one candidate-budget action.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CandidateActionLoss {
    /// Action being evaluated.
    pub action: CandidateBudgetAction,
    /// Expected loss under the current posterior.
    pub expected_loss: f64,
}

/// Explainable decision payload for budget derivation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateBudgetDecision {
    /// Retrieval mode requested by caller.
    pub mode: CandidateMode,
    /// Query class used by orchestration.
    pub query_class: QueryClass,
    /// Posterior over intent states used for expected-loss minimization.
    pub posterior: CandidateStatePosterior,
    /// Expected loss for each candidate action.
    pub action_losses: [CandidateActionLoss; 4],
    /// Action with minimum expected loss.
    pub chosen_action: CandidateBudgetAction,
    /// Minimum expected loss achieved by `chosen_action`.
    pub chosen_expected_loss: f64,
}

/// Full derivation payload: budget plus decision explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateBudgetDerivation {
    /// Derived budget used by orchestration.
    pub budget: CandidateBudget,
    /// Decision evidence that explains why this budget was chosen.
    pub decision: CandidateBudgetDecision,
}

impl CandidateBudget {
    /// Derive stage budgets from request limit, mode, query class, and config.
    #[must_use]
    pub fn derive(
        requested_limit: usize,
        mode: CandidateMode,
        query_class: QueryClass,
        config: CandidateBudgetConfig,
    ) -> Self {
        Self::derive_with_decision(requested_limit, mode, query_class, config).budget
    }

    /// Derive stage budgets and an explainable decision payload.
    #[must_use]
    pub fn derive_with_decision(
        requested_limit: usize,
        mode: CandidateMode,
        query_class: QueryClass,
        config: CandidateBudgetConfig,
    ) -> CandidateBudgetDerivation {
        const SCALE: u64 = 100;
        let requested_limit = requested_limit.clamp(1, 1_000);
        let decision = derive_budget_decision(mode, query_class);

        let (base_lexical_bps, base_semantic_bps) = match mode {
            CandidateMode::Hybrid => (config.hybrid_lexical_bps, config.hybrid_semantic_bps),
            CandidateMode::Auto => (config.auto_lexical_bps, config.auto_semantic_bps),
            CandidateMode::LexicalFallback => (config.lexical_fallback_bps, 0),
        };

        let (class_lexical_bps, class_semantic_bps) = match query_class {
            QueryClass::Identifier => (150_u32, 50_u32),
            QueryClass::ShortKeyword => (125_u32, 75_u32),
            QueryClass::NaturalLanguage => (90_u32, 135_u32),
            QueryClass::Empty => (100_u32, 0_u32),
        };

        let lexical_raw =
            scaled_ceil_limit(requested_limit, base_lexical_bps, class_lexical_bps, SCALE);
        let semantic_raw = scaled_ceil_limit(
            requested_limit,
            base_semantic_bps,
            class_semantic_bps,
            SCALE,
        );

        let lexical_limit = lexical_raw.clamp(config.min_lexical, config.max_lexical);

        let semantic_limit = if base_semantic_bps == 0 || class_semantic_bps == 0 {
            0
        } else {
            semantic_raw.clamp(config.min_semantic, config.max_semantic)
        };

        let combined_limit = requested_limit
            .max(lexical_limit.saturating_add(semantic_limit))
            .min(config.max_combined);

        CandidateBudgetDerivation {
            budget: Self {
                lexical_limit,
                semantic_limit,
                combined_limit,
            },
            decision,
        }
    }
}

fn derive_budget_decision(mode: CandidateMode, query_class: QueryClass) -> CandidateBudgetDecision {
    let posterior = derive_state_posterior(mode, query_class);
    let action_losses = evaluate_action_losses(posterior);
    let chosen = action_losses
        .iter()
        .copied()
        .min_by(|left, right| {
            left.expected_loss
                .partial_cmp(&right.expected_loss)
                .unwrap_or(Ordering::Equal)
        })
        .unwrap_or(action_losses[0]);

    CandidateBudgetDecision {
        mode,
        query_class,
        posterior,
        action_losses,
        chosen_action: chosen.action,
        chosen_expected_loss: chosen.expected_loss,
    }
}

fn derive_state_posterior(mode: CandidateMode, query_class: QueryClass) -> CandidateStatePosterior {
    let mut identifier = 1.0;
    let mut short_keyword = 1.0;
    let mut natural_language = 1.0;
    let mut empty = 1.0;

    match query_class {
        QueryClass::Identifier => {
            identifier += 8.0;
            short_keyword += 1.0;
            natural_language += 0.2;
        }
        QueryClass::ShortKeyword => {
            short_keyword += 7.0;
            identifier += 1.2;
            natural_language += 0.6;
        }
        QueryClass::NaturalLanguage => {
            natural_language += 8.0;
            short_keyword += 0.8;
            identifier += 0.4;
        }
        QueryClass::Empty => {
            empty += 9.0;
        }
    }

    match mode {
        CandidateMode::Hybrid => {
            natural_language += 0.8;
            short_keyword += 0.2;
        }
        CandidateMode::Auto => {
            short_keyword += 0.4;
            natural_language += 0.4;
            identifier += 0.2;
        }
        CandidateMode::LexicalFallback => {
            identifier += 0.8;
            short_keyword += 0.6;
            empty += 0.4;
        }
    }

    CandidateStatePosterior::normalized(identifier, short_keyword, natural_language, empty)
}

fn evaluate_action_losses(posterior: CandidateStatePosterior) -> [CandidateActionLoss; 4] {
    [
        CandidateActionLoss {
            action: CandidateBudgetAction::LexicalDominant,
            expected_loss: expected_loss(CandidateBudgetAction::LexicalDominant, posterior),
        },
        CandidateActionLoss {
            action: CandidateBudgetAction::Balanced,
            expected_loss: expected_loss(CandidateBudgetAction::Balanced, posterior),
        },
        CandidateActionLoss {
            action: CandidateBudgetAction::SemanticDominant,
            expected_loss: expected_loss(CandidateBudgetAction::SemanticDominant, posterior),
        },
        CandidateActionLoss {
            action: CandidateBudgetAction::LexicalOnly,
            expected_loss: expected_loss(CandidateBudgetAction::LexicalOnly, posterior),
        },
    ]
}

fn expected_loss(action: CandidateBudgetAction, posterior: CandidateStatePosterior) -> f64 {
    match action {
        CandidateBudgetAction::LexicalDominant => posterior.empty.mul_add(
            2.0,
            posterior.natural_language.mul_add(
                7.0,
                posterior
                    .identifier
                    .mul_add(1.0, posterior.short_keyword * 2.0),
            ),
        ),
        CandidateBudgetAction::Balanced => posterior.empty.mul_add(
            5.0,
            posterior.natural_language.mul_add(
                2.0,
                posterior
                    .identifier
                    .mul_add(3.0, posterior.short_keyword * 1.0),
            ),
        ),
        CandidateBudgetAction::SemanticDominant => posterior.empty.mul_add(
            8.0,
            posterior.natural_language.mul_add(
                1.0,
                posterior
                    .identifier
                    .mul_add(10.0, posterior.short_keyword * 4.0),
            ),
        ),
        CandidateBudgetAction::LexicalOnly => posterior.empty.mul_add(
            1.0,
            posterior.natural_language.mul_add(
                9.0,
                posterior
                    .identifier
                    .mul_add(2.0, posterior.short_keyword * 5.0),
            ),
        ),
    }
}

fn scaled_ceil_limit(
    requested_limit: usize,
    base_multiplier: u32,
    class_multiplier: u32,
    scale: u64,
) -> usize {
    let requested = u64::try_from(requested_limit).unwrap_or(u64::MAX);
    let numerator = requested
        .saturating_mul(u64::from(base_multiplier))
        .saturating_mul(u64::from(class_multiplier));
    let denominator = scale.saturating_mul(scale).max(1);
    let rounded_up = numerator
        .saturating_add(denominator.saturating_sub(1))
        .saturating_div(denominator);
    usize::try_from(rounded_up).unwrap_or(usize::MAX)
}

/// A candidate hit produced by a retrieval stage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CandidateHit {
    /// Document identifier.
    pub doc_id: i64,
    /// Stage-local score.
    pub score: f64,
}

impl CandidateHit {
    /// Construct a new candidate hit.
    #[must_use]
    pub const fn new(doc_id: i64, score: f64) -> Self {
        Self { doc_id, score }
    }
}

/// Retrieval source that first introduced a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    Lexical,
    Semantic,
}

/// Deduped candidate prepared for downstream fusion/reranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedCandidate {
    /// Document identifier.
    pub doc_id: i64,
    /// Lexical rank (1-based) if present.
    pub lexical_rank: Option<usize>,
    /// Semantic rank (1-based) if present.
    pub semantic_rank: Option<usize>,
    /// Lexical score if present.
    pub lexical_score: Option<f64>,
    /// Semantic score if present.
    pub semantic_score: Option<f64>,
    /// Which source first introduced this candidate.
    pub first_source: CandidateSource,
}

impl PreparedCandidate {
    fn best_rank(&self) -> usize {
        self.lexical_rank
            .into_iter()
            .chain(self.semantic_rank)
            .min()
            .unwrap_or(usize::MAX)
    }
}

/// Accounting counters for the orchestration stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CandidateStageCounts {
    /// Raw lexical candidates provided.
    pub lexical_considered: usize,
    /// Raw semantic candidates provided.
    pub semantic_considered: usize,
    /// Lexical candidates kept after budgeting.
    pub lexical_selected: usize,
    /// Semantic candidates kept after budgeting.
    pub semantic_selected: usize,
    /// Deduped candidates emitted.
    pub deduped_selected: usize,
    /// Number of removed duplicates.
    pub duplicates_removed: usize,
}

/// Deterministic orchestration output ready for fusion/rerank stages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidatePreparation {
    /// Budget used to trim source pools.
    pub budget: CandidateBudget,
    /// Stage accounting metrics.
    pub counts: CandidateStageCounts,
    /// Deterministically ordered deduped candidates.
    pub candidates: Vec<PreparedCandidate>,
}

/// Prepare lexical + semantic candidate pools for downstream fusion/reranking.
///
/// Rules:
/// - Trim each source by its budget.
/// - Merge by `doc_id`.
/// - Preserve source-specific rank/score.
/// - Emit deterministic ordering independent of hash-map iteration.
#[must_use]
pub fn prepare_candidates(
    lexical_hits: &[CandidateHit],
    semantic_hits: &[CandidateHit],
    budget: CandidateBudget,
) -> CandidatePreparation {
    let lexical_trimmed = lexical_hits
        .iter()
        .copied()
        .take(budget.lexical_limit)
        .collect::<Vec<_>>();
    let semantic_trimmed = semantic_hits
        .iter()
        .copied()
        .take(budget.semantic_limit)
        .collect::<Vec<_>>();

    let mut map: BTreeMap<i64, PreparedCandidate> = BTreeMap::new();

    for (idx, hit) in lexical_trimmed.iter().enumerate() {
        map.entry(hit.doc_id)
            .and_modify(|candidate| {
                candidate.lexical_rank = Some(idx + 1);
                candidate.lexical_score = Some(hit.score);
            })
            .or_insert(PreparedCandidate {
                doc_id: hit.doc_id,
                lexical_rank: Some(idx + 1),
                semantic_rank: None,
                lexical_score: Some(hit.score),
                semantic_score: None,
                first_source: CandidateSource::Lexical,
            });
    }

    for (idx, hit) in semantic_trimmed.iter().enumerate() {
        map.entry(hit.doc_id)
            .and_modify(|candidate| {
                candidate.semantic_rank = Some(idx + 1);
                candidate.semantic_score = Some(hit.score);
            })
            .or_insert(PreparedCandidate {
                doc_id: hit.doc_id,
                lexical_rank: None,
                semantic_rank: Some(idx + 1),
                lexical_score: None,
                semantic_score: Some(hit.score),
                first_source: CandidateSource::Semantic,
            });
    }

    let mut candidates = map.into_values().collect::<Vec<_>>();
    candidates.sort_by(prepared_candidate_cmp);
    candidates.truncate(budget.combined_limit);

    let selected_total = lexical_trimmed.len().saturating_add(semantic_trimmed.len());
    let deduped_selected = candidates.len();
    let duplicates_removed = selected_total.saturating_sub(deduped_selected);
    let counts = CandidateStageCounts {
        lexical_considered: lexical_hits.len(),
        semantic_considered: semantic_hits.len(),
        lexical_selected: lexical_trimmed.len(),
        semantic_selected: semantic_trimmed.len(),
        deduped_selected,
        duplicates_removed,
    };

    CandidatePreparation {
        budget,
        counts,
        candidates,
    }
}

fn rank_or_max(rank: Option<usize>) -> usize {
    rank.unwrap_or(usize::MAX)
}

fn score_cmp_desc(a: Option<f64>, b: Option<f64>) -> Ordering {
    b.unwrap_or(f64::NEG_INFINITY)
        .partial_cmp(&a.unwrap_or(f64::NEG_INFINITY))
        .unwrap_or(Ordering::Equal)
}

fn prepared_candidate_cmp(left: &PreparedCandidate, right: &PreparedCandidate) -> Ordering {
    left.best_rank()
        .cmp(&right.best_rank())
        .then_with(|| rank_or_max(left.lexical_rank).cmp(&rank_or_max(right.lexical_rank)))
        .then_with(|| rank_or_max(left.semantic_rank).cmp(&rank_or_max(right.semantic_rank)))
        .then_with(|| score_cmp_desc(left.lexical_score, right.lexical_score))
        .then_with(|| score_cmp_desc(left.semantic_score, right.semantic_score))
        .then_with(|| left.doc_id.cmp(&right.doc_id))
}

#[cfg(test)]
mod tests {
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
        assert!(prepared.candidates.is_empty());
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
        assert_eq!(
            derivation.decision.query_class,
            derivation2.decision.query_class
        );
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
}
