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
use std::collections::HashMap;

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
///
/// A zero mode-specific multiplier or lane maximum disables that lane.
/// Reallocation only boosts a lane that was already enabled. Hard maxima win
/// over minima, including when a caller supplies inverted bounds.
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
    /// Maximum combined candidate set size before fusion. Zero disables retrieval.
    /// Positive values cap the merged output, not each source's overfetch.
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
    ///
    /// Reallocation never revives a disabled lane or shrinks a healthy lane.
    /// Lexical fallback and empty queries cannot allocate semantic candidates.
    #[must_use]
    pub fn derive_with_decision(
        requested_limit: usize,
        mode: CandidateMode,
        query_class: QueryClass,
        config: CandidateBudgetConfig,
    ) -> CandidateBudgetDerivation {
        const SCALE: u64 = 100;
        let requested_limit = requested_limit.clamp(1, 100_000);
        let decision = derive_budget_decision(mode, query_class);
        if config.max_combined == 0 {
            return CandidateBudgetDerivation {
                budget: Self {
                    lexical_limit: 0,
                    semantic_limit: 0,
                    combined_limit: 0,
                },
                decision,
            };
        }

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

        let mut lexical_limit = clamp_candidate_limit(
            scaled_ceil_limit(requested_limit, base_lexical_bps, class_lexical_bps, SCALE),
            config.min_lexical,
            config.max_lexical,
        );
        let mut semantic_limit = clamp_candidate_limit(
            scaled_ceil_limit(
                requested_limit,
                base_semantic_bps,
                class_semantic_bps,
                SCALE,
            ),
            config.min_semantic,
            config.max_semantic,
        );

        // A single healthy lane may use its fallback multiplier for extra
        // recall. Neither a zero multiplier nor a zero cap is permission to
        // turn on a different lane, and a smaller fallback cannot shrink it.
        if semantic_limit == 0 && lexical_limit > 0 {
            lexical_limit = lexical_limit.max(clamp_candidate_limit(
                scaled_ceil_limit(
                    requested_limit,
                    config.lexical_fallback_bps,
                    class_lexical_bps,
                    SCALE,
                ),
                config.min_lexical,
                config.max_lexical,
            ));
        }
        if lexical_limit == 0 && semantic_limit > 0 {
            semantic_limit = semantic_limit.max(clamp_candidate_limit(
                scaled_ceil_limit(
                    requested_limit,
                    config.hybrid_semantic_bps,
                    class_semantic_bps,
                    SCALE,
                ),
                config.min_semantic,
                config.max_semantic,
            ));
        }

        let budget = Self {
            lexical_limit,
            semantic_limit,
            combined_limit: lexical_limit
                .saturating_add(semantic_limit)
                .min(config.max_combined),
        };

        CandidateBudgetDerivation { budget, decision }
    }
}

/// Hard caps take precedence over minima; zero work stays zero. Normalizing
/// the lower bound also prevents user-supplied inverted bounds from panicking.
fn clamp_candidate_limit(limit: usize, minimum: usize, maximum: usize) -> usize {
    if limit == 0 || maximum == 0 {
        0
    } else {
        limit.clamp(minimum.min(maximum), maximum)
    }
}

fn derive_budget_decision(mode: CandidateMode, query_class: QueryClass) -> CandidateBudgetDecision {
    let posterior = derive_state_posterior(mode, query_class);
    let action_losses = evaluate_action_losses(posterior);
    let chosen = action_losses
        .iter()
        .copied()
        .min_by(|left, right| left.expected_loss.total_cmp(&right.expected_loss))
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
    /// Repeated document IDs removed before applying the combined cap.
    /// Does not include unique candidates discarded by that cap.
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
/// - Trim each source by its budget; sources arrive in rank order.
/// - Merge by `doc_id`.
/// - Preserve the first (best) rank and its corresponding score per source.
/// - Emit deterministic ordering independent of hash-map iteration.
#[must_use]
pub fn prepare_candidates(
    lexical_hits: &[CandidateHit],
    semantic_hits: &[CandidateHit],
    budget: CandidateBudget,
) -> CandidatePreparation {
    // Borrow the selected prefixes rather than copying both source pools.
    let lexical_trimmed = &lexical_hits[..lexical_hits.len().min(budget.lexical_limit)];
    let semantic_trimmed = &semantic_hits[..semantic_hits.len().min(budget.semantic_limit)];

    let mut map: HashMap<i64, PreparedCandidate> =
        HashMap::with_capacity(lexical_trimmed.len().saturating_add(semantic_trimmed.len()));

    for (idx, hit) in lexical_trimmed.iter().enumerate() {
        // A later occurrence must not demote this document or attach a score
        // from a different occurrence to the winning rank.
        map.entry(hit.doc_id).or_insert(PreparedCandidate {
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
                if candidate.semantic_rank.is_none() {
                    candidate.semantic_rank = Some(idx + 1);
                    candidate.semantic_score = Some(hit.score);
                }
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

    let selected_total = lexical_trimmed.len().saturating_add(semantic_trimmed.len());
    // Count duplicates before truncation, not unique documents lost to a cap.
    let duplicates_removed = selected_total.saturating_sub(map.len());
    let mut candidates = map.into_values().collect::<Vec<_>>();
    candidates.sort_by(prepared_candidate_cmp);
    candidates.truncate(budget.combined_limit);

    let counts = CandidateStageCounts {
        lexical_considered: lexical_hits.len(),
        semantic_considered: semantic_hits.len(),
        lexical_selected: lexical_trimmed.len(),
        semantic_selected: semantic_trimmed.len(),
        deduped_selected: candidates.len(),
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
        .total_cmp(&a.unwrap_or(f64::NEG_INFINITY))
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
mod budget_tests;
#[cfg(test)]
mod regression_tests;
#[cfg(test)]
mod tests;
