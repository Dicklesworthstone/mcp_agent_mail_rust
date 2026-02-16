//! Relevance benchmark harness with CI-friendly JSON output (br-2tnl.7.4).
//!
//! Runs a curated query corpus through multiple search engine modes,
//! computes NDCG@k, MRR, Precision@k, Recall@k per query per mode,
//! and produces structured JSON reports with regression thresholds.
//!
//! **CI integration**: run with `--nocapture` to see the JSON report on stderr.
//! Parse the `RELEVANCE_REPORT_JSON` block for machine consumption.
//!
//! **Corpus version**: 2.0.0 (extends v1 with sender/date/thread intent queries)

#![allow(
    dead_code,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::missing_const_for_fn,
    clippy::similar_names,
    clippy::clone_on_copy,
    clippy::or_fun_call,
    clippy::uninlined_format_args,
    clippy::stable_sort_primitive,
    clippy::items_after_statements,
    clippy::manual_assert
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::config::SearchEngine;
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::search_planner::{
    DocKind, Importance, RankingMode, SearchQuery, SearchResponse, TimeRange,
};
use mcp_agent_mail_db::search_service::{SearchOptions, execute_search, execute_search_simple};
use mcp_agent_mail_db::{DbPool, DbPoolConfig, now_micros};
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::sync::atomic::{AtomicU64, Ordering};

// ────────────────────────────────────────────────────────────────────
// Async/pool infrastructure
// ────────────────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
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

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("relevance_harness_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 10,
        min_connections: 2,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

// ────────────────────────────────────────────────────────────────────
// Relevance metrics
// ────────────────────────────────────────────────────────────────────

/// Relevance grade for a document (higher = more relevant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Relevance {
    NotRelevant = 0,
    Marginal = 1,
    Relevant = 2,
    Highly = 3,
}

impl Relevance {
    fn gain(self) -> f64 {
        f64::from(self as u8)
    }
}

/// Discounted Cumulative Gain at position `k`.
fn dcg_at_k(relevances: &[f64], k: usize) -> f64 {
    relevances
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &rel)| (rel.exp2() - 1.0) / (i as f64 + 2.0).log2())
        .sum()
}

/// Normalized Discounted Cumulative Gain at position `k`.
fn ndcg_at_k(ranked_relevances: &[f64], ideal_relevances: &[f64], k: usize) -> f64 {
    let dcg = dcg_at_k(ranked_relevances, k);
    let idcg = dcg_at_k(ideal_relevances, k);
    if idcg == 0.0 {
        return if dcg == 0.0 { 1.0 } else { 0.0 };
    }
    dcg / idcg
}

/// Mean Reciprocal Rank: 1/position of first relevant result.
fn mrr(ranked_relevances: &[f64]) -> f64 {
    for (i, &rel) in ranked_relevances.iter().enumerate() {
        if rel > 0.0 {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Precision at position `k`: fraction of top-k that are relevant.
fn precision_at_k(ranked_relevances: &[f64], k: usize) -> f64 {
    let relevant = ranked_relevances
        .iter()
        .take(k)
        .filter(|&&r| r > 0.0)
        .count();
    relevant as f64 / k.min(ranked_relevances.len()).max(1) as f64
}

/// Recall at position `k`: fraction of all relevant docs found in top-k.
fn recall_at_k(ranked_relevances: &[f64], total_relevant: usize, k: usize) -> f64 {
    if total_relevant == 0 {
        return 1.0;
    }
    let found = ranked_relevances
        .iter()
        .take(k)
        .filter(|&&r| r > 0.0)
        .count();
    found as f64 / total_relevant as f64
}

// ────────────────────────────────────────────────────────────────────
// Benchmark corpus (v2.0.0)
// ────────────────────────────────────────────────────────────────────

struct CorpusMessage {
    subject: &'static str,
    body: &'static str,
    thread_id: Option<&'static str>,
    importance: &'static str,
    ack_required: bool,
    sender_name: &'static str,
}

fn corpus_v2() -> Vec<CorpusMessage> {
    vec![
        // ── Thread: br-42 (file reservation conflict) ────────────────
        CorpusMessage {
            subject: "[br-42] Start: file reservation conflict in storage layer",
            body: "RedHarbor claiming br-42. Storage crate has file reservation conflict when two agents attempt exclusive locks on overlapping glob patterns. Root cause: symmetric fnmatch not handling ** vs * correctly.",
            thread_id: Some("br-42"),
            importance: "high",
            ack_required: true,
            sender_name: "RedHarbor",
        },
        CorpusMessage {
            subject: "[br-42] Progress: guard fnmatch fixed, tests passing",
            body: "RedHarbor here. Fixed symmetric fnmatch in guard.rs. Now ** correctly matches nested paths. Added 12 unit tests. All 34 guard tests green.",
            thread_id: Some("br-42"),
            importance: "normal",
            ack_required: false,
            sender_name: "RedHarbor",
        },
        CorpusMessage {
            subject: "[br-42] Completed: reservation conflict resolution merged",
            body: "Guard fix merged. Commit abc123. Closing br-42. Fnmatch symmetric comparison handles all glob patterns including ** for recursive directory matching.",
            thread_id: Some("br-42"),
            importance: "normal",
            ack_required: false,
            sender_name: "RedHarbor",
        },
        // ── Thread: br-99 (SQLite WAL) ───────────────────────────────
        CorpusMessage {
            subject: "[br-99] Start: SQLite WAL checkpoint tuning for concurrent writes",
            body: "Under heavy concurrent message sending (8+ threads), WAL file grows unbounded. Plan: reduce wal_autocheckpoint from 2000 to 500, add explicit checkpoint after bulk operations.",
            thread_id: Some("br-99"),
            importance: "high",
            ack_required: true,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "[br-99] Progress: WAL checkpoint tuning benchmarks",
            body: "Benchmarked three checkpoint strategies. wal_autocheckpoint=500: 15% write latency improvement. Explicit TRUNCATE after bulk: 40% WAL size reduction. Recommending option 2.",
            thread_id: Some("br-99"),
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "[br-99] Completed: WAL tuning deployed with monitoring",
            body: "Deployed WAL checkpoint tuning. wal_autocheckpoint=500, explicit TRUNCATE checkpoint after bulk message inserts. Added WAL size metric to SystemHealth screen.",
            thread_id: Some("br-99"),
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        // ── Thread: search-v3 (search improvement) ───────────────────
        CorpusMessage {
            subject: "Search V3 architecture proposal: Tantivy + semantic embeddings",
            body: "Proposing hybrid search: Tantivy lexical (BM25) + semantic embeddings (MiniLM-L6-v2). RRF fusion with k=60, cross-encoder reranking optional. Two-tier: fast (potion-128M) then quality refinement.",
            thread_id: Some("search-v3"),
            importance: "high",
            ack_required: true,
            sender_name: "GoldHawk",
        },
        CorpusMessage {
            subject: "Re: Search V3: benchmark results for hybrid vs lexical-only",
            body: "Hybrid outperforms lexical-only on natural language queries (NDCG@5 0.87 vs 0.72). Lexical still better for exact ID lookups (MRR 1.0 vs 0.85). Recommend hybrid with lexical fallback.",
            thread_id: Some("search-v3"),
            importance: "normal",
            ack_required: false,
            sender_name: "GoldHawk",
        },
        // ── Thread: perf-review (performance) ────────────────────────
        CorpusMessage {
            subject: "Performance regression in message delivery pipeline",
            body: "P95 delivery latency increased from 12ms to 45ms after commit coalescer changes. Sharded worker pool has hot-shard problem when most messages go to the same project.",
            thread_id: Some("perf-review"),
            importance: "urgent",
            ack_required: true,
            sender_name: "SilverLake",
        },
        CorpusMessage {
            subject: "Re: Performance regression in message delivery pipeline",
            body: "Confirmed hot-shard issue. Weighted hashing deployed. P95 back down to 18ms. Remaining latency from WAL contention during checkpoint windows.",
            thread_id: Some("perf-review"),
            importance: "high",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        // ── Thread: migration-v3 (database migration) ────────────────
        CorpusMessage {
            subject: "v3 migration: converting TEXT timestamps to i64 microseconds",
            body: "The v3 migration converts legacy Python TEXT timestamps (ISO-8601) to Rust i64 microseconds. Uses strftime('%s') * 1000000 plus fractional extraction. DATETIME column has NUMERIC affinity.",
            thread_id: Some("migration-v3"),
            importance: "high",
            ack_required: true,
            sender_name: "SilverLake",
        },
        CorpusMessage {
            subject: "Re: v3 migration: timestamp edge cases for pre-1970 dates",
            body: "Found edge case: pre-1970 timestamps produce negative values. Need div_euclid/rem_euclid for correct microsecond extraction. Added test vector for 1969-12-31.",
            thread_id: Some("migration-v3"),
            importance: "normal",
            ack_required: false,
            sender_name: "SilverLake",
        },
        // ── Thread: security-audit ───────────────────────────────────
        CorpusMessage {
            subject: "Security audit: FTS5 query injection prevention",
            body: "Reviewed sanitize_fts_query(). Protections: strip leading wildcards, quote hyphenated tokens, reject bare operators, LIKE fallback. No SQL injection risk because FTS5 MATCH is parameterized.",
            thread_id: Some("security-audit"),
            importance: "high",
            ack_required: false,
            sender_name: "GoldHawk",
        },
        CorpusMessage {
            subject: "Security audit: path traversal in thread_id",
            body: "Found and fixed path traversal vulnerability in sanitize_thread_id(). Thread IDs used as directory names in Git archive. Reject any thread_id containing .. or absolute path prefixes.",
            thread_id: Some("security-audit"),
            importance: "urgent",
            ack_required: true,
            sender_name: "RedHarbor",
        },
        // ── Standalone messages ──────────────────────────────────────
        CorpusMessage {
            subject: "conformance test results: all 23 tools passing",
            body: "Full conformance run against Python fixtures: 23/23 tools passing, 23+ resources matching. Format parity verified for ToolDirectory, ToolSchemasResponse, LocksResponse.",
            thread_id: None,
            importance: "normal",
            ack_required: false,
            sender_name: "GreenMeadow",
        },
        CorpusMessage {
            subject: "E2E test suite expansion: 44 share assertions added",
            body: "Added 44 new E2E assertions for share/export pipeline. Covers snapshot creation, scrub presets, bundle encryption, deterministic archive hashing.",
            thread_id: None,
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "write-behind cache coherency issue under concurrent load",
            body: "ReadCache can serve stale project data when ensure_project races with concurrent register_agent. Dual-indexed cache needs invalidation on agent registration.",
            thread_id: Some("cache-coherency"),
            importance: "high",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "Circuit breaker implementation for database pool",
            body: "Added circuit breaker to DbPool. States: Closed, Open, HalfOpen. Trips after 5 consecutive failures. Reset timeout: 30 seconds. Health check sends probe query on half-open.",
            thread_id: Some("circuit-breaker"),
            importance: "high",
            ack_required: false,
            sender_name: "SilverLake",
        },
        CorpusMessage {
            subject: "Attachment pipeline: WebP conversion + inline mode",
            body: "Three attachment modes: inline (base64), file (archive), auto (256KB threshold). WebP conversion reduces storage by 60%. Seven unit tests covering all modes.",
            thread_id: Some("attachment-pipeline"),
            importance: "normal",
            ack_required: false,
            sender_name: "GreenMeadow",
        },
        CorpusMessage {
            subject: "Agent coordination: who is working on what?",
            body: "Roll call. GreenMeadow on TUI dashboard sparkline. Need to know if anyone touching tui_app.rs or tui_chrome.rs for reservation.",
            thread_id: Some("coord-agents"),
            importance: "normal",
            ack_required: true,
            sender_name: "GreenMeadow",
        },
        // ── Noise / low relevance ────────────────────────────────────
        CorpusMessage {
            subject: "Weekly status: nothing to report",
            body: "No blockers. Continuing with assigned beads. Will update when noteworthy.",
            thread_id: Some("weekly-status"),
            importance: "low",
            ack_required: false,
            sender_name: "GreenMeadow",
        },
        CorpusMessage {
            subject: "Out of office: vacation next week",
            body: "RedHarbor unavailable Monday through Friday. Current reservations expire naturally.",
            thread_id: Some("ooo"),
            importance: "low",
            ack_required: false,
            sender_name: "RedHarbor",
        },
        CorpusMessage {
            subject: "Question: should we use tokio or asupersync?",
            body: "Project mandate: use asupersync (not tokio). Provides Cx-based context propagation, Budget-aware cancellation, and testing utilities.",
            thread_id: Some("tech-questions"),
            importance: "normal",
            ack_required: false,
            sender_name: "GoldHawk",
        },
    ]
}

// ────────────────────────────────────────────────────────────────────
// Benchmark query definition
// ────────────────────────────────────────────────────────────────────

struct BenchmarkQuery {
    /// Human-readable label.
    label: &'static str,
    /// Intent category for grouping in reports.
    intent: &'static str,
    /// Search text.
    text: &'static str,
    /// Facet filters.
    importance: Vec<Importance>,
    /// Thread filter.
    thread_id: Option<&'static str>,
    /// Time range filter.
    use_time_range: bool,
    /// Ranking mode.
    ranking: RankingMode,
    /// Relevance judgments: message subject -> grade.
    judgments: Vec<(&'static str, Relevance)>,
    /// Minimum NDCG@5 threshold.
    min_ndcg5: f64,
    /// Minimum MRR threshold.
    min_mrr: f64,
    /// Minimum Precision@3 threshold.
    min_p3: f64,
    /// Minimum Recall@5 threshold.
    min_r5: f64,
}

fn queries_v2() -> Vec<BenchmarkQuery> {
    use Relevance::*;
    vec![
        // ── Thread intent: exact thread ID ───────────────────────────
        BenchmarkQuery {
            label: "thread_exact_br42",
            intent: "thread",
            text: "br-42",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("[br-42] Start: file reservation conflict in storage layer", Highly),
                ("[br-42] Progress: guard fnmatch fixed, tests passing", Highly),
                ("[br-42] Completed: reservation conflict resolution merged", Highly),
            ],
            min_ndcg5: 0.8,
            min_mrr: 1.0,
            min_p3: 1.0,
            min_r5: 0.9,
        },
        // ── Thread intent: thread filter facet ───────────────────────
        BenchmarkQuery {
            label: "thread_filter_migration_v3",
            intent: "thread",
            text: "",
            importance: vec![],
            thread_id: Some("migration-v3"),
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("v3 migration: converting TEXT timestamps to i64 microseconds", Highly),
                ("Re: v3 migration: timestamp edge cases for pre-1970 dates", Highly),
            ],
            min_ndcg5: 1.0,
            min_mrr: 1.0,
            min_p3: 0.66,
            min_r5: 1.0,
        },
        // ── Sender intent: agent name in text ────────────────────────
        BenchmarkQuery {
            label: "sender_redharbor",
            intent: "sender",
            text: "RedHarbor",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("[br-42] Start: file reservation conflict in storage layer", Highly),
                ("[br-42] Progress: guard fnmatch fixed, tests passing", Highly),
                ("[br-42] Completed: reservation conflict resolution merged", Highly),
                ("Security audit: path traversal in thread_id", Relevant),
                ("Out of office: vacation next week", Marginal),
            ],
            min_ndcg5: 0.6,
            min_mrr: 1.0,
            min_p3: 0.66,
            min_r5: 0.6,
        },
        // ── Sender intent: agent name BlueCastle ─────────────────────
        BenchmarkQuery {
            label: "sender_bluecastle",
            intent: "sender",
            text: "BlueCastle",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("[br-99] Start: SQLite WAL checkpoint tuning for concurrent writes", Highly),
                ("[br-99] Progress: WAL checkpoint tuning benchmarks", Highly),
                ("[br-99] Completed: WAL tuning deployed with monitoring", Highly),
                ("Re: Performance regression in message delivery pipeline", Relevant),
                ("E2E test suite expansion: 44 share assertions added", Relevant),
                ("write-behind cache coherency issue under concurrent load", Relevant),
            ],
            min_ndcg5: 0.5,
            min_mrr: 1.0,
            min_p3: 0.33,
            min_r5: 0.5,
        },
        // ── Topic intent: WAL/SQLite tuning ──────────────────────────
        BenchmarkQuery {
            label: "topic_wal_tuning",
            intent: "topic",
            text: "WAL checkpoint SQLite",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("[br-99] Start: SQLite WAL checkpoint tuning for concurrent writes", Highly),
                ("[br-99] Progress: WAL checkpoint tuning benchmarks", Highly),
                ("[br-99] Completed: WAL tuning deployed with monitoring", Relevant),
            ],
            min_ndcg5: 0.7,
            min_mrr: 1.0,
            min_p3: 0.66,
            min_r5: 0.6,
        },
        // ── Topic intent: security ───────────────────────────────────
        BenchmarkQuery {
            label: "topic_security",
            intent: "topic",
            text: "security audit",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("Security audit: FTS5 query injection prevention", Highly),
                ("Security audit: path traversal in thread_id", Highly),
            ],
            min_ndcg5: 0.8,
            min_mrr: 1.0,
            min_p3: 0.66,
            min_r5: 0.8,
        },
        // ── Topic intent: performance regression ─────────────────────
        BenchmarkQuery {
            label: "topic_perf_regression",
            intent: "topic",
            text: "performance regression latency",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("Performance regression in message delivery pipeline", Highly),
                ("Re: Performance regression in message delivery pipeline", Highly),
            ],
            min_ndcg5: 0.5,
            min_mrr: 0.5,
            min_p3: 0.33,
            min_r5: 0.4,
        },
        // ── Topic intent: search architecture ────────────────────────
        BenchmarkQuery {
            label: "topic_search_v3",
            intent: "topic",
            text: "hybrid search Tantivy semantic",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("Search V3 architecture proposal: Tantivy + semantic embeddings", Highly),
                ("Re: Search V3: benchmark results for hybrid vs lexical-only", Highly),
            ],
            min_ndcg5: 0.5,
            min_mrr: 0.5,
            min_p3: 0.33,
            min_r5: 0.4,
        },
        // ── Facet intent: importance filter ───────────────────────────
        BenchmarkQuery {
            label: "facet_urgent_only",
            intent: "facet",
            text: "",
            importance: vec![Importance::Urgent],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("Performance regression in message delivery pipeline", Highly),
                ("Security audit: path traversal in thread_id", Highly),
            ],
            min_ndcg5: 0.9,
            min_mrr: 1.0,
            min_p3: 0.66,
            min_r5: 1.0,
        },
        // ── Facet intent: high importance + text ─────────────────────
        BenchmarkQuery {
            label: "facet_high_circuit_breaker",
            intent: "facet",
            text: "circuit breaker",
            importance: vec![Importance::High, Importance::Urgent],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("Circuit breaker implementation for database pool", Highly),
            ],
            min_ndcg5: 0.8,
            min_mrr: 1.0,
            min_p3: 0.33,
            min_r5: 1.0,
        },
        // ── Date intent: recency ranking ─────────────────────────────
        BenchmarkQuery {
            label: "date_recency_all",
            intent: "date",
            text: "",
            importance: vec![],
            thread_id: None,
            use_time_range: true,
            ranking: RankingMode::Recency,
            judgments: vec![], // recency doesn't judge relevance — just checks ordering
            min_ndcg5: 1.0,   // vacuously true
            min_mrr: 0.0,
            min_p3: 0.0,
            min_r5: 1.0,
        },
        // ── Prefix intent: partial match ─────────────────────────────
        BenchmarkQuery {
            label: "prefix_conform",
            intent: "topic",
            text: "conform*",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("conformance test results: all 23 tools passing", Highly),
            ],
            min_ndcg5: 0.8,
            min_mrr: 1.0,
            min_p3: 0.33,
            min_r5: 1.0,
        },
        // ── Negative intent: no matches ──────────────────────────────
        BenchmarkQuery {
            label: "negative_kubernetes",
            intent: "negative",
            text: "kubernetes docker container orchestration",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![],
            min_ndcg5: 1.0,
            min_mrr: 0.0,
            min_p3: 0.0,
            min_r5: 1.0,
        },
        // ── Compound intent: text + thread ───────────────────────────
        BenchmarkQuery {
            label: "compound_guard_in_br42",
            intent: "compound",
            text: "guard fnmatch",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("[br-42] Progress: guard fnmatch fixed, tests passing", Highly),
                ("[br-42] Start: file reservation conflict in storage layer", Relevant),
                ("[br-42] Completed: reservation conflict resolution merged", Relevant),
            ],
            min_ndcg5: 0.5,
            min_mrr: 1.0,
            min_p3: 0.33,
            min_r5: 0.5,
        },
        // ── Cache/invalidation intent ────────────────────────────────
        BenchmarkQuery {
            label: "topic_cache_invalidation",
            intent: "topic",
            text: "cache coherency invalidation",
            importance: vec![],
            thread_id: None,
            use_time_range: false,
            ranking: RankingMode::Relevance,
            judgments: vec![
                ("write-behind cache coherency issue under concurrent load", Highly),
            ],
            min_ndcg5: 0.6,
            min_mrr: 1.0,
            min_p3: 0.33,
            min_r5: 0.5,
        },
    ]
}

// ────────────────────────────────────────────────────────────────────
// Corpus seeding
// ────────────────────────────────────────────────────────────────────

struct SeededCorpus {
    pool: DbPool,
    project_id: i64,
    agents: HashMap<String, i64>,
    message_subjects: HashMap<String, i64>,
    _dir: tempfile::TempDir,
}

fn seed_corpus() -> SeededCorpus {
    let (pool, dir) = make_pool();
    let corpus = corpus_v2();

    let sender_names: Vec<&str> = {
        let mut names: Vec<&str> = corpus.iter().map(|m| m.sender_name).collect();
        names.sort_unstable();
        names.dedup();
        names
    };

    let (project_id, agents) = {
        let p = pool.clone();
        block_on(move |cx| async move {
            let proj = match queries::ensure_project(&cx, &p, "/bench/relevance-harness").await {
                Outcome::Ok(r) => r,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let pid = proj.id.unwrap();

            let mut agent_map = HashMap::new();
            for name in &sender_names {
                let agent = match queries::register_agent(
                    &cx, &p, pid, name, "bench", "test", None, None,
                )
                .await
                {
                    Outcome::Ok(r) => r,
                    other => panic!("register_agent({name}) failed: {other:?}"),
                };
                agent_map.insert(name.to_string(), agent.id.unwrap());
            }

            (pid, agent_map)
        })
    };

    let mut message_subjects = HashMap::new();
    for msg in &corpus {
        let sender_id = agents[msg.sender_name];
        let p = pool.clone();
        let subject = msg.subject;
        let body = msg.body;
        let thread_id = msg.thread_id;
        let importance = msg.importance;
        let ack_required = msg.ack_required;

        let row = block_on(move |cx| async move {
            match queries::create_message(
                &cx,
                &p,
                project_id,
                sender_id,
                subject,
                body,
                thread_id,
                importance,
                ack_required,
                "",
            )
            .await
            {
                Outcome::Ok(r) => r,
                other => panic!("create_message({subject}) failed: {other:?}"),
            }
        });

        message_subjects.insert(subject.to_string(), row.id.unwrap());
    }

    SeededCorpus {
        pool,
        project_id,
        agents,
        message_subjects,
        _dir: dir,
    }
}

// ────────────────────────────────────────────────────────────────────
// Query evaluation
// ────────────────────────────────────────────────────────────────────

/// Metrics for a single query evaluation.
#[derive(Debug, Clone)]
struct QueryMetrics {
    label: &'static str,
    intent: &'static str,
    engine: String,
    ndcg5: f64,
    mrr: f64,
    precision3: f64,
    recall5: f64,
    result_count: usize,
    method: String,
    pass: bool,
    failures: Vec<String>,
}

fn build_search_query(bq: &BenchmarkQuery, project_id: i64) -> SearchQuery {
    let mut query = SearchQuery {
        text: bq.text.to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(project_id),
        importance: bq.importance.clone(),
        explain: true,
        limit: Some(20),
        ranking: bq.ranking.clone(),
        ..SearchQuery::default()
    };
    if let Some(tid) = bq.thread_id {
        query.thread_id = Some(tid.to_string());
    }
    if bq.use_time_range {
        let now = now_micros();
        query.time_range = TimeRange {
            min_ts: Some(0),
            max_ts: Some(now + 1_000_000),
        };
    }
    query
}

fn evaluate_query_simple(corpus: &SeededCorpus, bq: &BenchmarkQuery) -> QueryMetrics {
    let p = corpus.pool.clone();
    let query = build_search_query(bq, corpus.project_id);

    let response: SearchResponse = block_on(move |cx| async move {
        match execute_search_simple(&cx, &p, &query).await {
            Outcome::Ok(resp) => resp,
            other => panic!("search failed for '{}': {other:?}", bq.label),
        }
    });

    let method = response
        .explain
        .as_ref()
        .map_or("unknown".to_string(), |e| e.method.clone());

    score_response(bq, &response.results, "default", &method)
}

fn evaluate_query_with_engine(
    corpus: &SeededCorpus,
    bq: &BenchmarkQuery,
    engine: SearchEngine,
    engine_label: &str,
) -> QueryMetrics {
    let p = corpus.pool.clone();
    let query = build_search_query(bq, corpus.project_id);
    let opts = SearchOptions {
        scope_ctx: None,
        redaction_policy: None,
        track_telemetry: false,
        search_engine: Some(engine),
    };

    let response = block_on(move |cx| async move {
        match execute_search(&cx, &p, &query, &opts).await {
            Outcome::Ok(resp) => resp,
            other => panic!("search({engine_label}) failed for '{}': {other:?}", bq.label),
        }
    });

    // ScopedSearchResponse -> extract titles from results
    let titles: Vec<(String, Option<f64>)> = response
        .results
        .iter()
        .map(|r| (r.result.title.clone(), r.result.score))
        .collect();

    let method = response
        .explain
        .as_ref()
        .map_or("unknown".to_string(), |e| e.method.clone());

    score_titles(bq, &titles, engine_label, &method)
}

fn score_response(
    bq: &BenchmarkQuery,
    results: &[mcp_agent_mail_db::search_planner::SearchResult],
    engine_label: &str,
    method: &str,
) -> QueryMetrics {
    let titles: Vec<(String, Option<f64>)> = results
        .iter()
        .map(|r| (r.title.clone(), r.score))
        .collect();
    score_titles(bq, &titles, engine_label, method)
}

fn score_titles(
    bq: &BenchmarkQuery,
    titles: &[(String, Option<f64>)],
    engine_label: &str,
    method: &str,
) -> QueryMetrics {
    let judgment_map: HashMap<&str, Relevance> = bq.judgments.iter().copied().collect();

    let ranked_relevances: Vec<f64> = titles
        .iter()
        .map(|(title, _)| {
            judgment_map
                .get(title.as_str())
                .copied()
                .unwrap_or(Relevance::NotRelevant)
                .gain()
        })
        .collect();

    let mut ideal: Vec<f64> = bq.judgments.iter().map(|(_, rel)| rel.gain()).collect();
    ideal.sort_by(|a, b| b.partial_cmp(a).unwrap());

    let total_relevant = bq
        .judgments
        .iter()
        .filter(|(_, r)| *r != Relevance::NotRelevant)
        .count();

    let ndcg5 = ndcg_at_k(&ranked_relevances, &ideal, 5);
    let mrr_val = mrr(&ranked_relevances);
    let precision3 = precision_at_k(&ranked_relevances, 3);
    let recall5 = recall_at_k(&ranked_relevances, total_relevant, 5);

    let mut failures = Vec::new();
    if ndcg5 < bq.min_ndcg5 {
        failures.push(format!("NDCG@5 {ndcg5:.3} < min {:.3}", bq.min_ndcg5));
    }
    if mrr_val < bq.min_mrr {
        failures.push(format!("MRR {mrr_val:.3} < min {:.3}", bq.min_mrr));
    }
    if precision3 < bq.min_p3 {
        failures.push(format!("P@3 {precision3:.3} < min {:.3}", bq.min_p3));
    }
    if recall5 < bq.min_r5 {
        failures.push(format!("R@5 {recall5:.3} < min {:.3}", bq.min_r5));
    }

    QueryMetrics {
        label: bq.label,
        intent: bq.intent,
        engine: engine_label.to_string(),
        ndcg5,
        mrr: mrr_val,
        precision3,
        recall5,
        result_count: titles.len(),
        method: method.to_string(),
        pass: failures.is_empty(),
        failures,
    }
}

// ────────────────────────────────────────────────────────────────────
// JSON report generation
// ────────────────────────────────────────────────────────────────────

fn generate_json_report(all_metrics: &[QueryMetrics]) -> String {
    let mut json = String::from("{\n");

    // Metadata
    let _ = writeln!(json, "  \"corpus_version\": \"2.0.0\",");
    let _ = writeln!(json, "  \"harness_version\": \"1.0.0\",");
    let _ = writeln!(
        json,
        "  \"total_queries\": {},",
        all_metrics.iter().map(|m| m.label).collect::<std::collections::HashSet<_>>().len()
    );
    let _ = writeln!(
        json,
        "  \"engines_tested\": {:?},",
        all_metrics
            .iter()
            .map(|m| m.engine.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );

    // Per-query metrics
    let _ = writeln!(json, "  \"queries\": [");
    for (i, m) in all_metrics.iter().enumerate() {
        let comma = if i + 1 < all_metrics.len() { "," } else { "" };
        let _ = writeln!(json, "    {{");
        let _ = writeln!(json, "      \"label\": {:?},", m.label);
        let _ = writeln!(json, "      \"intent\": {:?},", m.intent);
        let _ = writeln!(json, "      \"engine\": {:?},", m.engine);
        let _ = writeln!(json, "      \"method\": {:?},", m.method);
        let _ = writeln!(json, "      \"ndcg5\": {:.4},", m.ndcg5);
        let _ = writeln!(json, "      \"mrr\": {:.4},", m.mrr);
        let _ = writeln!(json, "      \"precision3\": {:.4},", m.precision3);
        let _ = writeln!(json, "      \"recall5\": {:.4},", m.recall5);
        let _ = writeln!(json, "      \"result_count\": {},", m.result_count);
        let _ = writeln!(json, "      \"pass\": {}", m.pass);
        let _ = writeln!(json, "    }}{comma}");
    }
    let _ = writeln!(json, "  ],");

    // Aggregate by engine
    let engines: Vec<String> = {
        let mut e: Vec<String> = all_metrics.iter().map(|m| m.engine.clone()).collect();
        e.sort();
        e.dedup();
        e
    };
    let _ = writeln!(json, "  \"aggregates_by_engine\": {{");
    for (ei, engine) in engines.iter().enumerate() {
        let engine_metrics: Vec<&QueryMetrics> =
            all_metrics.iter().filter(|m| m.engine == *engine).collect();
        let n = engine_metrics.len() as f64;
        let mean_ndcg5 = engine_metrics.iter().map(|m| m.ndcg5).sum::<f64>() / n;
        let mean_mrr = engine_metrics.iter().map(|m| m.mrr).sum::<f64>() / n;
        let mean_p3 = engine_metrics.iter().map(|m| m.precision3).sum::<f64>() / n;
        let mean_r5 = engine_metrics.iter().map(|m| m.recall5).sum::<f64>() / n;
        let pass_count = engine_metrics.iter().filter(|m| m.pass).count();
        let comma = if ei + 1 < engines.len() { "," } else { "" };
        let _ = writeln!(json, "    {:?}: {{", engine);
        let _ = writeln!(json, "      \"mean_ndcg5\": {mean_ndcg5:.4},");
        let _ = writeln!(json, "      \"mean_mrr\": {mean_mrr:.4},");
        let _ = writeln!(json, "      \"mean_precision3\": {mean_p3:.4},");
        let _ = writeln!(json, "      \"mean_recall5\": {mean_r5:.4},");
        let _ = writeln!(json, "      \"pass_rate\": {:.4}", pass_count as f64 / n);
        let _ = writeln!(json, "    }}{comma}");
    }
    let _ = writeln!(json, "  }},");

    // Aggregate by intent
    let intents: Vec<&str> = {
        let mut i: Vec<&str> = all_metrics.iter().map(|m| m.intent).collect();
        i.sort();
        i.dedup();
        i
    };
    let _ = writeln!(json, "  \"aggregates_by_intent\": {{");
    for (ii, intent) in intents.iter().enumerate() {
        let intent_metrics: Vec<&QueryMetrics> =
            all_metrics.iter().filter(|m| m.intent == *intent).collect();
        let n = intent_metrics.len() as f64;
        let mean_ndcg5 = intent_metrics.iter().map(|m| m.ndcg5).sum::<f64>() / n;
        let mean_mrr = intent_metrics.iter().map(|m| m.mrr).sum::<f64>() / n;
        let comma = if ii + 1 < intents.len() { "," } else { "" };
        let _ = writeln!(json, "    {:?}: {{", intent);
        let _ = writeln!(json, "      \"query_count\": {},", intent_metrics.len());
        let _ = writeln!(json, "      \"mean_ndcg5\": {mean_ndcg5:.4},");
        let _ = writeln!(json, "      \"mean_mrr\": {mean_mrr:.4}");
        let _ = writeln!(json, "    }}{comma}");
    }
    let _ = writeln!(json, "  }}");

    json.push('}');
    json
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

/// Full relevance benchmark: default engine path with threshold assertions.
#[test]
fn relevance_benchmark_default_engine() {
    let corpus = seed_corpus();
    let queries = queries_v2();

    let mut all_metrics: Vec<QueryMetrics> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for bq in &queries {
        let metrics = evaluate_query_simple(&corpus, bq);

        if !metrics.pass {
            let mut fail_msg = format!("[{}] FAIL:", bq.label);
            for f in &metrics.failures {
                let _ = write!(fail_msg, " {f};");
            }
            failures.push(fail_msg);
        }

        all_metrics.push(metrics);
    }

    // Print report table
    eprintln!("\n╔══════════════════════════════════════════════════════════════════════════════╗");
    eprintln!("║  Relevance Benchmark Report (corpus v2.0.0, engine: default)               ║");
    eprintln!("╠══════════════════════════════════════════════════════════════════════════════╣");
    eprintln!(
        "║  {:<35} {:>7} {:>7} {:>7} {:>7} {:>5} {:>6}",
        "Query", "NDCG@5", "MRR", "P@3", "R@5", "N", "Status"
    );
    eprintln!("║  {}", "-".repeat(74));
    for m in &all_metrics {
        let status = if m.pass { "PASS" } else { "FAIL" };
        eprintln!(
            "║  {:<35} {:>7.3} {:>7.3} {:>7.3} {:>7.3} {:>5} {:>6}",
            m.label, m.ndcg5, m.mrr, m.precision3, m.recall5, m.result_count, status,
        );
    }
    eprintln!("╚══════════════════════════════════════════════════════════════════════════════╝");

    // Aggregate
    let n = all_metrics.len() as f64;
    let mean_ndcg5 = all_metrics.iter().map(|m| m.ndcg5).sum::<f64>() / n;
    let mean_mrr = all_metrics.iter().map(|m| m.mrr).sum::<f64>() / n;
    let mean_p3 = all_metrics.iter().map(|m| m.precision3).sum::<f64>() / n;
    let mean_r5 = all_metrics.iter().map(|m| m.recall5).sum::<f64>() / n;
    eprintln!(
        "\nAggregate: mean_NDCG@5={mean_ndcg5:.3}  mean_MRR={mean_mrr:.3}  \
         mean_P@3={mean_p3:.3}  mean_R@5={mean_r5:.3}"
    );

    // JSON report
    let json_report = generate_json_report(&all_metrics);
    eprintln!("\nRELEVANCE_REPORT_JSON_BEGIN");
    eprintln!("{json_report}");
    eprintln!("RELEVANCE_REPORT_JSON_END");

    if !failures.is_empty() {
        eprintln!("\n=== FAILURES ===");
        for f in &failures {
            eprintln!("{f}");
        }
        panic!(
            "{} quality threshold(s) failed out of {} queries.\n\
             See report above for details.",
            failures.len(),
            queries.len()
        );
    }
}

/// Mode-stratified benchmark: run queries through Legacy, Lexical, and Hybrid engines.
///
/// All modes should gracefully degrade to FTS5 when bridges aren't initialized.
/// This test validates that quality thresholds hold regardless of engine selection.
#[test]
fn relevance_benchmark_mode_stratified() {
    let corpus = seed_corpus();
    let queries = queries_v2();
    let engines = [
        (SearchEngine::Legacy, "legacy"),
        (SearchEngine::Lexical, "lexical"),
        (SearchEngine::Hybrid, "hybrid"),
    ];

    let mut all_metrics: Vec<QueryMetrics> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for (engine, engine_label) in &engines {
        for bq in &queries {
            let metrics =
                evaluate_query_with_engine(&corpus, bq, engine.clone(), engine_label);

            if !metrics.pass {
                let mut fail_msg = format!("[{}/{}] FAIL:", engine_label, bq.label);
                for f in &metrics.failures {
                    let _ = write!(fail_msg, " {f};");
                }
                failures.push(fail_msg);
            }

            all_metrics.push(metrics);
        }
    }

    // Print per-engine summary
    eprintln!("\n╔══════════════════════════════════════════════════════════════════════════════╗");
    eprintln!("║  Mode-Stratified Relevance Benchmark (corpus v2.0.0)                       ║");
    eprintln!("╠══════════════════════════════════════════════════════════════════════════════╣");
    for (_, engine_label) in &engines {
        let engine_metrics: Vec<&QueryMetrics> = all_metrics
            .iter()
            .filter(|m| m.engine == *engine_label)
            .collect();
        let n = engine_metrics.len() as f64;
        let mean_ndcg5 = engine_metrics.iter().map(|m| m.ndcg5).sum::<f64>() / n;
        let mean_mrr = engine_metrics.iter().map(|m| m.mrr).sum::<f64>() / n;
        let mean_p3 = engine_metrics.iter().map(|m| m.precision3).sum::<f64>() / n;
        let mean_r5 = engine_metrics.iter().map(|m| m.recall5).sum::<f64>() / n;
        let pass_count = engine_metrics.iter().filter(|m| m.pass).count();
        eprintln!(
            "║  Engine {:<10} NDCG@5={:.3}  MRR={:.3}  P@3={:.3}  R@5={:.3}  pass={}/{}",
            engine_label,
            mean_ndcg5,
            mean_mrr,
            mean_p3,
            mean_r5,
            pass_count,
            engine_metrics.len(),
        );
    }
    eprintln!("╚══════════════════════════════════════════════════════════════════════════════╝");

    // JSON report
    let json_report = generate_json_report(&all_metrics);
    eprintln!("\nMODE_STRATIFIED_REPORT_JSON_BEGIN");
    eprintln!("{json_report}");
    eprintln!("MODE_STRATIFIED_REPORT_JSON_END");

    if !failures.is_empty() {
        eprintln!("\n=== FAILURES ({}) ===", failures.len());
        for f in &failures {
            eprintln!("{f}");
        }
        panic!(
            "{} quality threshold(s) failed across {} engine×query evaluations.",
            failures.len(),
            all_metrics.len()
        );
    }
}

/// Regression gate: aggregate metrics must meet minimum thresholds.
///
/// These thresholds represent the quality floor. If any engine's aggregate
/// drops below these, the CI build should fail.
#[test]
fn relevance_regression_gate() {
    let corpus = seed_corpus();
    let queries = queries_v2();

    // Run through default engine
    let metrics: Vec<QueryMetrics> = queries
        .iter()
        .map(|bq| evaluate_query_simple(&corpus, bq))
        .collect();

    let n = metrics.len() as f64;
    let mean_ndcg5 = metrics.iter().map(|m| m.ndcg5).sum::<f64>() / n;
    let mean_mrr = metrics.iter().map(|m| m.mrr).sum::<f64>() / n;
    let mean_p3 = metrics.iter().map(|m| m.precision3).sum::<f64>() / n;
    let mean_r5 = metrics.iter().map(|m| m.recall5).sum::<f64>() / n;
    let pass_rate = metrics.iter().filter(|m| m.pass).count() as f64 / n;

    // Regression thresholds — these are the minimum acceptable aggregate values.
    // Tighten these as search quality improves.
    const MIN_MEAN_NDCG5: f64 = 0.70;
    const MIN_MEAN_MRR: f64 = 0.65;
    const MIN_MEAN_P3: f64 = 0.30;
    const MIN_MEAN_R5: f64 = 0.50;
    const MIN_PASS_RATE: f64 = 0.80;

    eprintln!("\n=== Regression Gate ===");
    eprintln!("mean_NDCG@5 = {mean_ndcg5:.3} (min: {MIN_MEAN_NDCG5:.3})");
    eprintln!("mean_MRR    = {mean_mrr:.3} (min: {MIN_MEAN_MRR:.3})");
    eprintln!("mean_P@3    = {mean_p3:.3} (min: {MIN_MEAN_P3:.3})");
    eprintln!("mean_R@5    = {mean_r5:.3} (min: {MIN_MEAN_R5:.3})");
    eprintln!("pass_rate   = {pass_rate:.3} (min: {MIN_PASS_RATE:.3})");

    let mut gate_failures = Vec::new();
    if mean_ndcg5 < MIN_MEAN_NDCG5 {
        gate_failures.push(format!(
            "mean_NDCG@5 {mean_ndcg5:.3} < {MIN_MEAN_NDCG5:.3}"
        ));
    }
    if mean_mrr < MIN_MEAN_MRR {
        gate_failures.push(format!("mean_MRR {mean_mrr:.3} < {MIN_MEAN_MRR:.3}"));
    }
    if mean_p3 < MIN_MEAN_P3 {
        gate_failures.push(format!("mean_P@3 {mean_p3:.3} < {MIN_MEAN_P3:.3}"));
    }
    if mean_r5 < MIN_MEAN_R5 {
        gate_failures.push(format!("mean_R@5 {mean_r5:.3} < {MIN_MEAN_R5:.3}"));
    }
    if pass_rate < MIN_PASS_RATE {
        gate_failures.push(format!(
            "pass_rate {pass_rate:.3} < {MIN_PASS_RATE:.3}"
        ));
    }

    if !gate_failures.is_empty() {
        panic!(
            "Regression gate FAILED:\n{}",
            gate_failures.join("\n")
        );
    }
}

/// Recency ordering: results in recency mode must be in descending timestamp order.
#[test]
fn relevance_recency_ordering() {
    let corpus = seed_corpus();
    let p = corpus.pool.clone();
    let pid = corpus.project_id;

    let results: SearchResponse = block_on(move |cx| async move {
        let query = SearchQuery {
            text: String::new(),
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            limit: Some(20),
            ranking: RankingMode::Recency,
            ..SearchQuery::default()
        };
        match execute_search_simple(&cx, &p, &query).await {
            Outcome::Ok(resp) => resp,
            other => panic!("recency search failed: {other:?}"),
        }
    });

    let timestamps: Vec<Option<i64>> = results.results.iter().map(|r| r.created_ts).collect();
    for window in timestamps.windows(2) {
        if let (Some(a), Some(b)) = (window[0], window[1]) {
            assert!(a >= b, "recency ordering violated: {a} < {b}");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Metric self-tests
// ────────────────────────────────────────────────────────────────────

#[test]
fn metric_ndcg_perfect() {
    let ranked = vec![3.0, 2.0, 1.0, 0.0];
    let ideal = vec![3.0, 2.0, 1.0, 0.0];
    let score = ndcg_at_k(&ranked, &ideal, 4);
    assert!((score - 1.0).abs() < 1e-9, "perfect NDCG should be 1.0, got {score}");
}

#[test]
fn metric_ndcg_reversed() {
    let ranked = vec![0.0, 1.0, 2.0, 3.0];
    let ideal = vec![3.0, 2.0, 1.0, 0.0];
    let score = ndcg_at_k(&ranked, &ideal, 4);
    assert!(score < 0.8, "reversed NDCG should be < 0.8, got {score}");
    assert!(score > 0.0, "reversed NDCG should be > 0.0");
}

#[test]
fn metric_ndcg_empty() {
    assert!((ndcg_at_k(&[], &[], 5) - 1.0).abs() < 1e-9);
}

#[test]
fn metric_mrr_first() {
    assert!((mrr(&[3.0, 0.0, 0.0]) - 1.0).abs() < 1e-9);
}

#[test]
fn metric_mrr_third() {
    assert!((mrr(&[0.0, 0.0, 2.0]) - 1.0 / 3.0).abs() < 1e-9);
}

#[test]
fn metric_mrr_none() {
    assert!(mrr(&[0.0, 0.0, 0.0]).abs() < 1e-9);
}

#[test]
fn metric_precision_all_relevant() {
    assert!((precision_at_k(&[2.0, 3.0, 1.0], 3) - 1.0).abs() < 1e-9);
}

#[test]
fn metric_precision_none() {
    assert!(precision_at_k(&[0.0, 0.0, 0.0], 3).abs() < 1e-9);
}

#[test]
fn metric_precision_mixed() {
    assert!((precision_at_k(&[1.0, 0.0, 2.0, 0.0], 4) - 0.5).abs() < 1e-9);
}

#[test]
fn metric_recall_full() {
    assert!((recall_at_k(&[2.0, 1.0, 3.0], 3, 3) - 1.0).abs() < 1e-9);
}

#[test]
fn metric_recall_partial() {
    assert!((recall_at_k(&[2.0, 0.0, 0.0], 3, 3) - 1.0 / 3.0).abs() < 1e-9);
}

#[test]
fn metric_recall_empty_relevant() {
    assert!((recall_at_k(&[0.0, 0.0], 0, 5) - 1.0).abs() < 1e-9);
}
