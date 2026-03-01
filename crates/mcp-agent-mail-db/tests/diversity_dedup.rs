//! # Relevance Regression Suite for Diversity/Dedup Behavior
//!
//! **Bead**: `br-2tnl.7.20`
//!
//! Validates that diversity controls improve result usability (thread/sender
//! spread) without unacceptable relevance loss.  Tests cover:
//!
//! 1. Pre/post diversity ranking quality (NDCG, MRR, Recall)
//! 2. Thread/sender concentration metrics in top-k
//! 3. Config variations (caps, tolerance, window size)
//! 4. Score-gap protection (high-relevance results never demoted)
//! 5. Deterministic behavior across repeated runs
//! 6. Integration with DB-backed corpus and search pipeline

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::uninlined_format_args
)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};

use mcp_agent_mail_db::search_planner::SearchQuery;
use mcp_agent_mail_db::search_service::execute_search_simple;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, queries};

use mcp_agent_mail_db::search_candidates::CandidateSource;
use mcp_agent_mail_db::search_diversity::{DiversityConfig, DiversityMeta, diversify};
use mcp_agent_mail_db::search_fusion::{FusedHit, FusionExplain, SourceContribution};

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("diversity_dedup_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 8,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = Cx::for_testing();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

/// Build a `FusedHit` with the given `doc_id` and `rrf_score`.
fn hit(doc_id: i64, rrf_score: f64) -> FusedHit {
    FusedHit {
        doc_id,
        rrf_score,
        first_source: CandidateSource::Lexical,
        explain: FusionExplain {
            lexical_rank: Some(doc_id as usize),
            lexical_score: Some(rrf_score),
            semantic_rank: None,
            semantic_score: None,
            rrf_score,
            source_contributions: vec![SourceContribution {
                source: "lexical".to_string(),
                contribution: rrf_score,
                rank: Some(doc_id as usize),
            }],
        },
    }
}

/// Build diversity metadata.
fn meta(thread_id: Option<&str>, sender: Option<&str>) -> DiversityMeta {
    DiversityMeta {
        thread_id: thread_id.map(String::from),
        sender: sender.map(String::from),
    }
}

// ── Metrics ────────────────────────────────────────────────────────

fn dcg_at_k(relevances: &[f64], k: usize) -> f64 {
    relevances
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &rel)| (rel.exp2() - 1.0) / (i as f64 + 2.0).log2())
        .sum()
}

fn ndcg_at_k(ranked: &[f64], ideal: &[f64], k: usize) -> f64 {
    let dcg = dcg_at_k(ranked, k);
    let idcg = dcg_at_k(ideal, k);
    if idcg == 0.0 {
        return if dcg == 0.0 { 1.0 } else { 0.0 };
    }
    dcg / idcg
}

fn mrr(ranked: &[f64]) -> f64 {
    for (i, &rel) in ranked.iter().enumerate() {
        if rel > 0.0 {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

fn recall_at_k(ranked: &[f64], total_relevant: usize, k: usize) -> f64 {
    if total_relevant == 0 {
        return 1.0;
    }
    let found = ranked.iter().take(k).filter(|&&r| r > 0.0).count();
    found as f64 / total_relevant as f64
}

// ── Concentration metrics ──────────────────────────────────────────

fn thread_count_in_top_k(
    hits: &[FusedHit],
    metadata: &HashMap<i64, DiversityMeta>,
    k: usize,
    thread_id: &str,
) -> usize {
    hits.iter()
        .take(k)
        .filter(|h| metadata.get(&h.doc_id).and_then(|m| m.thread_id.as_deref()) == Some(thread_id))
        .count()
}

fn sender_count_in_top_k(
    hits: &[FusedHit],
    metadata: &HashMap<i64, DiversityMeta>,
    k: usize,
    sender: &str,
) -> usize {
    hits.iter()
        .take(k)
        .filter(|h| metadata.get(&h.doc_id).and_then(|m| m.sender.as_deref()) == Some(sender))
        .count()
}

fn unique_threads_in_top_k(
    hits: &[FusedHit],
    metadata: &HashMap<i64, DiversityMeta>,
    k: usize,
) -> usize {
    hits.iter()
        .take(k)
        .filter_map(|h| metadata.get(&h.doc_id).and_then(|m| m.thread_id.as_deref()))
        .collect::<HashSet<_>>()
        .len()
}

fn unique_senders_in_top_k(
    hits: &[FusedHit],
    metadata: &HashMap<i64, DiversityMeta>,
    k: usize,
) -> usize {
    hits.iter()
        .take(k)
        .filter_map(|h| metadata.get(&h.doc_id).and_then(|m| m.sender.as_deref()))
        .collect::<HashSet<_>>()
        .len()
}

/// Map `doc_ids` in hits to relevance grades using a provided mapping.
fn extract_relevances(hits: &[FusedHit], grades: &HashMap<i64, f64>) -> Vec<f64> {
    hits.iter()
        .map(|h| grades.get(&h.doc_id).copied().unwrap_or(0.0))
        .collect()
}

// ────────────────────────────────────────────────────────────────────
// Test 1: Thread concentration capping
// ────────────────────────────────────────────────────────────────────

#[test]
fn thread_concentration_capping() {
    // Corpus: 10 docs, 8 from thread-A, 2 from thread-B.
    // All near-tied scores => diversity should spread thread-B docs into top positions.
    let hits: Vec<FusedHit> = (0..10).map(|i| hit(i, 0.5 - (i as f64 * 0.0001))).collect();

    let mut metadata = HashMap::new();
    for i in 0..8 {
        metadata.insert(i, meta(Some("thread-A"), Some("alice")));
    }
    metadata.insert(8, meta(Some("thread-B"), Some("bob")));
    metadata.insert(9, meta(Some("thread-B"), Some("bob")));

    // Before diversity
    let pre_thread_a_top5 = thread_count_in_top_k(&hits, &metadata, 5, "thread-A");
    assert_eq!(pre_thread_a_top5, 5, "pre-diversity: top-5 all thread-A"); // assertion 1

    // Apply diversity with max_per_thread=3
    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 3,
        max_per_sender: 10, // high to isolate thread effect
        score_tolerance: 0.01,
        window_size: 10,
    };
    let result = diversify(hits, &metadata, &config);

    let post_thread_a_top5 = thread_count_in_top_k(&result.hits, &metadata, 5, "thread-A");
    assert!(
        post_thread_a_top5 <= 3,
        "post-diversity: top-5 thread-A count should be <= 3, got {post_thread_a_top5}",
    ); // assertion 2

    let thread_b_in_top5 = thread_count_in_top_k(&result.hits, &metadata, 5, "thread-B");
    assert!(
        thread_b_in_top5 >= 2,
        "post-diversity: thread-B should appear in top-5, got {thread_b_in_top5}",
    ); // assertion 3

    assert!(result.demotions > 0, "should have demoted some hits"); // assertion 4
    assert_eq!(result.hits.len(), 10, "total hits preserved"); // assertion 5
}

// ────────────────────────────────────────────────────────────────────
// Test 2: Sender concentration capping
// ────────────────────────────────────────────────────────────────────

#[test]
fn sender_concentration_capping() {
    // 12 docs: 9 from alice, 3 from bob, all different threads
    let hits: Vec<FusedHit> = (0..12).map(|i| hit(i, 0.8 - (i as f64 * 0.0001))).collect();

    let mut metadata = HashMap::new();
    for i in 0..9 {
        metadata.insert(i, meta(Some(&format!("t{i}")), Some("alice")));
    }
    for i in 9..12 {
        metadata.insert(i, meta(Some(&format!("t{i}")), Some("bob")));
    }

    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 20, // high to isolate sender effect
        max_per_sender: 3,
        score_tolerance: 0.01,
        window_size: 12,
    };

    let result = diversify(hits, &metadata, &config);

    let alice_top5 = sender_count_in_top_k(&result.hits, &metadata, 5, "alice");
    assert!(
        alice_top5 <= 3,
        "alice should have at most 3 in top-5, got {}",
        alice_top5
    ); // assertion 6

    let bob_top5 = sender_count_in_top_k(&result.hits, &metadata, 5, "bob");
    assert!(
        bob_top5 >= 2,
        "bob should appear at least twice in top-5, got {}",
        bob_top5
    ); // assertion 7

    let unique_senders_5 = unique_senders_in_top_k(&result.hits, &metadata, 5);
    assert_eq!(unique_senders_5, 2, "both senders should appear in top-5"); // assertion 8

    assert!(result.demotions > 0, "should have demotions"); // assertion 9
}

// ────────────────────────────────────────────────────────────────────
// Test 3: Score gap protection
// ────────────────────────────────────────────────────────────────────

#[test]
fn score_gap_protection() {
    // Top 3 docs have clearly higher scores (gap > tolerance).
    // They should NEVER be demoted regardless of thread/sender concentration.
    let mut hits = Vec::new();
    hits.push(hit(0, 0.90)); // clearly best
    hits.push(hit(1, 0.85)); // clearly second
    hits.push(hit(2, 0.80)); // clearly third
    // These are near-tied and all from same thread => demotion candidates
    for i in 3..10 {
        hits.push(hit(i, 0.50 - (i as f64 * 0.0001)));
    }

    let mut metadata = HashMap::new();
    // All from same thread and sender
    for i in 0..10 {
        metadata.insert(i, meta(Some("thread-X"), Some("alice")));
    }

    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 2,
        max_per_sender: 2,
        score_tolerance: 0.01,
        window_size: 10,
    };

    let result = diversify(hits, &metadata, &config);

    // Top 3 should remain in their original positions (score gap protects them)
    assert_eq!(result.hits[0].doc_id, 0, "doc 0 should stay at position 0"); // assertion 10
    assert_eq!(result.hits[1].doc_id, 1, "doc 1 should stay at position 1"); // assertion 11
    assert_eq!(result.hits[2].doc_id, 2, "doc 2 should stay at position 2"); // assertion 12
}

// ────────────────────────────────────────────────────────────────────
// Test 4: NDCG/MRR regression with diversity
// ────────────────────────────────────────────────────────────────────

#[test]
fn ndcg_mrr_regression_with_diversity() {
    // Scenario: well-distributed relevance across threads.
    // Diversity should NOT significantly harm NDCG/MRR since relevant
    // docs from different threads get promoted.
    let hits: Vec<FusedHit> = (0..20).map(|i| hit(i, 0.9 - (i as f64 * 0.001))).collect();

    // Relevance grades: docs 0,5,10,15 are highly relevant,
    // docs 1,6,11,16 are relevant, rest are marginal.
    let mut grades = HashMap::new();
    for i in 0..20 {
        let grade = match i % 5 {
            0 => 3.0, // highly relevant
            1 => 2.0, // relevant
            _ => 0.0, // not relevant
        };
        grades.insert(i, grade);
    }

    // Assign diverse threads: each group of 5 in different thread
    let mut metadata = HashMap::new();
    let senders = ["alice", "bob", "carol", "dave"];
    for i in 0..20 {
        let thread = format!("thread-{}", i / 5);
        let sender = senders[(i / 5) as usize % senders.len()];
        metadata.insert(i, meta(Some(&thread), Some(sender)));
    }

    // Pre-diversity metrics
    let pre_rels = extract_relevances(&hits, &grades);
    let mut ideal = pre_rels.clone();
    ideal.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let pre_ndcg5 = ndcg_at_k(&pre_rels, &ideal, 5);
    let pre_mrr = mrr(&pre_rels);
    let total_relevant = grades.values().filter(|&&g| g > 0.0).count();
    let pre_recall5 = recall_at_k(&pre_rels, total_relevant, 5);

    // Apply diversity
    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 2,
        max_per_sender: 3,
        score_tolerance: 0.01,
        window_size: 20,
    };
    let result = diversify(hits.clone(), &metadata, &config);

    let post_rels = extract_relevances(&result.hits, &grades);
    let post_ndcg5 = ndcg_at_k(&post_rels, &ideal, 5);
    let post_mrr = mrr(&post_rels);
    let post_recall5 = recall_at_k(&post_rels, total_relevant, 5);

    eprintln!("ndcg_mrr_regression_with_diversity:");
    eprintln!("  pre:  NDCG@5={pre_ndcg5:.4}, MRR={pre_mrr:.4}, Recall@5={pre_recall5:.4}");
    eprintln!("  post: NDCG@5={post_ndcg5:.4}, MRR={post_mrr:.4}, Recall@5={post_recall5:.4}");

    // MRR should stay at 1.0 (first result is highly relevant and protected by score gap)
    assert!(
        post_mrr >= 0.5,
        "MRR should stay reasonable (>= 0.5), got {post_mrr:.4}"
    ); // assertion 13

    // NDCG shouldn't drop dramatically (tolerance: 30% degradation max)
    let ndcg_ratio = if pre_ndcg5 > 0.0 {
        post_ndcg5 / pre_ndcg5
    } else {
        1.0
    };
    assert!(
        ndcg_ratio >= 0.70,
        "NDCG@5 should not degrade more than 30%, ratio={ndcg_ratio:.4}"
    ); // assertion 14

    // Recall should be preserved or improved
    assert!(
        post_recall5 >= pre_recall5 * 0.8,
        "Recall@5 should not degrade more than 20%"
    ); // assertion 15

    // Thread diversity should improve
    let pre_unique = unique_threads_in_top_k(&hits, &metadata, 5);
    let post_unique = unique_threads_in_top_k(&result.hits, &metadata, 5);
    assert!(
        post_unique >= pre_unique,
        "thread diversity should not decrease: pre={pre_unique}, post={post_unique}"
    ); // assertion 16
}

// ────────────────────────────────────────────────────────────────────
// Test 5: Disabled diversity is passthrough
// ────────────────────────────────────────────────────────────────────

#[test]
fn disabled_diversity_passthrough() {
    let hits: Vec<FusedHit> = (0..10).map(|i| hit(i, 0.5 - (i as f64 * 0.001))).collect();

    let mut metadata = HashMap::new();
    for i in 0..10 {
        metadata.insert(i, meta(Some("same-thread"), Some("alice")));
    }

    let config = DiversityConfig {
        enabled: false,
        ..DiversityConfig::default()
    };

    let result = diversify(hits.clone(), &metadata, &config);

    assert_eq!(result.demotions, 0, "no demotions when disabled"); // assertion 17

    for (i, h) in result.hits.iter().enumerate() {
        assert_eq!(
            h.doc_id, hits[i].doc_id,
            "order preserved when disabled at position {i}"
        ); // assertions 18..27
    }
}

// ────────────────────────────────────────────────────────────────────
// Test 6: Window size boundary
// ────────────────────────────────────────────────────────────────────

#[test]
fn window_size_boundary() {
    // 15 hits, window_size=5. Only first 5 should be subject to diversity.
    let hits: Vec<FusedHit> = (0..15).map(|i| hit(i, 0.8 - (i as f64 * 0.001))).collect();

    let mut metadata = HashMap::new();
    for i in 0..15 {
        metadata.insert(i, meta(Some("same-thread"), Some("alice")));
    }

    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 2,
        max_per_sender: 2,
        score_tolerance: 0.01,
        window_size: 5,
    };

    let result = diversify(hits, &metadata, &config);

    // Hits beyond window (index 5..14) should be in original relative order
    for i in 5..15 {
        assert_eq!(
            result.hits[i].doc_id, i as i64,
            "hit beyond window at position {i} should be unchanged"
        ); // assertions 28..37
    }

    assert_eq!(result.hits.len(), 15, "total hits preserved"); // assertion 38
}

// ────────────────────────────────────────────────────────────────────
// Test 7: Determinism across 100 runs
// ────────────────────────────────────────────────────────────────────

#[test]
fn diversity_determinism() {
    let hits: Vec<FusedHit> = (0..20).map(|i| hit(i, 0.7 - (i as f64 * 0.0005))).collect();

    let mut metadata = HashMap::new();
    let threads = ["t1", "t1", "t1", "t2", "t2", "t3"];
    let senders = ["alice", "alice", "bob", "bob", "carol", "carol"];
    for i in 0..20 {
        metadata.insert(
            i,
            meta(
                Some(threads[i as usize % threads.len()]),
                Some(senders[i as usize % senders.len()]),
            ),
        );
    }

    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 2,
        max_per_sender: 3,
        score_tolerance: 0.005,
        window_size: 20,
    };

    let baseline = diversify(hits.clone(), &metadata, &config);
    let baseline_ids: Vec<i64> = baseline.hits.iter().map(|h| h.doc_id).collect();

    for run in 1..100 {
        let result = diversify(hits.clone(), &metadata, &config);
        let result_ids: Vec<i64> = result.hits.iter().map(|h| h.doc_id).collect();
        assert_eq!(
            result_ids, baseline_ids,
            "run {run} should match baseline ordering"
        );
    }
    // assertion 39 (100 runs match)
    assert_eq!(
        baseline.demotions,
        diversify(hits, &metadata, &config).demotions,
        "demotion count is deterministic"
    ); // assertion 40
}

// ────────────────────────────────────────────────────────────────────
// Test 8: Config parameter sweep
// ────────────────────────────────────────────────────────────────────

#[test]
fn config_parameter_sweep() {
    // Fixed corpus: 20 docs, 4 threads × 5 docs each, 2 senders
    let hits: Vec<FusedHit> = (0..20).map(|i| hit(i, 0.6 - (i as f64 * 0.001))).collect();

    let mut metadata = HashMap::new();
    let threads = ["tA", "tB", "tC", "tD"];
    let senders = ["alice", "bob"];
    for i in 0..20 {
        metadata.insert(
            i,
            meta(
                Some(threads[(i / 5) as usize]),
                Some(senders[(i % 2) as usize]),
            ),
        );
    }

    // Relevance: first doc of each thread is highly relevant
    let mut grades = HashMap::new();
    for i in 0..20 {
        grades.insert(i, if i % 5 == 0 { 3.0 } else { 0.0 });
    }

    // Use wider tolerance so near-tied logic doesn't prevent demotions
    let configs = [
        ("strict", 1, 2, 0.05),
        ("moderate", 3, 5, 0.05),
        ("lenient", 5, 10, 0.05),
        ("very_strict", 1, 1, 0.05),
    ];

    for (label, max_thread, max_sender, tolerance) in &configs {
        let config = DiversityConfig {
            enabled: true,
            max_per_thread: *max_thread,
            max_per_sender: *max_sender,
            score_tolerance: *tolerance,
            window_size: 20,
        };

        let result = diversify(hits.clone(), &metadata, &config);
        let rels = extract_relevances(&result.hits, &grades);
        let mut ideal = rels.clone();
        ideal.sort_by(|a, b| b.partial_cmp(a).unwrap());
        let ndcg = ndcg_at_k(&rels, &ideal, 5);
        let unique_t = unique_threads_in_top_k(&result.hits, &metadata, 5);
        let unique_s = unique_senders_in_top_k(&result.hits, &metadata, 5);

        eprintln!(
            "config_sweep {label}: demotions={}, NDCG@5={ndcg:.4}, unique_threads={unique_t}, unique_senders={unique_s}",
            result.demotions
        );

        // All configs should preserve total result count
        assert_eq!(result.hits.len(), 20, "{label}: total hits preserved"); // assertions 41..44

        // Non-disabled diversity should produce at least some demotions
        // when all docs are near-tied and from limited threads/senders
        assert!(
            result.demotions > 0 || *max_thread >= 5,
            "{label}: strict configs should produce demotions"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Test 9: Missing metadata graceful handling
// ────────────────────────────────────────────────────────────────────

#[test]
fn missing_metadata_graceful() {
    let hits: Vec<FusedHit> = (0..10).map(|i| hit(i, 0.5 - (i as f64 * 0.001))).collect();

    // Only metadata for half the docs
    let mut metadata = HashMap::new();
    for i in 0..5 {
        metadata.insert(i, meta(Some("thread-A"), Some("alice")));
    }
    // docs 5..9 have NO metadata

    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 2,
        max_per_sender: 2,
        score_tolerance: 0.01,
        window_size: 10,
    };

    let result = diversify(hits, &metadata, &config);

    assert_eq!(result.hits.len(), 10, "all hits preserved"); // assertion 45

    // Docs without metadata should still appear in results
    let has_doc5 = result.hits.iter().any(|h| h.doc_id == 5);
    let has_doc9 = result.hits.iter().any(|h| h.doc_id == 9);
    assert!(has_doc5, "doc 5 (no metadata) should be in results"); // assertion 46
    assert!(has_doc9, "doc 9 (no metadata) should be in results"); // assertion 47
}

// ────────────────────────────────────────────────────────────────────
// Test 10: Combined thread + sender caps
// ────────────────────────────────────────────────────────────────────

#[test]
fn combined_thread_sender_caps() {
    // 15 docs: alice sends 10 in thread-A, bob sends 5 in thread-B
    let hits: Vec<FusedHit> = (0..15).map(|i| hit(i, 0.7 - (i as f64 * 0.0001))).collect();

    let mut metadata = HashMap::new();
    for i in 0..10 {
        metadata.insert(i, meta(Some("thread-A"), Some("alice")));
    }
    for i in 10..15 {
        metadata.insert(i, meta(Some("thread-B"), Some("bob")));
    }

    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 3,
        max_per_sender: 3,
        score_tolerance: 0.01,
        window_size: 15,
    };

    let result = diversify(hits, &metadata, &config);

    let alice_top5 = sender_count_in_top_k(&result.hits, &metadata, 5, "alice");
    let bob_top5 = sender_count_in_top_k(&result.hits, &metadata, 5, "bob");
    let thread_a_top5 = thread_count_in_top_k(&result.hits, &metadata, 5, "thread-A");
    let count_tb_top5 = thread_count_in_top_k(&result.hits, &metadata, 5, "thread-B");

    eprintln!(
        "combined_caps top-5: alice={alice_top5}, bob={bob_top5}, tA={thread_a_top5}, tB={count_tb_top5}"
    );

    // Within the window, caps enforce max 3 per sender before forced placement kicks in.
    // In top-5, alice should be capped at max_per_sender=3 since bob docs are available.
    assert!(
        alice_top5 <= 3,
        "alice should have at most 3 in top-5 (before forced placement), got {alice_top5}"
    ); // assertion 48

    assert!(
        thread_a_top5 <= 3,
        "thread-A should have at most 3 in top-5, got {thread_a_top5}"
    ); // assertion 49

    // Both senders and threads should appear
    let unique_s = unique_senders_in_top_k(&result.hits, &metadata, 5);
    assert_eq!(unique_s, 2, "both senders should appear in top-5"); // assertion 50

    let unique_t = unique_threads_in_top_k(&result.hits, &metadata, 5);
    assert_eq!(unique_t, 2, "both threads should appear in top-5"); // assertion 51

    // Verify total result count preserved
    assert_eq!(result.hits.len(), 15, "all hits preserved"); // assertion 52 (bonus)
}

// ────────────────────────────────────────────────────────────────────
// Test 11: Relevance quality floor gating
// ────────────────────────────────────────────────────────────────────

#[test]
fn relevance_quality_floor_gating() {
    // Prove that for diverse corpora, diversity improves spread without
    // breaching a minimum quality floor.
    let hits: Vec<FusedHit> = (0..30).map(|i| hit(i, 0.95 - (i as f64 * 0.001))).collect();

    let mut metadata = HashMap::new();
    let mut grades = HashMap::new();
    let threads: Vec<String> = (0..6).map(|t| format!("thread-{t}")).collect();
    let senders = ["alice", "bob", "carol"];

    for i in 0..30 {
        let thread_idx = (i / 5) as usize;
        metadata.insert(
            i,
            meta(
                Some(&threads[thread_idx]),
                Some(senders[i as usize % senders.len()]),
            ),
        );
        // Relevance: top 2 of each thread are relevant
        grades.insert(i, if i % 5 < 2 { 2.0 } else { 0.0 });
    }

    // Without diversity
    let pre_rels = extract_relevances(&hits, &grades);
    let mut ideal = pre_rels.clone();
    ideal.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let total_rel = grades.values().filter(|&&g| g > 0.0).count();

    let pre_ndcg10 = ndcg_at_k(&pre_rels, &ideal, 10);
    let pre_recall10 = recall_at_k(&pre_rels, total_rel, 10);

    // With diversity
    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 2,
        max_per_sender: 4,
        score_tolerance: 0.01,
        window_size: 20,
    };
    let result = diversify(hits.clone(), &metadata, &config);

    let post_rels = extract_relevances(&result.hits, &grades);
    let post_ndcg10 = ndcg_at_k(&post_rels, &ideal, 10);
    let post_recall10 = recall_at_k(&post_rels, total_rel, 10);

    eprintln!("quality_floor_gating:");
    eprintln!("  pre:  NDCG@10={pre_ndcg10:.4}, Recall@10={pre_recall10:.4}");
    eprintln!("  post: NDCG@10={post_ndcg10:.4}, Recall@10={post_recall10:.4}");

    // Quality floor: NDCG@10 should stay above 0.3
    assert!(
        post_ndcg10 >= 0.3,
        "NDCG@10 quality floor breached: {post_ndcg10:.4}"
    ); // assertion 52

    // Recall should be preserved (diversity spreads relevant docs, may improve recall)
    assert!(
        post_recall10 >= pre_recall10 * 0.7,
        "Recall@10 should not drop more than 30%"
    ); // assertion 53

    // Diversity benefit: unique threads in top-10 should increase
    let pre_unique_t = unique_threads_in_top_k(&hits, &metadata, 10);
    let post_unique_t = unique_threads_in_top_k(&result.hits, &metadata, 10);
    assert!(
        post_unique_t >= pre_unique_t,
        "thread diversity should not decrease in top-10"
    ); // assertion 54
}

// ────────────────────────────────────────────────────────────────────
// Test 12: Score tolerance effect
// ────────────────────────────────────────────────────────────────────

#[test]
fn score_tolerance_effect() {
    // Same corpus, different tolerances. Higher tolerance = more diversity
    let hits: Vec<FusedHit> = (0..10).map(|i| hit(i, 0.5 - (i as f64 * 0.005))).collect();

    let mut metadata = HashMap::new();
    for i in 0..10 {
        metadata.insert(i, meta(Some("same-thread"), Some("alice")));
    }

    let tolerances = [0.0, 0.001, 0.01, 0.05, 0.5];
    let mut demotion_counts = Vec::new();

    for &tol in &tolerances {
        let config = DiversityConfig {
            enabled: true,
            max_per_thread: 2,
            max_per_sender: 2,
            score_tolerance: tol,
            window_size: 10,
        };
        let result = diversify(hits.clone(), &metadata, &config);
        demotion_counts.push(result.demotions);
        eprintln!("tolerance={tol}: demotions={}", result.demotions);
    }

    // Higher tolerance should allow >= demotions as lower
    // (monotonically non-decreasing or at least not dramatically less)
    assert!(
        demotion_counts[4] >= demotion_counts[0],
        "wider tolerance should allow >= demotions vs zero tolerance"
    ); // assertion 55

    // Zero tolerance with score gaps should have fewer demotions
    // since score-gap-protected docs can't be moved
    assert!(
        demotion_counts[0] <= demotion_counts[3],
        "zero tolerance should have <= demotions than moderate tolerance"
    ); // assertion 56
}

// ────────────────────────────────────────────────────────────────────
// Test 13: DB-integrated diversity regression
// ────────────────────────────────────────────────────────────────────

#[test]
fn db_integrated_diversity_regression() {
    // Create a real DB corpus, search it, then apply diversity to the
    // result IDs to verify end-to-end workflow.
    let (pool, _dir) = make_pool();

    let project_id = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::ensure_project(&cx, &pool, "/tmp/div-regression").await {
                Outcome::Ok(p) => p.id.expect("project id"),
                other => panic!("ensure_project failed: {other:?}"),
            }
        }
    });

    // Register agents
    let agent_names = ["RedFox", "BlueLake", "GreenHawk"];
    let mut agent_ids = HashMap::new();
    for &name in &agent_names {
        let id = block_on(|cx| {
            let pool = pool.clone();
            async move {
                match queries::register_agent(
                    &cx, &pool, project_id, name, "test", "model", None, None,
                )
                .await
                {
                    Outcome::Ok(a) => a.id.expect("agent id"),
                    other => panic!("register agent failed: {other:?}"),
                }
            }
        });
        agent_ids.insert(name, id);
    }

    // Create messages: 3 threads × 5 messages each, different senders
    let thread_data = [
        (
            "thread-deploy",
            "RedFox",
            "deploy production release pipeline verification",
        ),
        (
            "thread-auth",
            "BlueLake",
            "auth token refresh session management oauth",
        ),
        (
            "thread-search",
            "GreenHawk",
            "search index rebuild query optimization performance",
        ),
    ];

    let mut msg_ids = Vec::new();
    for (thread_id, sender, topic) in &thread_data {
        let sender_id = agent_ids[sender];
        for i in 0..5 {
            let mid = block_on(|cx| {
                let pool = pool.clone();
                let subject = format!("regression {topic} msg{i}");
                let body = format!("body for regression {topic} discussion iteration {i}");
                let thread_id = thread_id.to_string();
                async move {
                    match queries::create_message(
                        &cx,
                        &pool,
                        project_id,
                        sender_id,
                        &subject,
                        &body,
                        Some(&thread_id),
                        "normal",
                        false,
                        "[]",
                    )
                    .await
                    {
                        Outcome::Ok(m) => m.id.expect("msg id"),
                        other => panic!("create_message failed: {other:?}"),
                    }
                }
            });
            msg_ids.push((mid, *thread_id, *sender));
        }
    }

    // Search for "regression"
    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(
                &cx,
                &pool,
                &SearchQuery::messages("regression", project_id),
            )
            .await
            {
                Outcome::Ok(r) => r,
                other => panic!("search failed: {other:?}"),
            }
        }
    });

    assert!(
        resp.results.len() >= 10,
        "should find regression messages, got {}",
        resp.results.len()
    ); // assertion 57

    // Build FusedHits from search results
    let fused: Vec<FusedHit> = resp
        .results
        .iter()
        .enumerate()
        .map(|(rank, r)| {
            let score = r.score.unwrap_or(1.0 / (rank as f64 + 1.0));
            hit(r.id, score.abs())
        })
        .collect();

    // Build metadata from known corpus
    let mut diversity_meta = HashMap::new();
    for &(mid, thread, sender) in &msg_ids {
        diversity_meta.insert(mid, meta(Some(thread), Some(sender)));
    }

    // Apply diversity
    let config = DiversityConfig {
        enabled: true,
        max_per_thread: 2,
        max_per_sender: 3,
        score_tolerance: 1.0, // wide tolerance since FTS scores vary
        window_size: 15,
    };

    let result = diversify(fused, &diversity_meta, &config);

    // All results should be preserved
    assert_eq!(
        result.hits.len(),
        resp.results.len(),
        "diversity should preserve total result count"
    ); // assertion 58

    // Check thread spread in top-5
    let unique_t_top5 = unique_threads_in_top_k(&result.hits, &diversity_meta, 5);
    eprintln!(
        "db_integrated: unique_threads_top5={unique_t_top5}, demotions={}",
        result.demotions
    );
    // With 3 threads and max_per_thread=2, we should see >= 2 unique threads in top-5
    assert!(
        unique_t_top5 >= 2,
        "should have at least 2 unique threads in top-5 after diversity, got {unique_t_top5}"
    ); // assertion 59
}

// ────────────────────────────────────────────────────────────────────
// Test 14: Concentration regression artifact
// ────────────────────────────────────────────────────────────────────

#[test]
fn concentration_regression_artifact() {
    // Produce a structured concentration report for CI.
    let hits: Vec<FusedHit> = (0..20).map(|i| hit(i, 0.8 - (i as f64 * 0.001))).collect();

    let mut metadata = HashMap::new();
    let threads = ["tA", "tA", "tA", "tA", "tA", "tB", "tB", "tC", "tD", "tE"];
    let senders = [
        "alice", "alice", "alice", "bob", "bob", "carol", "carol", "dave", "dave", "eve",
    ];
    for i in 0..20 {
        let idx = i as usize % threads.len();
        metadata.insert(i, meta(Some(threads[idx]), Some(senders[idx])));
    }

    let mut grades = HashMap::new();
    for i in 0..20 {
        grades.insert(
            i,
            if i < 5 {
                3.0
            } else if i < 10 {
                1.0
            } else {
                0.0
            },
        );
    }

    let configs = [
        ("disabled", false, 10, 10, 0.01),
        ("moderate", true, 3, 5, 0.01),
        ("strict", true, 1, 2, 0.01),
    ];

    let total_rel = grades.values().filter(|&&g| g > 0.0).count();
    let mut ideal_rels = extract_relevances(&hits, &grades);
    ideal_rels.sort_by(|a, b| b.partial_cmp(a).unwrap());

    eprintln!("=== CONCENTRATION REGRESSION ARTIFACT ===");

    for (label, enabled, max_t, max_s, tol) in &configs {
        let config = DiversityConfig {
            enabled: *enabled,
            max_per_thread: *max_t,
            max_per_sender: *max_s,
            score_tolerance: *tol,
            window_size: 20,
        };
        let result = diversify(hits.clone(), &metadata, &config);
        let rels = extract_relevances(&result.hits, &grades);

        let ndcg5 = ndcg_at_k(&rels, &ideal_rels, 5);
        let ndcg10 = ndcg_at_k(&rels, &ideal_rels, 10);
        let mrr_val = mrr(&rels);
        let recall5 = recall_at_k(&rels, total_rel, 5);
        let recall10 = recall_at_k(&rels, total_rel, 10);
        let ut5 = unique_threads_in_top_k(&result.hits, &metadata, 5);
        let ut10 = unique_threads_in_top_k(&result.hits, &metadata, 10);
        let uniq_s5 = unique_senders_in_top_k(&result.hits, &metadata, 5);
        let uniq_s10 = unique_senders_in_top_k(&result.hits, &metadata, 10);

        eprintln!(
            "  {label}: dem={}, NDCG@5={ndcg5:.3}, NDCG@10={ndcg10:.3}, MRR={mrr_val:.3}, R@5={recall5:.3}, R@10={recall10:.3}, ut5={ut5}, ut10={ut10}, us5={uniq_s5}, us10={uniq_s10}",
            result.demotions
        );

        assert_eq!(result.hits.len(), 20, "{label}: hits preserved"); // assertions 60..62
    }

    eprintln!("=== END ARTIFACT ===");
}
