//! Thread/sender diversity controls for post-fusion result ranking.
//!
//! After RRF fusion (and optional reranking), top results can collapse to a
//! single thread or sender when relevance scores are near-tied. This module
//! provides a greedy diversification pass that enforces configurable caps on
//! per-thread and per-sender result density while preserving overall relevance
//! ordering.
//!
//! The algorithm:
//! 1. Walk the fused hits in score-descending order.
//! 2. For each position, pick the highest-scoring unplaced hit that does not
//!    exceed thread or sender caps within the diversity window.
//! 3. If all remaining candidates violate caps, take the highest-scored one
//!    anyway (relevance always wins over diversity for the last slot).
//! 4. Tie-breaking is deterministic: original position (which was already
//!    deterministically sorted by `fused_hit_cmp`).

use std::collections::HashMap;
use std::hash::BuildHasher;

use serde::{Deserialize, Serialize};

use mcp_agent_mail_search_core::fusion::FusedHit;

// ── Environment variable names ────────────────────────────────────────────

/// Env var to enable/disable diversity controls entirely.
pub const DIVERSITY_ENABLED_ENV: &str = "AM_SEARCH_DIVERSITY_ENABLED";

/// Env var for maximum results from the same thread within the diversity window.
pub const DIVERSITY_MAX_PER_THREAD_ENV: &str = "AM_SEARCH_DIVERSITY_MAX_PER_THREAD";

/// Env var for maximum results from the same sender within the diversity window.
pub const DIVERSITY_MAX_PER_SENDER_ENV: &str = "AM_SEARCH_DIVERSITY_MAX_PER_SENDER";

/// Env var for score tolerance below which diversity kicks in.
/// Two hits within this RRF score delta are considered "near-tied".
pub const DIVERSITY_SCORE_TOLERANCE_ENV: &str = "AM_SEARCH_DIVERSITY_SCORE_TOLERANCE";

/// Env var for the number of top results where diversity is enforced.
pub const DIVERSITY_WINDOW_SIZE_ENV: &str = "AM_SEARCH_DIVERSITY_WINDOW_SIZE";

// ── Defaults ──────────────────────────────────────────────────────────────

const DEFAULT_MAX_PER_THREAD: usize = 3;
const DEFAULT_MAX_PER_SENDER: usize = 5;
const DEFAULT_SCORE_TOLERANCE: f64 = 0.001;
const DEFAULT_WINDOW_SIZE: usize = 20;

// ── Configuration ─────────────────────────────────────────────────────────

/// Configuration for post-fusion diversity controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiversityConfig {
    /// Whether diversity controls are enabled.
    pub enabled: bool,
    /// Maximum results from the same thread within the diversity window.
    pub max_per_thread: usize,
    /// Maximum results from the same sender within the diversity window.
    pub max_per_sender: usize,
    /// Score tolerance: two hits within this RRF score delta are "near-tied"
    /// and eligible for diversity reordering. Hits with a clear score gap
    /// above this threshold are never demoted.
    pub score_tolerance: f64,
    /// Number of top results where diversity constraints are enforced.
    /// Results beyond this position pass through unmodified.
    pub window_size: usize,
}

impl Default for DiversityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_thread: DEFAULT_MAX_PER_THREAD,
            max_per_sender: DEFAULT_MAX_PER_SENDER,
            score_tolerance: DEFAULT_SCORE_TOLERANCE,
            window_size: DEFAULT_WINDOW_SIZE,
        }
    }
}

impl DiversityConfig {
    /// Load diversity config from environment variables, falling back to defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = std::env::var(DIVERSITY_ENABLED_ENV).ok().is_none_or(|v| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        });

        let max_per_thread = std::env::var(DIVERSITY_MAX_PER_THREAD_ENV)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_MAX_PER_THREAD);

        let max_per_sender = std::env::var(DIVERSITY_MAX_PER_SENDER_ENV)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_MAX_PER_SENDER);

        let score_tolerance = std::env::var(DIVERSITY_SCORE_TOLERANCE_ENV)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&v| v >= 0.0)
            .unwrap_or(DEFAULT_SCORE_TOLERANCE);

        let window_size = std::env::var(DIVERSITY_WINDOW_SIZE_ENV)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_WINDOW_SIZE);

        Self {
            enabled,
            max_per_thread,
            max_per_sender,
            score_tolerance,
            window_size,
        }
    }
}

// ── Per-document metadata ─────────────────────────────────────────────────

/// Metadata about a document used for diversity decisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DiversityMeta {
    /// Thread ID this document belongs to, if any.
    pub thread_id: Option<String>,
    /// Sender agent name, if any.
    pub sender: Option<String>,
}

// ── Result ────────────────────────────────────────────────────────────────

/// Result of the diversity pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiversityResult {
    /// Reordered fused hits with diversity applied.
    pub hits: Vec<FusedHit>,
    /// Number of hits that were demoted (moved later) for diversity.
    pub demotions: usize,
    /// Configuration used.
    pub config: DiversityConfig,
}

// ── Algorithm ─────────────────────────────────────────────────────────────

/// Apply thread/sender diversity controls to fused search results.
///
/// The function expects `hits` to be pre-sorted by relevance (score descending,
/// as produced by `fuse_rrf`). It reorders only within the diversity window,
/// leaving tail results in their original relative order.
///
/// # Arguments
/// * `hits` - Fused hits sorted by relevance (score descending).
/// * `metadata` - Per-document thread/sender metadata keyed by `doc_id`.
/// * `config` - Diversity configuration.
///
/// # Returns
/// `DiversityResult` with reordered hits and demotion count.
#[must_use]
pub fn diversify<S: BuildHasher>(
    hits: Vec<FusedHit>,
    metadata: &HashMap<i64, DiversityMeta, S>,
    config: &DiversityConfig,
) -> DiversityResult {
    if !config.enabled || hits.is_empty() {
        return DiversityResult {
            hits,
            demotions: 0,
            config: config.clone(),
        };
    }

    let window = config.window_size.min(hits.len());
    let (window_hits, tail_hits) = hits.split_at(window);

    // Track which window candidates have been placed.
    let mut placed = vec![false; window];
    let mut result: Vec<FusedHit> = Vec::with_capacity(hits.len());

    // Per-thread and per-sender counts within placed results.
    let mut thread_counts: HashMap<&str, usize> = HashMap::new();
    let mut sender_counts: HashMap<&str, usize> = HashMap::new();

    let mut demotions: usize = 0;

    for position in 0..window {
        let mut best_violating_idx: Option<usize> = None;
        let mut best_valid_idx: Option<usize> = None;

        for (idx, hit) in window_hits.iter().enumerate() {
            if placed[idx] {
                continue;
            }

            let meta = metadata.get(&hit.doc_id);
            let would_violate = would_violate_caps(meta, &thread_counts, &sender_counts, config);

            if would_violate {
                if best_violating_idx.is_none() {
                    best_violating_idx = Some(idx);
                }
            } else {
                best_valid_idx = Some(idx);
                break; // Found the best valid candidate
            }
        }

        let idx_to_take = match (best_violating_idx, best_valid_idx) {
            (Some(v_idx), Some(valid_idx)) => {
                let v_score = window_hits[v_idx].rrf_score;
                let valid_score = window_hits[valid_idx].rrf_score;
                // A violating hit is eligible for diversity demotion only if it's "near-tied"
                // with the next valid hit. Hits with a clear lead are never demoted.
                if (v_score - valid_score).abs() <= config.score_tolerance {
                    valid_idx
                } else {
                    v_idx
                }
            }
            (Some(v_idx), None) => v_idx,
            (None, Some(valid_idx)) => valid_idx,
            (None, None) => break,
        };

        placed[idx_to_take] = true;
        let hit = &window_hits[idx_to_take];
        let meta = metadata.get(&hit.doc_id);

        // Update counts
        if let Some(m) = meta {
            if let Some(ref tid) = m.thread_id {
                *thread_counts.entry(tid.as_str()).or_insert(0) += 1;
            }
            if let Some(ref s) = m.sender {
                *sender_counts.entry(s.as_str()).or_insert(0) += 1;
            }
        }

        // Track if this was a demotion (placed out of original order)
        if idx_to_take != position {
            demotions += 1;
        }

        result.push(hit.clone());
    }

    // Append any unplaced window hits (shouldn't happen, but safety)
    for (idx, hit) in window_hits.iter().enumerate() {
        if !placed[idx] {
            result.push(hit.clone());
        }
    }

    // Append tail hits unchanged
    result.extend(tail_hits.iter().cloned());

    DiversityResult {
        hits: result,
        demotions,
        config: config.clone(),
    }
}

/// Check if placing a document with the given metadata would violate caps.
fn would_violate_caps(
    meta: Option<&DiversityMeta>,
    thread_counts: &HashMap<&str, usize>,
    sender_counts: &HashMap<&str, usize>,
    config: &DiversityConfig,
) -> bool {
    let Some(meta) = meta else {
        return false; // No metadata = no constraints
    };

    if let Some(ref tid) = meta.thread_id {
        let count = thread_counts.get(tid.as_str()).copied().unwrap_or(0);
        if count >= config.max_per_thread {
            return true;
        }
    }

    if let Some(ref sender) = meta.sender {
        let count = sender_counts.get(sender.as_str()).copied().unwrap_or(0);
        if count >= config.max_per_sender {
            return true;
        }
    }

    false
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use mcp_agent_mail_search_core::fusion::FusionExplain;
    use mcp_agent_mail_search_core::hybrid_candidates::CandidateSource;

    fn make_hit(doc_id: i64, score: f64) -> FusedHit {
        FusedHit {
            doc_id,
            rrf_score: score,
            first_source: CandidateSource::Lexical,
            explain: FusionExplain {
                lexical_rank: Some(1),
                lexical_score: Some(score),
                semantic_rank: None,
                semantic_score: None,
                rrf_score: score,
                source_contributions: vec![],
            },
        }
    }

    fn make_meta(thread_id: Option<&str>, sender: Option<&str>) -> DiversityMeta {
        DiversityMeta {
            thread_id: thread_id.map(String::from),
            sender: sender.map(String::from),
        }
    }

    // ── Disabled / passthrough ────────────────────────────────────────

    #[test]
    fn disabled_returns_unchanged() {
        let hits = vec![make_hit(1, 0.9), make_hit(2, 0.8)];
        let meta = HashMap::new();
        let config = DiversityConfig {
            enabled: false,
            ..Default::default()
        };

        let result = diversify(hits, &meta, &config);
        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.hits[0].doc_id, 1);
        assert_eq!(result.hits[1].doc_id, 2);
        assert_eq!(result.demotions, 0);
    }

    #[test]
    fn empty_hits_returns_empty() {
        let result = diversify(Vec::new(), &HashMap::new(), &DiversityConfig::default());
        assert!(result.hits.is_empty());
        assert_eq!(result.demotions, 0);
    }

    #[test]
    fn single_hit_unchanged() {
        let hits = vec![make_hit(1, 0.9)];
        let result = diversify(hits, &HashMap::new(), &DiversityConfig::default());
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].doc_id, 1);
    }

    // ── Thread diversity ──────────────────────────────────────────────

    #[test]
    fn thread_cap_demotes_excess() {
        // 5 hits from same thread, near-tied scores, max_per_thread=2
        let hits = vec![
            make_hit(1, 0.030),
            make_hit(2, 0.030),
            make_hit(3, 0.030),
            make_hit(4, 0.030),
            make_hit(5, 0.029), // different thread
        ];
        let mut meta = HashMap::new();
        meta.insert(1, make_meta(Some("thread-A"), Some("alice")));
        meta.insert(2, make_meta(Some("thread-A"), Some("alice")));
        meta.insert(3, make_meta(Some("thread-A"), Some("alice")));
        meta.insert(4, make_meta(Some("thread-A"), Some("alice")));
        meta.insert(5, make_meta(Some("thread-B"), Some("bob")));

        let config = DiversityConfig {
            enabled: true,
            max_per_thread: 2,
            max_per_sender: 100, // effectively unlimited
            score_tolerance: 0.01,
            window_size: 5,
        };

        let result = diversify(hits, &meta, &config);
        assert_eq!(result.hits.len(), 5);

        // Doc 5 (thread-B) should be promoted within the window
        // because docs from thread-A exceed the cap
        let thread_a_in_top3: usize = result.hits[..3]
            .iter()
            .filter(|h| {
                meta.get(&h.doc_id).and_then(|m| m.thread_id.as_deref()) == Some("thread-A")
            })
            .count();
        assert!(
            thread_a_in_top3 <= 2,
            "Expected at most 2 thread-A docs in top 3, got {thread_a_in_top3}"
        );
    }

    // ── Sender diversity ──────────────────────────────────────────────

    #[test]
    fn sender_cap_demotes_excess() {
        let hits = vec![
            make_hit(1, 0.030),
            make_hit(2, 0.030),
            make_hit(3, 0.030),
            make_hit(4, 0.029), // different sender
        ];
        let mut meta = HashMap::new();
        meta.insert(1, make_meta(None, Some("alice")));
        meta.insert(2, make_meta(None, Some("alice")));
        meta.insert(3, make_meta(None, Some("alice")));
        meta.insert(4, make_meta(None, Some("bob")));

        let config = DiversityConfig {
            enabled: true,
            max_per_thread: 100,
            max_per_sender: 2,
            score_tolerance: 0.01,
            window_size: 4,
        };

        let result = diversify(hits, &meta, &config);
        let alice_in_top3: usize = result.hits[..3]
            .iter()
            .filter(|h| meta.get(&h.doc_id).and_then(|m| m.sender.as_deref()) == Some("alice"))
            .count();
        assert!(
            alice_in_top3 <= 2,
            "Expected at most 2 alice docs in top 3, got {alice_in_top3}"
        );
    }

    // ── Score gap protection ──────────────────────────────────────────

    #[test]
    fn clear_score_gap_prevents_demotion() {
        // Doc 1 has a clear score lead — should never be demoted even if
        // it violates thread cap
        let hits = vec![
            make_hit(1, 0.500), // clear leader
            make_hit(2, 0.030), // near-tied cluster
            make_hit(3, 0.030),
            make_hit(4, 0.029),
        ];
        let mut meta = HashMap::new();
        meta.insert(1, make_meta(Some("thread-A"), None));
        meta.insert(2, make_meta(Some("thread-A"), None));
        meta.insert(3, make_meta(Some("thread-A"), None));
        meta.insert(4, make_meta(Some("thread-B"), None));

        let config = DiversityConfig {
            enabled: true,
            max_per_thread: 1,
            max_per_sender: 100,
            score_tolerance: 0.01,
            window_size: 4,
        };

        let result = diversify(hits, &meta, &config);
        // Doc 1 must remain first — score gap protects it
        assert_eq!(result.hits[0].doc_id, 1);
    }

    // ── All unique — no demotions ─────────────────────────────────────

    #[test]
    fn all_unique_threads_no_demotions() {
        let hits = vec![make_hit(1, 0.030), make_hit(2, 0.029), make_hit(3, 0.028)];
        let mut meta = HashMap::new();
        meta.insert(1, make_meta(Some("t1"), Some("alice")));
        meta.insert(2, make_meta(Some("t2"), Some("bob")));
        meta.insert(3, make_meta(Some("t3"), Some("charlie")));

        let result = diversify(hits, &meta, &DiversityConfig::default());
        assert_eq!(result.hits[0].doc_id, 1);
        assert_eq!(result.hits[1].doc_id, 2);
        assert_eq!(result.hits[2].doc_id, 3);
        assert_eq!(result.demotions, 0);
    }

    // ── No metadata — passthrough ─────────────────────────────────────

    #[test]
    fn no_metadata_passthrough() {
        let hits = vec![make_hit(1, 0.030), make_hit(2, 0.029), make_hit(3, 0.028)];
        let result = diversify(hits, &HashMap::new(), &DiversityConfig::default());
        assert_eq!(result.hits[0].doc_id, 1);
        assert_eq!(result.hits[1].doc_id, 2);
        assert_eq!(result.hits[2].doc_id, 3);
        assert_eq!(result.demotions, 0);
    }

    // ── Window size ───────────────────────────────────────────────────

    #[test]
    fn window_size_limits_diversity_scope() {
        // 6 hits, window_size=3 — only top 3 get diversity treatment
        let hits = vec![
            make_hit(1, 0.030),
            make_hit(2, 0.030),
            make_hit(3, 0.030),
            make_hit(4, 0.030), // outside window
            make_hit(5, 0.030),
            make_hit(6, 0.029),
        ];
        let mut meta = HashMap::new();
        for id in 1..=6 {
            meta.insert(id, make_meta(Some("same-thread"), None));
        }

        let config = DiversityConfig {
            enabled: true,
            max_per_thread: 1,
            max_per_sender: 100,
            score_tolerance: 0.01,
            window_size: 3,
        };

        let result = diversify(hits, &meta, &config);
        assert_eq!(result.hits.len(), 6);
        // Tail (positions 3,4,5) should remain in original relative order
        assert_eq!(result.hits[3].doc_id, 4);
        assert_eq!(result.hits[4].doc_id, 5);
        assert_eq!(result.hits[5].doc_id, 6);
    }

    // ── Config from_env defaults ──────────────────────────────────────

    #[test]
    fn config_defaults() {
        let config = DiversityConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_per_thread, DEFAULT_MAX_PER_THREAD);
        assert_eq!(config.max_per_sender, DEFAULT_MAX_PER_SENDER);
        assert!((config.score_tolerance - DEFAULT_SCORE_TOLERANCE).abs() < f64::EPSILON);
        assert_eq!(config.window_size, DEFAULT_WINDOW_SIZE);
    }

    #[test]
    fn config_from_env_uses_defaults() {
        let config = DiversityConfig::from_env();
        assert!(config.max_per_thread > 0);
        assert!(config.max_per_sender > 0);
        assert!(config.score_tolerance >= 0.0);
        assert!(config.window_size > 0);
    }

    // ── Serde roundtrips ──────────────────────────────────────────────

    #[test]
    fn config_serde_roundtrip() {
        let config = DiversityConfig {
            enabled: false,
            max_per_thread: 7,
            max_per_sender: 3,
            score_tolerance: 0.005,
            window_size: 50,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: DiversityConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn meta_serde_roundtrip() {
        let meta = DiversityMeta {
            thread_id: Some("br-42".to_owned()),
            sender: Some("BlueLake".to_owned()),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: DiversityMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, meta);
    }

    #[test]
    fn result_serde_roundtrip() {
        let result = DiversityResult {
            hits: vec![make_hit(1, 0.5)],
            demotions: 1,
            config: DiversityConfig::default(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: DiversityResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.demotions, 1);
        assert_eq!(back.hits.len(), 1);
    }

    // ── Trait coverage ────────────────────────────────────────────────

    #[test]
    fn config_debug_clone() {
        let config = DiversityConfig::default();
        let debug = format!("{config:?}");
        assert!(debug.contains("DiversityConfig"));
        let cloned = config.clone();
        assert_eq!(cloned, config);
    }

    #[test]
    fn meta_debug_clone() {
        let meta = make_meta(Some("t1"), Some("alice"));
        let debug = format!("{meta:?}");
        assert!(debug.contains("DiversityMeta"));
        let cloned = meta.clone();
        assert_eq!(cloned, meta);
    }

    // ── Determinism ───────────────────────────────────────────────────

    #[test]
    fn deterministic_across_100_runs() {
        let hits: Vec<_> = (1_u32..=10)
            .map(|i| make_hit(i64::from(i), f64::from(i).mul_add(-0.0001, 0.030)))
            .collect();
        let mut meta = HashMap::new();
        for i in 1_i64..=10 {
            let thread = if i <= 5 { "thread-A" } else { "thread-B" };
            meta.insert(i, make_meta(Some(thread), Some("agent-x")));
        }

        let config = DiversityConfig {
            enabled: true,
            max_per_thread: 2,
            max_per_sender: 10,
            score_tolerance: 0.01,
            window_size: 10,
        };

        let first_result = diversify(hits.clone(), &meta, &config);
        let first_order: Vec<i64> = first_result.hits.iter().map(|h| h.doc_id).collect();

        for _ in 0..100 {
            let result = diversify(hits.clone(), &meta, &config);
            let order: Vec<i64> = result.hits.iter().map(|h| h.doc_id).collect();
            assert_eq!(order, first_order, "Ordering must be deterministic");
        }
    }

    // ── Env var constants ─────────────────────────────────────────────

    #[test]
    fn env_var_names() {
        assert_eq!(DIVERSITY_ENABLED_ENV, "AM_SEARCH_DIVERSITY_ENABLED");
        assert_eq!(
            DIVERSITY_MAX_PER_THREAD_ENV,
            "AM_SEARCH_DIVERSITY_MAX_PER_THREAD"
        );
        assert_eq!(
            DIVERSITY_MAX_PER_SENDER_ENV,
            "AM_SEARCH_DIVERSITY_MAX_PER_SENDER"
        );
        assert_eq!(
            DIVERSITY_SCORE_TOLERANCE_ENV,
            "AM_SEARCH_DIVERSITY_SCORE_TOLERANCE"
        );
        assert_eq!(DIVERSITY_WINDOW_SIZE_ENV, "AM_SEARCH_DIVERSITY_WINDOW_SIZE");
    }

    // ── Combined thread + sender caps ─────────────────────────────────

    #[test]
    fn combined_thread_and_sender_caps() {
        let hits = vec![
            make_hit(1, 0.030), // thread-A, alice
            make_hit(2, 0.030), // thread-A, alice
            make_hit(3, 0.030), // thread-A, bob
            make_hit(4, 0.030), // thread-B, alice
            make_hit(5, 0.029), // thread-B, bob
        ];
        let mut meta = HashMap::new();
        meta.insert(1, make_meta(Some("thread-A"), Some("alice")));
        meta.insert(2, make_meta(Some("thread-A"), Some("alice")));
        meta.insert(3, make_meta(Some("thread-A"), Some("bob")));
        meta.insert(4, make_meta(Some("thread-B"), Some("alice")));
        meta.insert(5, make_meta(Some("thread-B"), Some("bob")));

        let config = DiversityConfig {
            enabled: true,
            max_per_thread: 2,
            max_per_sender: 2,
            score_tolerance: 0.01,
            window_size: 5,
        };

        let result = diversify(hits, &meta, &config);
        assert_eq!(result.hits.len(), 5);

        // Count thread-A and alice in entire result
        let thread_a_count: usize = result
            .hits
            .iter()
            .filter(|h| {
                meta.get(&h.doc_id).and_then(|m| m.thread_id.as_deref()) == Some("thread-A")
            })
            .count();
        assert_eq!(thread_a_count, 3); // all 3 thread-A docs still present

        // But in the top 2, thread-A should be capped at 2
        let thread_a_top2: usize = result.hits[..2]
            .iter()
            .filter(|h| {
                meta.get(&h.doc_id).and_then(|m| m.thread_id.as_deref()) == Some("thread-A")
            })
            .count();
        assert!(
            thread_a_top2 <= 2,
            "Expected at most 2 thread-A in top 2, got {thread_a_top2}"
        );
    }
}
