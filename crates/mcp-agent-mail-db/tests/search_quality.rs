//! Search quality benchmark corpus + relevance tuning harness (br-3vwi.2.5).
//!
//! Provides a deterministic, versioned relevance benchmark corpus for global
//! mail/project search. Tests ranking quality via `NDCG@k`, `MRR`, and `precision@k`
//! against hand-labeled relevance judgments.
//!
//! The corpus simulates realistic agent coordination traffic: thread IDs,
//! agent names, project names, mixed keyword+facet queries, typo/partial
//! queries, and negative cases.

#![allow(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::missing_const_for_fn
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::config::SearchEngine;
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::search_planner::{DocKind, Importance, RankingMode, SearchQuery};
use mcp_agent_mail_db::search_service::{SearchOptions, execute_search, execute_search_simple};
use mcp_agent_mail_db::search_v3;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tantivy::doc;

// ────────────────────────────────────────────────────────────────────
// Test infrastructure
// ────────────────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);
const BR_BEAD_ID: &str = "br-2tnl.7.4";
const CORPUS_VERSION: &str = "1.0.0";
const QUERY_SET_VERSION: &str = "1.0.0";

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
        .join(format!("search_quality_{}.db", unique_suffix()));
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
    /// Not relevant at all.
    NotRelevant = 0,
    /// Marginally relevant (mentions topic).
    Marginal = 1,
    /// Relevant (directly about the topic).
    Relevant = 2,
    /// Highly relevant (authoritative answer).
    Highly = 3,
}

impl Relevance {
    fn gain(self) -> f64 {
        f64::from(self as u8)
    }
}

/// A single benchmark query with expected relevance judgments.
struct BenchmarkQuery {
    /// Human-readable label for diagnostics.
    label: &'static str,
    /// The search text (what an operator types).
    text: &'static str,
    /// Facet filters (optional).
    importance: Vec<Importance>,
    /// Expected relevance judgments: message subject → grade.
    /// Messages not listed are assumed `NotRelevant`.
    judgments: Vec<(&'static str, Relevance)>,
    /// Minimum acceptable `NDCG@5` for this query.
    min_ndcg5: f64,
    /// Minimum acceptable `precision@3` for this query.
    min_precision3: f64,
}

/// Compute `NDCG@k` (Normalized Discounted Cumulative Gain).
///
/// `ranked_relevances` is the relevance score for each result position.
/// `ideal_relevances` is the sorted-descending ideal ordering.
fn ndcg_at_k(ranked_relevances: &[f64], ideal_relevances: &[f64], k: usize) -> f64 {
    let dcg = dcg_at_k(ranked_relevances, k);
    let idcg = dcg_at_k(ideal_relevances, k);
    if idcg == 0.0 {
        return if dcg == 0.0 { 1.0 } else { 0.0 };
    }
    dcg / idcg
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

/// Mean Reciprocal Rank: `1/position` of the first relevant result.
fn mrr(ranked_relevances: &[f64]) -> f64 {
    for (i, &rel) in ranked_relevances.iter().enumerate() {
        if rel > 0.0 {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// `Precision@k`: fraction of top-k results that are relevant.
fn precision_at_k(ranked_relevances: &[f64], k: usize) -> f64 {
    let relevant = ranked_relevances
        .iter()
        .take(k)
        .filter(|&&r| r > 0.0)
        .count();
    relevant as f64 / k.min(ranked_relevances.len()).max(1) as f64
}

/// `Recall@k`: fraction of all relevant docs found in top-k.
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

/// Quality report for a single query.
#[derive(Debug)]
struct QueryReport {
    mode: SearchQualityMode,
    label: &'static str,
    ndcg5: f64,
    mrr: f64,
    precision3: f64,
    recall5: f64,
    result_count: usize,
    method: String,
    rerank_outcome: Option<String>,
    top_results: Vec<String>,
    ranking_explanation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SearchQualityMode {
    Legacy,
    Lexical,
    Semantic,
    Hybrid,
    HybridRerank,
}

impl SearchQualityMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Lexical => "lexical",
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
            Self::HybridRerank => "hybrid_rerank",
        }
    }

    const fn engine(self) -> SearchEngine {
        match self {
            Self::Legacy => SearchEngine::Legacy,
            Self::Lexical => SearchEngine::Lexical,
            Self::Semantic => SearchEngine::Semantic,
            Self::Hybrid | Self::HybridRerank => SearchEngine::Hybrid,
        }
    }

    const fn threshold_multiplier(self) -> f64 {
        match self {
            Self::Legacy => 1.0,
            Self::Lexical | Self::Hybrid => 0.8,
            Self::Semantic => 0.7,
            Self::HybridRerank => 0.75,
        }
    }

    const fn aggregate_min_ndcg5(self) -> f64 {
        match self {
            Self::Legacy => 0.60,
            Self::Lexical | Self::Hybrid => 0.45,
            Self::Semantic | Self::HybridRerank => 0.40,
        }
    }

    const fn aggregate_min_mrr(self) -> f64 {
        match self {
            Self::Legacy => 0.55,
            Self::Lexical | Self::Hybrid => 0.40,
            Self::Semantic | Self::HybridRerank => 0.35,
        }
    }

    const fn aggregate_min_recall5(self) -> f64 {
        match self {
            Self::Legacy => 0.50,
            Self::Lexical | Self::Hybrid => 0.35,
            Self::Semantic | Self::HybridRerank => 0.30,
        }
    }
}

#[derive(Debug, Serialize)]
struct QueryModeArtifact {
    mode: SearchQualityMode,
    requested_engine: String,
    query_label: &'static str,
    query_text: &'static str,
    ndcg5: f64,
    mrr: f64,
    precision3: f64,
    recall5: f64,
    result_count: usize,
    min_ndcg5: f64,
    min_precision3: f64,
    method: String,
    rerank_outcome: Option<String>,
    top_results: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ModeAggregateArtifact {
    mode: SearchQualityMode,
    requested_engine: String,
    queries: usize,
    mean_ndcg5: f64,
    mean_mrr: f64,
    mean_precision3: f64,
    mean_recall5: f64,
    min_ndcg5_threshold: f64,
    min_mrr_threshold: f64,
    min_recall5_threshold: f64,
}

#[derive(Debug, Serialize)]
struct SearchQualityArtifact {
    generated_at: String,
    bead: &'static str,
    corpus_version: &'static str,
    query_set_version: &'static str,
    mode_reports: Vec<QueryModeArtifact>,
    mode_aggregates: Vec<ModeAggregateArtifact>,
}

// ────────────────────────────────────────────────────────────────────
// Corpus definition
// ────────────────────────────────────────────────────────────────────

/// A message in the benchmark corpus.
struct CorpusMessage {
    subject: &'static str,
    body: &'static str,
    thread_id: Option<&'static str>,
    importance: &'static str,
    ack_required: bool,
    sender_name: &'static str,
}

/// Return the full benchmark corpus (deterministic, versioned).
///
/// Corpus version: 1.0.0
/// Last updated: 2026-02-10
fn corpus_v1() -> Vec<CorpusMessage> {
    vec![
        // ── Thread: br-42 (file reservation conflict resolution) ──────
        CorpusMessage {
            subject: "[br-42] Start: file reservation conflict in storage layer",
            body: "RedHarbor claiming br-42. The storage crate has a file reservation conflict when two agents attempt exclusive locks on overlapping glob patterns. Root cause is symmetric fnmatch not handling ** vs * correctly. Will fix in mcp-agent-mail-guard.",
            thread_id: Some("br-42"),
            importance: "high",
            ack_required: true,
            sender_name: "RedHarbor",
        },
        CorpusMessage {
            subject: "[br-42] Progress: guard fnmatch fixed, tests passing",
            body: "RedHarbor here. Fixed the symmetric fnmatch comparison in guard.rs. Now ** correctly matches nested paths. Added 12 unit tests covering edge cases: trailing slashes, dot-prefixed files, empty patterns. All 34 guard tests green.",
            thread_id: Some("br-42"),
            importance: "normal",
            ack_required: false,
            sender_name: "RedHarbor",
        },
        CorpusMessage {
            subject: "[br-42] Completed: reservation conflict resolution merged",
            body: "RedHarbor: guard fix merged. Commit abc123. Closing br-42. The fnmatch symmetric comparison now correctly handles all glob patterns including ** for recursive directory matching.",
            thread_id: Some("br-42"),
            importance: "normal",
            ack_required: false,
            sender_name: "RedHarbor",
        },
        // ── Thread: br-99 (SQLite WAL tuning) ──────────────────────
        CorpusMessage {
            subject: "[br-99] Start: SQLite WAL checkpoint tuning for concurrent writes",
            body: "Taking br-99. Under heavy concurrent message sending (8+ threads), WAL file grows unbounded because auto-checkpoint threshold is too high. Plan: reduce wal_autocheckpoint from 2000 to 500, add explicit checkpoint after bulk operations, monitor WAL size in health metrics.",
            thread_id: Some("br-99"),
            importance: "high",
            ack_required: true,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "[br-99] Progress: WAL checkpoint tuning benchmarks",
            body: "Benchmarked three checkpoint strategies:\n1. wal_autocheckpoint=500 (default tuned): 15% write latency improvement\n2. Explicit PRAGMA wal_checkpoint(TRUNCATE) after bulk: 40% WAL size reduction\n3. Passive checkpoint in background thread: best throughput but complex\n\nRecommending option 2 for simplicity. SQLite busy_timeout stays at 60s.",
            thread_id: Some("br-99"),
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "[br-99] Completed: WAL tuning deployed with monitoring",
            body: "Deployed WAL checkpoint tuning. wal_autocheckpoint=500, explicit TRUNCATE checkpoint after bulk message inserts. Added WAL size metric to SystemHealth screen. Pool exhaustion recovery test updated.",
            thread_id: Some("br-99"),
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        // ── Thread: br-150 (TUI search cockpit) ────────────────────
        CorpusMessage {
            subject: "[br-150] Start: Search Cockpit v2 query bar implementation",
            body: "Starting the Search Cockpit v2 query bar. This implements the faceted search UI with importance filters, direction toggles, and agent name autocomplete. Uses frankentui InputField widget with custom keybindings for tab-completion.",
            thread_id: Some("br-150"),
            importance: "high",
            ack_required: true,
            sender_name: "GoldHawk",
        },
        CorpusMessage {
            subject: "[br-150] Design: facet rail interaction model",
            body: "Facet rail design:\n- Left sidebar: importance chips (low/normal/high/urgent), direction toggle (inbox/outbox/all), thread filter\n- Query bar: FTS5 text input with live preview count\n- Results: virtualized list with snippet highlighting\n- Deep links: Ctrl+Enter opens message detail, Shift+Enter opens thread\n\nKeyboard-first: Tab cycles facets, Enter applies, Esc clears.",
            thread_id: Some("br-150"),
            importance: "normal",
            ack_required: false,
            sender_name: "GoldHawk",
        },
        // ── Thread: perf-review (performance discussion) ────────────
        CorpusMessage {
            subject: "Performance regression in message delivery pipeline",
            body: "Noticed p95 message delivery latency increased from 12ms to 45ms after the commit coalescer changes. The sharded worker pool seems to have a hot-shard problem when most messages go to the same project. Need to investigate hash distribution.",
            thread_id: Some("perf-review"),
            importance: "urgent",
            ack_required: true,
            sender_name: "SilverLake",
        },
        CorpusMessage {
            subject: "Re: Performance regression in message delivery pipeline",
            body: "Confirmed the hot-shard issue. Project /data/main gets 80% of traffic but only 1 of 4 shards. Fix options:\n1. Weighted hashing based on message volume\n2. Round-robin within same project\n3. Separate fast-path for high-volume projects\n\nI'll prototype option 1 first.",
            thread_id: Some("perf-review"),
            importance: "high",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "Re: Performance regression in message delivery pipeline",
            body: "Weighted hashing deployed. P95 back down to 18ms. Not quite at the original 12ms but acceptable. The remaining latency is from WAL contention during checkpoint windows. Closing this thread.",
            thread_id: Some("perf-review"),
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        // ── Thread: coord-agents (agent coordination) ───────────────
        CorpusMessage {
            subject: "Agent coordination: who is working on what?",
            body: "Quick roll call. I'm working on the TUI dashboard sparkline widget. Need to know if anyone is touching tui_app.rs or tui_chrome.rs so I can reserve properly.",
            thread_id: Some("coord-agents"),
            importance: "normal",
            ack_required: true,
            sender_name: "GreenMeadow",
        },
        CorpusMessage {
            subject: "Re: Agent coordination: who is working on what?",
            body: "RedHarbor here. I'm on the CLI integration tests. Not touching any TUI files. My reservations are limited to mcp-agent-mail-cli/src/** and scripts/. You're safe to reserve the TUI surface.",
            thread_id: Some("coord-agents"),
            importance: "normal",
            ack_required: false,
            sender_name: "RedHarbor",
        },
        // ── Thread: migration-v3 (database migration) ───────────────
        CorpusMessage {
            subject: "v3 migration: converting TEXT timestamps to i64 microseconds",
            body: "The v3 migration converts legacy Python TEXT timestamps (ISO-8601 via SQLAlchemy DATETIME) to Rust i64 microseconds. Uses strftime('%s') * 1000000 plus fractional microsecond extraction. DATETIME column type has NUMERIC affinity so existing integers are preserved.",
            thread_id: Some("migration-v3"),
            importance: "high",
            ack_required: true,
            sender_name: "SilverLake",
        },
        CorpusMessage {
            subject: "Re: v3 migration: timestamp edge cases",
            body: "Found edge case: pre-1970 timestamps produce negative values. Need to use div_euclid/rem_euclid instead of regular division for correct microsecond extraction. Added test vector for 1969-12-31T23:59:59.999999Z.",
            thread_id: Some("migration-v3"),
            importance: "normal",
            ack_required: false,
            sender_name: "SilverLake",
        },
        // ── Thread: security-audit (security review) ────────────────
        CorpusMessage {
            subject: "Security audit: FTS5 query injection prevention",
            body: "Reviewed the FTS5 query sanitization in sanitize_fts_query(). Current protections:\n- Strip leading wildcards\n- Quote hyphenated tokens\n- Reject bare boolean operators\n- LIKE fallback for malformed queries\n\nNo SQL injection risk because FTS5 MATCH is parameterized. The sanitizer prevents FTS5 syntax errors, not injection.",
            thread_id: Some("security-audit"),
            importance: "high",
            ack_required: false,
            sender_name: "GoldHawk",
        },
        CorpusMessage {
            subject: "Security audit: path traversal in thread_id",
            body: "RedHarbor: found and fixed path traversal vulnerability in sanitize_thread_id(). Thread IDs are used as directory names in the Git archive. An attacker could use ../../../etc/passwd as a thread_id to escape the archive root. Fix: reject any thread_id containing .. or absolute path prefixes.",
            thread_id: Some("security-audit"),
            importance: "urgent",
            ack_required: true,
            sender_name: "RedHarbor",
        },
        // ── Standalone messages (no thread) ──────────────────────────
        CorpusMessage {
            subject: "conformance test results: all 23 tools passing",
            body: "Full conformance run against Python fixtures: 23/23 tools passing, 23+ resources matching. Format parity verified for ToolDirectory, ToolSchemasResponse, LocksResponse, and all view resources. No regressions.",
            thread_id: None,
            importance: "normal",
            ack_required: false,
            sender_name: "GreenMeadow",
        },
        CorpusMessage {
            subject: "E2E test suite expansion: 44 share assertions added",
            body: "Added 44 new E2E assertions for the share/export pipeline. Covers snapshot creation, scrub presets, bundle encryption, and deterministic archive hashing. All tests green.",
            thread_id: None,
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "clippy warning cleanup: 0 warnings workspace-wide",
            body: "Cleaned up all remaining clippy warnings across the workspace. Key fixes: unnecessary clone in search_planner, unused import in tui_chrome, redundant pattern match in guard conflict detection. Pedantic + nursery lints all clear.",
            thread_id: None,
            importance: "normal",
            ack_required: false,
            sender_name: "SilverLake",
        },
        CorpusMessage {
            subject: "FTS5 tokenizer configuration for multilingual support",
            body: "Current FTS5 config uses porter unicode61 with remove_diacritics=2 and prefix='2,3'. This handles English stemming and accented characters well. For CJK support we'd need the ICU tokenizer, but that requires linking libicu. Deferring CJK to a future bead.",
            thread_id: None,
            importance: "normal",
            ack_required: false,
            sender_name: "GoldHawk",
        },
        CorpusMessage {
            subject: "write-behind cache coherency issue under concurrent load",
            body: "Discovered that the ReadCache can serve stale project data when ensure_project races with a concurrent register_agent. The dual-indexed cache (slug+human_key, key+id) needs invalidation on agent registration. Fix: add cache.invalidate_project(id) call in register_agent path.",
            thread_id: Some("cache-coherency"),
            importance: "high",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        CorpusMessage {
            subject: "Re: write-behind cache coherency issue under concurrent load",
            body: "Cache invalidation fix deployed. Added targeted invalidation in register_agent, create_message, and create_file_reservations. Stress test cache_coherency_mixed_workload now passes consistently (was flaky before).",
            thread_id: Some("cache-coherency"),
            importance: "normal",
            ack_required: false,
            sender_name: "BlueCastle",
        },
        // ── Thread: attachment-pipeline ──────────────────────────────
        CorpusMessage {
            subject: "Attachment pipeline: WebP conversion + inline mode",
            body: "Implemented the attachment processing pipeline. Three modes: inline (base64 in body), file (saved to archive), auto (size-based threshold at 256KB). WebP conversion for images reduces storage by ~60%. Seven unit tests covering all modes plus error cases.",
            thread_id: Some("attachment-pipeline"),
            importance: "normal",
            ack_required: false,
            sender_name: "GreenMeadow",
        },
        // ── Thread: circuit-breaker ──────────────────────────────────
        CorpusMessage {
            subject: "Circuit breaker implementation for database pool",
            body: "Added circuit breaker pattern to DbPool. States: Closed (normal), Open (all requests fail-fast), HalfOpen (probe with single request). Trips after 5 consecutive failures. Reset timeout: 30 seconds. Health check integration sends probe query on half-open transition.",
            thread_id: Some("circuit-breaker"),
            importance: "high",
            ack_required: false,
            sender_name: "SilverLake",
        },
        CorpusMessage {
            subject: "Re: Circuit breaker: pool exhaustion recovery test",
            body: "The pool_exhaustion_recovery stress test validates circuit breaker behavior under 3-connection pool with 12 concurrent threads. Verifies: breaker trips after pool saturation, fail-fast responses during open state, successful recovery after timeout, no leaked connections.",
            thread_id: Some("circuit-breaker"),
            importance: "normal",
            ack_required: false,
            sender_name: "SilverLake",
        },
        // ── Red herrings / noise messages ────────────────────────────
        CorpusMessage {
            subject: "Weekly status: nothing to report",
            body: "No blockers. Continuing with assigned beads. Will update when there's something noteworthy.",
            thread_id: Some("weekly-status"),
            importance: "low",
            ack_required: false,
            sender_name: "GreenMeadow",
        },
        CorpusMessage {
            subject: "Out of office: vacation next week",
            body: "RedHarbor will be unavailable from Monday through Friday. My current reservations will expire naturally. Please don't force-release them before Thursday.",
            thread_id: Some("ooo"),
            importance: "low",
            ack_required: false,
            sender_name: "RedHarbor",
        },
        CorpusMessage {
            subject: "Question: should we use tokio or asupersync?",
            body: "The project mandate is to use asupersync (not tokio) for the async runtime. This is a firm requirement from the project lead. asupersync provides Cx-based context propagation, Budget-aware cancellation, and testing utilities that tokio does not.",
            thread_id: Some("tech-questions"),
            importance: "normal",
            ack_required: false,
            sender_name: "GoldHawk",
        },
    ]
}

/// Return the benchmark query set with relevance judgments.
///
/// Query set version: 1.0.0
/// Last updated: 2026-02-10
fn queries_v1() -> Vec<BenchmarkQuery> {
    use Relevance::*;
    vec![
        // ── Q1: Exact thread ID lookup ──────────────────────────────
        BenchmarkQuery {
            label: "exact_thread_id_br42",
            text: "br-42",
            importance: vec![],
            judgments: vec![
                (
                    "[br-42] Start: file reservation conflict in storage layer",
                    Highly,
                ),
                (
                    "[br-42] Progress: guard fnmatch fixed, tests passing",
                    Highly,
                ),
                (
                    "[br-42] Completed: reservation conflict resolution merged",
                    Highly,
                ),
            ],
            min_ndcg5: 0.8,
            min_precision3: 1.0,
        },
        // ── Q2: Topic search — WAL / SQLite tuning ─────────────────
        BenchmarkQuery {
            label: "topic_wal_tuning",
            text: "WAL checkpoint SQLite",
            importance: vec![],
            judgments: vec![
                (
                    "[br-99] Start: SQLite WAL checkpoint tuning for concurrent writes",
                    Highly,
                ),
                ("[br-99] Progress: WAL checkpoint tuning benchmarks", Highly),
                (
                    "[br-99] Completed: WAL tuning deployed with monitoring",
                    Relevant,
                ),
                // tangentially related:
                (
                    "Performance regression in message delivery pipeline",
                    Marginal,
                ),
                (
                    "Re: Performance regression in message delivery pipeline",
                    Marginal,
                ),
            ],
            min_ndcg5: 0.7,
            min_precision3: 0.66,
        },
        // ── Q3: Agent name search ───────────────────────────────────
        BenchmarkQuery {
            label: "agent_name_redharbor",
            text: "RedHarbor",
            importance: vec![],
            judgments: vec![
                (
                    "[br-42] Start: file reservation conflict in storage layer",
                    Highly,
                ),
                (
                    "[br-42] Progress: guard fnmatch fixed, tests passing",
                    Highly,
                ),
                (
                    "[br-42] Completed: reservation conflict resolution merged",
                    Highly,
                ),
                ("Re: Agent coordination: who is working on what?", Relevant),
                ("Security audit: path traversal in thread_id", Relevant),
                ("Out of office: vacation next week", Marginal),
            ],
            min_ndcg5: 0.6,
            min_precision3: 0.66,
        },
        // ── Q4: Security-focused search ─────────────────────────────
        BenchmarkQuery {
            label: "security_topics",
            text: "security audit",
            importance: vec![],
            judgments: vec![
                ("Security audit: FTS5 query injection prevention", Highly),
                ("Security audit: path traversal in thread_id", Highly),
            ],
            min_ndcg5: 0.8,
            min_precision3: 0.66,
        },
        // ── Q5: Performance regression ──────────────────────────────
        BenchmarkQuery {
            label: "performance_regression",
            text: "performance regression latency",
            importance: vec![],
            judgments: vec![
                (
                    "Performance regression in message delivery pipeline",
                    Highly,
                ),
                (
                    "Re: Performance regression in message delivery pipeline",
                    Highly,
                ),
            ],
            min_ndcg5: 0.5,
            min_precision3: 0.33,
        },
        // ── Q6: Importance-faceted search (high + urgent only) ──────
        BenchmarkQuery {
            label: "high_importance_circuit_breaker",
            text: "circuit breaker",
            importance: vec![Importance::High, Importance::Urgent],
            judgments: vec![
                ("Circuit breaker implementation for database pool", Highly),
                // the "Re:" is normal importance, should be filtered out
            ],
            min_ndcg5: 0.8,
            min_precision3: 1.0,
        },
        // ── Q7: Cache coherency search ──────────────────────────────
        BenchmarkQuery {
            label: "cache_coherency",
            text: "cache coherency invalidation",
            importance: vec![],
            judgments: vec![
                (
                    "write-behind cache coherency issue under concurrent load",
                    Highly,
                ),
                (
                    "Re: write-behind cache coherency issue under concurrent load",
                    Highly,
                ),
            ],
            min_ndcg5: 0.6,
            min_precision3: 0.33,
        },
        // ── Q8: Partial/prefix search ───────────────────────────────
        BenchmarkQuery {
            label: "prefix_conform",
            text: "conform*",
            importance: vec![],
            judgments: vec![("conformance test results: all 23 tools passing", Highly)],
            min_ndcg5: 0.8,
            min_precision3: 0.33,
        },
        // ── Q9: Boolean compound query ──────────────────────────────
        BenchmarkQuery {
            label: "fts_guard_tests",
            text: "guard fnmatch tests",
            importance: vec![],
            judgments: vec![
                (
                    "[br-42] Progress: guard fnmatch fixed, tests passing",
                    Highly,
                ),
                (
                    "[br-42] Start: file reservation conflict in storage layer",
                    Relevant,
                ),
                (
                    "[br-42] Completed: reservation conflict resolution merged",
                    Relevant,
                ),
            ],
            min_ndcg5: 0.5,
            min_precision3: 0.33,
        },
        // ── Q10: Negative case — no relevant results ────────────────
        BenchmarkQuery {
            label: "negative_no_results",
            text: "kubernetes docker container orchestration",
            importance: vec![],
            judgments: vec![],
            min_ndcg5: 1.0, // vacuously true (no ideal docs)
            min_precision3: 0.0,
        },
        // ── Q11: Migration / timestamp search ───────────────────────
        BenchmarkQuery {
            label: "migration_timestamps",
            text: "migration timestamp",
            importance: vec![],
            judgments: vec![
                (
                    "v3 migration: converting TEXT timestamps to i64 microseconds",
                    Highly,
                ),
                ("Re: v3 migration: timestamp edge cases", Highly),
            ],
            min_ndcg5: 0.7,
            min_precision3: 0.66,
        },
        // ── Q12: Attachment pipeline ────────────────────────────────
        BenchmarkQuery {
            label: "attachment_webp",
            text: "attachment WebP conversion pipeline",
            importance: vec![],
            judgments: vec![("Attachment pipeline: WebP conversion + inline mode", Highly)],
            min_ndcg5: 0.8,
            min_precision3: 0.33,
        },
    ]
}

// ────────────────────────────────────────────────────────────────────
// Corpus seeding
// ────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct SeededCorpus {
    pool: DbPool,
    project_id: i64,
    /// Map from `sender_name` → `agent_id`.
    agents: HashMap<String, i64>,
    /// Map from message subject → `message_id`.
    message_ids: HashMap<String, i64>,
    _dir: tempfile::TempDir,
}

fn ensure_v3_tantivy_index(corpus: &SeededCorpus) {
    let index_dir = std::env::temp_dir().join(format!(
        "am_search_quality_v3_index_{}_{}",
        unique_suffix(),
        std::process::id()
    ));
    std::fs::create_dir_all(&index_dir).expect("create Tantivy index directory");
    search_v3::init_bridge(Path::new(&index_dir)).expect("init Tantivy bridge");

    let bridge = search_v3::get_bridge().expect("Tantivy bridge initialized");
    let handles = bridge.handles();
    let project_id = u64::try_from(corpus.project_id).expect("project_id should be non-negative");
    let mut writer = bridge
        .index()
        .writer(64_000_000)
        .expect("create Tantivy writer");

    let base_ts = mcp_agent_mail_db::now_micros();
    for (idx, msg) in corpus_v1().iter().enumerate() {
        let row_id = *corpus
            .message_ids
            .get(msg.subject)
            .unwrap_or_else(|| panic!("missing seeded id for subject: {}", msg.subject));
        let doc_id = u64::try_from(row_id).expect("message id should be non-negative");
        let created_ts = base_ts + i64::try_from(idx).expect("idx fits i64");

        writer
            .add_document(doc!(
                handles.id => doc_id,
                handles.doc_kind => "message",
                handles.subject => msg.subject,
                handles.body => msg.body,
                handles.sender => msg.sender_name,
                handles.project_slug => "bench-search-quality",
                handles.project_id => project_id,
                handles.thread_id => msg.thread_id.unwrap_or(""),
                handles.importance => msg.importance,
                handles.created_ts => created_ts
            ))
            .expect("add Tantivy benchmark doc");
    }

    writer.commit().expect("commit Tantivy benchmark index");
}

fn facet_value(
    explain: Option<&mcp_agent_mail_db::search_planner::QueryExplain>,
    facet_key: &str,
) -> Option<String> {
    explain.and_then(|meta| {
        meta.facets_applied.iter().find_map(|facet| {
            facet
                .strip_prefix(&format!("{facet_key}:"))
                .map(str::to_string)
        })
    })
}

fn seed_corpus() -> SeededCorpus {
    let (pool, dir) = make_pool();
    let corpus = corpus_v1();

    // Collect unique sender names.
    let sender_names: Vec<&str> = {
        let mut names: Vec<&str> = corpus.iter().map(|m| m.sender_name).collect();
        names.sort_unstable();
        names.dedup();
        names
    };

    // Create project and agents.
    let (project_id, agents) = {
        let p = pool.clone();
        block_on(move |cx| async move {
            let proj = match queries::ensure_project(&cx, &p, "/bench/search-quality").await {
                Outcome::Ok(r) => r,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let pid = proj.id.unwrap();

            let mut agent_map = HashMap::new();
            for name in &sender_names {
                let agent =
                    match queries::register_agent(&cx, &p, pid, name, "bench", "test", None, None)
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

    // Insert all messages.
    let mut message_ids = HashMap::new();
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

        message_ids.insert(subject.to_string(), row.id.unwrap());
    }

    SeededCorpus {
        pool,
        project_id,
        agents,
        message_ids,
        _dir: dir,
    }
}

// ────────────────────────────────────────────────────────────────────
// Query execution + evaluation
// ────────────────────────────────────────────────────────────────────

type QueryEvalOutput = (
    Vec<String>,
    Option<mcp_agent_mail_db::search_planner::QueryExplain>,
    Vec<(String, Option<f64>)>,
);

fn evaluate_query_with_mode(
    corpus: &SeededCorpus,
    bq: &BenchmarkQuery,
    mode: SearchQualityMode,
) -> QueryReport {
    let p = corpus.pool.clone();
    let pid = corpus.project_id;
    let text = bq.text.to_string();
    let importance = bq.importance.clone();

    let (ranked_titles, explain_meta, scored_rows): QueryEvalOutput = block_on(move |cx| async move {
        let mut query = SearchQuery {
            text,
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            importance,
            explain: true,
            limit: Some(20),
            ranking: RankingMode::Relevance,
            ..SearchQuery::default()
        };
        query.limit = Some(20);

        if mode == SearchQualityMode::Legacy {
            let resp = match execute_search_simple(&cx, &p, &query).await {
                Outcome::Ok(resp) => resp,
                other => panic!("search failed for '{}': {other:?}", bq.label),
            };
            let titles = resp.results.iter().map(|r| r.title.clone()).collect();
            let rows = resp
                .results
                .iter()
                .map(|r| (r.title.clone(), r.score))
                .collect();
            (titles, resp.explain, rows)
        } else {
            let opts = SearchOptions {
                scope_ctx: None,
                redaction_policy: None,
                track_telemetry: false,
                search_engine: Some(mode.engine()),
            };
            let resp = match execute_search(&cx, &p, &query, &opts).await {
                Outcome::Ok(resp) => resp,
                other => panic!("v3 search failed for '{}': {other:?}", bq.label),
            };
            let titles = resp
                .results
                .iter()
                .map(|r| r.result.title.clone())
                .collect::<Vec<_>>();
            let rows = resp
                .results
                .iter()
                .map(|r| (r.result.title.clone(), r.result.score))
                .collect::<Vec<_>>();
            (titles, resp.explain, rows)
        }
    });

    // Build relevance map from judgments.
    let judgment_map: HashMap<&str, Relevance> = bq.judgments.iter().copied().collect();

    // Map result titles to relevance scores.
    let ranked_relevances: Vec<f64> = ranked_titles
        .iter()
        .map(|r| {
            judgment_map
                .get(r.as_str())
                .copied()
                .unwrap_or(Relevance::NotRelevant)
                .gain()
        })
        .collect();

    // Compute ideal ordering (sorted descending).
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

    // Build explanation.
    let mut explanation = format!(
        "Mode: {} (requested engine: {})\nQuery: '{}'\nResults returned: {}\n",
        mode.label(),
        mode.engine(),
        bq.text,
        ranked_titles.len()
    );

    if let Some(ref explain) = explain_meta {
        let _ = writeln!(explanation, "Method: {:?}", explain.method);
        if let Some(ref normalized) = explain.normalized_query {
            let _ = writeln!(explanation, "Normalized: {normalized}");
        }
        if let Some(rerank_outcome) = facet_value(Some(explain), "rerank_outcome") {
            let _ = writeln!(explanation, "Rerank outcome: {rerank_outcome}");
        }
    }

    explanation.push_str("Ranking:\n");
    for (i, (title, score)) in scored_rows.iter().take(5).enumerate() {
        let rel = judgment_map
            .get(title.as_str())
            .copied()
            .unwrap_or(Relevance::NotRelevant);
        let score_str = score.map_or_else(|| "n/a".to_owned(), |s| format!("{s:.4}"));
        let _ = writeln!(
            explanation,
            "  #{}: [score={score_str}] [rel={rel:?}] {}",
            i + 1,
            title
        );
    }

    QueryReport {
        mode,
        label: bq.label,
        ndcg5,
        mrr: mrr_val,
        precision3,
        recall5,
        result_count: ranked_titles.len(),
        method: explain_meta
            .as_ref()
            .map_or_else(|| "none".to_string(), |meta| meta.method.clone()),
        rerank_outcome: facet_value(explain_meta.as_ref(), "rerank_outcome"),
        top_results: scored_rows
            .iter()
            .take(5)
            .map(|(title, _)| title.clone())
            .collect(),
        ranking_explanation: explanation,
    }
}

fn evaluate_query(corpus: &SeededCorpus, bq: &BenchmarkQuery) -> QueryReport {
    evaluate_query_with_mode(corpus, bq, SearchQualityMode::Legacy)
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

/// Run the full benchmark suite and validate quality thresholds.
#[test]
fn search_quality_benchmark_full() {
    let corpus = seed_corpus();
    let queries = queries_v1();

    let mut reports: Vec<QueryReport> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for bq in &queries {
        let report = evaluate_query(&corpus, bq);

        // Check thresholds.
        if report.ndcg5 < bq.min_ndcg5 {
            failures.push(format!(
                "[{}] NDCG@5 = {:.3} < min {:.3}\n{}",
                bq.label, report.ndcg5, bq.min_ndcg5, report.ranking_explanation
            ));
        }
        if report.precision3 < bq.min_precision3 {
            failures.push(format!(
                "[{}] P@3 = {:.3} < min {:.3}\n{}",
                bq.label, report.precision3, bq.min_precision3, report.ranking_explanation
            ));
        }

        reports.push(report);
    }

    // Print full report (visible with --nocapture).
    eprintln!("\n╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  Search Quality Benchmark Report (corpus v1.0.0)           ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    for r in &reports {
        eprintln!(
            "║  {:<35} NDCG@5={:.3}  MRR={:.3}  P@3={:.3}  R@5={:.3}  n={}",
            r.label, r.ndcg5, r.mrr, r.precision3, r.recall5, r.result_count,
        );
    }
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // Aggregate metrics.
    let mean_ndcg5: f64 = reports.iter().map(|r| r.ndcg5).sum::<f64>() / reports.len() as f64;
    let mean_mrr: f64 = reports.iter().map(|r| r.mrr).sum::<f64>() / reports.len() as f64;
    let mean_p3: f64 = reports.iter().map(|r| r.precision3).sum::<f64>() / reports.len() as f64;
    eprintln!(
        "\nAggregate: mean_NDCG@5={mean_ndcg5:.3}  mean_MRR={mean_mrr:.3}  mean_P@3={mean_p3:.3}"
    );

    if !failures.is_empty() {
        eprintln!("\n=== FAILURES ===");
        for f in &failures {
            eprintln!("{f}");
        }
        panic!(
            "{} quality threshold(s) failed out of {} queries.\n\
             See ranking explanations above for debugging.",
            failures.len(),
            queries.len()
        );
    }
}

fn save_search_quality_artifact(artifact: &SearchQualityArtifact) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .join(format!(
            "tests/artifacts/search_quality/{ts}_{}",
            std::process::id()
        ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("report.json");
    let json = serde_json::to_string_pretty(artifact).unwrap_or_default();
    let _ = std::fs::write(&path, json);
    eprintln!("search quality artifact: {}", path.display());
}

/// Multi-mode quality harness for Search V3 gates.
///
/// Produces machine-readable per-query/per-mode metrics and validates baseline
/// quality floors for lexical/semantic/hybrid (plus hybrid+rerkank diagnostics).
#[test]
fn search_quality_multimode_harness() {
    let corpus = seed_corpus();
    ensure_v3_tantivy_index(&corpus);
    let queries = queries_v1();
    let modes = [
        SearchQualityMode::Legacy,
        SearchQualityMode::Lexical,
        SearchQualityMode::Semantic,
        SearchQualityMode::Hybrid,
        SearchQualityMode::HybridRerank,
    ];

    let mut failures: Vec<String> = Vec::new();
    let mut mode_reports: Vec<QueryModeArtifact> = Vec::new();
    let mut mode_aggregates: Vec<ModeAggregateArtifact> = Vec::new();

    for mode in modes {
        let mut reports_for_mode = Vec::new();

        for query in &queries {
            let report = evaluate_query_with_mode(&corpus, query, mode);
            let scaled_ndcg5 = (query.min_ndcg5 * mode.threshold_multiplier()).clamp(0.0, 1.0);
            let scaled_p3 = (query.min_precision3 * mode.threshold_multiplier()).clamp(0.0, 1.0);

            if report.ndcg5 < scaled_ndcg5 {
                failures.push(format!(
                    "[{}/{}] NDCG@5={:.3} < {:.3}\n{}",
                    mode.label(),
                    query.label,
                    report.ndcg5,
                    scaled_ndcg5,
                    report.ranking_explanation
                ));
            }
            if report.precision3 < scaled_p3 {
                failures.push(format!(
                    "[{}/{}] P@3={:.3} < {:.3}\n{}",
                    mode.label(),
                    query.label,
                    report.precision3,
                    scaled_p3,
                    report.ranking_explanation
                ));
            }

            mode_reports.push(QueryModeArtifact {
                mode: report.mode,
                requested_engine: mode.engine().to_string(),
                query_label: query.label,
                query_text: query.text,
                ndcg5: report.ndcg5,
                mrr: report.mrr,
                precision3: report.precision3,
                recall5: report.recall5,
                result_count: report.result_count,
                min_ndcg5: scaled_ndcg5,
                min_precision3: scaled_p3,
                method: report.method.clone(),
                rerank_outcome: report.rerank_outcome.clone(),
                top_results: report.top_results.clone(),
            });

            reports_for_mode.push(report);
        }

        let divisor = reports_for_mode.len().max(1) as f64;
        let mean_ndcg5 = reports_for_mode.iter().map(|r| r.ndcg5).sum::<f64>() / divisor;
        let mean_mrr = reports_for_mode.iter().map(|r| r.mrr).sum::<f64>() / divisor;
        let mean_precision3 = reports_for_mode.iter().map(|r| r.precision3).sum::<f64>() / divisor;
        let mean_recall5 = reports_for_mode.iter().map(|r| r.recall5).sum::<f64>() / divisor;

        if mean_ndcg5 < mode.aggregate_min_ndcg5() {
            failures.push(format!(
                "[{}] mean_NDCG@5={:.3} < {:.3}",
                mode.label(),
                mean_ndcg5,
                mode.aggregate_min_ndcg5()
            ));
        }
        if mean_mrr < mode.aggregate_min_mrr() {
            failures.push(format!(
                "[{}] mean_MRR={:.3} < {:.3}",
                mode.label(),
                mean_mrr,
                mode.aggregate_min_mrr()
            ));
        }
        if mean_recall5 < mode.aggregate_min_recall5() {
            failures.push(format!(
                "[{}] mean_Recall@5={:.3} < {:.3}",
                mode.label(),
                mean_recall5,
                mode.aggregate_min_recall5()
            ));
        }

        mode_aggregates.push(ModeAggregateArtifact {
            mode,
            requested_engine: mode.engine().to_string(),
            queries: reports_for_mode.len(),
            mean_ndcg5,
            mean_mrr,
            mean_precision3,
            mean_recall5,
            min_ndcg5_threshold: mode.aggregate_min_ndcg5(),
            min_mrr_threshold: mode.aggregate_min_mrr(),
            min_recall5_threshold: mode.aggregate_min_recall5(),
        });
    }

    let artifact = SearchQualityArtifact {
        generated_at: chrono::Utc::now().to_rfc3339(),
        bead: BR_BEAD_ID,
        corpus_version: CORPUS_VERSION,
        query_set_version: QUERY_SET_VERSION,
        mode_reports,
        mode_aggregates,
    };
    save_search_quality_artifact(&artifact);

    if !failures.is_empty() {
        let joined = failures.join("\n\n");
        panic!(
            "search quality multimode regressions ({}):\n{}",
            failures.len(),
            joined
        );
    }
}

/// Validate that the legacy `search_messages` API also returns reasonable results.
#[test]
fn search_quality_legacy_api_smoke() {
    let corpus = seed_corpus();
    let p = corpus.pool.clone();
    let pid = corpus.project_id;

    // Test a few basic queries through the legacy API.
    let test_cases: Vec<(&str, &str)> = vec![
        (
            "br-42",
            "[br-42] Start: file reservation conflict in storage layer",
        ),
        (
            "WAL checkpoint",
            "[br-99] Start: SQLite WAL checkpoint tuning for concurrent writes",
        ),
        (
            "circuit breaker",
            "Circuit breaker implementation for database pool",
        ),
        (
            "conformance",
            "conformance test results: all 23 tools passing",
        ),
    ];

    for (query, expected_subject) in test_cases {
        let p2 = p.clone();
        let query_str = query.to_string();
        let results = block_on(move |cx| async move {
            match queries::search_messages(&cx, &p2, pid, &query_str, 10).await {
                Outcome::Ok(rows) => rows,
                other => panic!("search_messages('{query_str}') failed: {other:?}"),
            }
        });

        assert!(!results.is_empty(), "query '{query}' returned no results");
        assert!(
            results.iter().any(|r| r.subject == expected_subject),
            "query '{query}': expected '{expected_subject}' in results, got: {:?}",
            results.iter().map(|r| &r.subject).collect::<Vec<_>>()
        );
    }
}

/// Validate LIKE fallback produces results for queries that would break FTS5.
#[test]
fn search_quality_like_fallback() {
    let corpus = seed_corpus();
    let p = corpus.pool.clone();
    let pid = corpus.project_id;

    // These queries should fall back to LIKE because they're problematic for FTS5.
    let fallback_queries: Vec<(&str, bool)> = vec![
        // Bare wildcards → should sanitize or fallback
        ("*", false), // unsearchable, expect empty
        // Single character → too short for meaningful FTS
        ("a", false),
        // Normal query that should work via LIKE fallback if FTS fails
        ("reservation conflict", true),
    ];

    for (query, expect_results) in fallback_queries {
        let p2 = p.clone();
        let q = query.to_string();
        let results = block_on(move |cx| async move {
            match queries::search_messages(&cx, &p2, pid, &q, 10).await {
                Outcome::Ok(rows) => rows,
                other => panic!("search_messages('{q}') failed: {other:?}"),
            }
        });

        if expect_results {
            assert!(
                !results.is_empty(),
                "LIKE fallback query '{query}' should return results"
            );
        }
    }
}

/// Verify ranking order: more specific matches should rank higher.
#[test]
fn search_quality_ranking_order() {
    let corpus = seed_corpus();
    let p = corpus.pool.clone();
    let pid = corpus.project_id;

    // "fnmatch" appears in br-42 thread but not elsewhere.
    // The most relevant result should have "fnmatch" in both subject and body.
    let results = block_on(move |cx| async move {
        let query = SearchQuery {
            text: "fnmatch".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            explain: true,
            limit: Some(10),
            ranking: RankingMode::Relevance,
            ..SearchQuery::default()
        };
        match execute_search_simple(&cx, &p, &query).await {
            Outcome::Ok(resp) => resp,
            other => panic!("search failed: {other:?}"),
        }
    });

    assert!(
        !results.results.is_empty(),
        "fnmatch search should return results"
    );

    // The top result should be the "Progress" message (fnmatch in both subject + body).
    let top = &results.results[0];
    assert!(
        top.title.contains("fnmatch"),
        "top result should contain 'fnmatch' in title, got: {}",
        top.title
    );
}

/// Verify that importance facet filtering works correctly.
#[test]
fn search_quality_facet_importance_filter() {
    let corpus = seed_corpus();
    let p = corpus.pool.clone();
    let pid = corpus.project_id;

    // Search for messages about "reservation" but only urgent importance.
    let results = block_on(move |cx| async move {
        let query = SearchQuery {
            text: "reservation".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            importance: vec![Importance::Urgent],
            limit: Some(10),
            ranking: RankingMode::Relevance,
            ..SearchQuery::default()
        };
        match execute_search_simple(&cx, &p, &query).await {
            Outcome::Ok(resp) => resp,
            other => panic!("search failed: {other:?}"),
        }
    });

    // All returned results should be urgent importance.
    for r in &results.results {
        if let Some(ref imp) = r.importance {
            assert_eq!(
                imp, "urgent",
                "facet filter should only return urgent messages, got: {imp}"
            );
        }
    }
}

/// Verify that recency ranking mode works.
#[test]
fn search_quality_recency_ranking() {
    let corpus = seed_corpus();
    let p = corpus.pool.clone();
    let pid = corpus.project_id;

    let results = block_on(move |cx| async move {
        let query = SearchQuery {
            text: String::new(), // empty text = all messages
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            limit: Some(10),
            ranking: RankingMode::Recency,
            ..SearchQuery::default()
        };
        match execute_search_simple(&cx, &p, &query).await {
            Outcome::Ok(resp) => resp,
            other => panic!("search failed: {other:?}"),
        }
    });

    // Recency mode: results should be ordered by created_ts descending.
    let timestamps: Vec<Option<i64>> = results.results.iter().map(|r| r.created_ts).collect();
    for window in timestamps.windows(2) {
        if let (Some(a), Some(b)) = (window[0], window[1]) {
            assert!(a >= b, "recency ranking should be descending: {a} < {b}");
        }
    }
}

/// Verify explain metadata is populated for FTS queries.
#[test]
fn search_quality_explain_metadata() {
    let corpus = seed_corpus();
    let p = corpus.pool.clone();
    let pid = corpus.project_id;

    let results = block_on(move |cx| async move {
        let query = SearchQuery {
            text: "WAL checkpoint".to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            explain: true,
            limit: Some(5),
            ranking: RankingMode::Relevance,
            ..SearchQuery::default()
        };
        match execute_search_simple(&cx, &p, &query).await {
            Outcome::Ok(resp) => resp,
            other => panic!("search failed: {other:?}"),
        }
    });

    let explain = results
        .explain
        .expect("explain should be populated when requested");
    assert_eq!(
        explain.method, "fts5",
        "should use FTS5 method for normal query"
    );
    assert!(
        explain.normalized_query.is_some(),
        "should include normalized query"
    );
}

/// Stress test: seed corpus twice (60+ messages) and verify search still works.
#[test]
fn search_quality_larger_corpus_stability() {
    let (pool, _dir) = make_pool();
    let corpus = corpus_v1();

    // Create project + agents.
    let (project_id, agents) = {
        let p = pool.clone();
        let sender_names: Vec<&str> = {
            let mut names: Vec<&str> = corpus.iter().map(|m| m.sender_name).collect();
            names.sort_unstable();
            names.dedup();
            names
        };
        block_on(move |cx| async move {
            let proj = match queries::ensure_project(&cx, &p, "/bench/large-corpus").await {
                Outcome::Ok(r) => r,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let pid = proj.id.unwrap();
            let mut agent_map = HashMap::new();
            for name in &sender_names {
                let a =
                    match queries::register_agent(&cx, &p, pid, name, "bench", "test", None, None)
                        .await
                    {
                        Outcome::Ok(r) => r,
                        other => panic!("register_agent failed: {other:?}"),
                    };
                agent_map.insert(name.to_string(), a.id.unwrap());
            }
            (pid, agent_map)
        })
    };

    // Insert corpus twice with slight subject variations.
    for round in 0..2 {
        for msg in &corpus {
            let p = pool.clone();
            let sid = agents[msg.sender_name];
            let subject = if round == 0 {
                msg.subject.to_string()
            } else {
                format!("(round 2) {}", msg.subject)
            };
            let body = msg.body.to_string();
            let thread_id = msg.thread_id.map(ToString::to_string);
            let importance = msg.importance.to_string();
            let ack = msg.ack_required;

            block_on(move |cx| async move {
                match queries::create_message(
                    &cx,
                    &p,
                    project_id,
                    sid,
                    &subject,
                    &body,
                    thread_id.as_deref(),
                    &importance,
                    ack,
                    "",
                )
                .await
                {
                    Outcome::Ok(_) => {}
                    other => panic!("create_message failed: {other:?}"),
                }
            });
        }
    }

    // Run a sample query and verify it returns results from both rounds.
    let results = block_on(move |cx| async move {
        match queries::search_messages(&cx, &pool, project_id, "WAL checkpoint", 20).await {
            Outcome::Ok(rows) => rows,
            other => panic!("search failed: {other:?}"),
        }
    });

    // Should find results from both original and round 2.
    assert!(
        results.len() >= 3,
        "should find WAL messages from both rounds, got {}",
        results.len()
    );
}

// ────────────────────────────────────────────────────────────────────
// Metric unit tests (self-tests for the metric functions)
// ────────────────────────────────────────────────────────────────────

#[test]
fn metric_ndcg_perfect_ranking() {
    let ranked = vec![3.0, 2.0, 1.0, 0.0];
    let ideal = vec![3.0, 2.0, 1.0, 0.0];
    let score = ndcg_at_k(&ranked, &ideal, 4);
    assert!(
        (score - 1.0).abs() < 1e-9,
        "perfect ranking should have NDCG=1.0, got {score}"
    );
}

#[test]
fn metric_ndcg_reversed_ranking() {
    let ranked = vec![0.0, 1.0, 2.0, 3.0];
    let ideal = vec![3.0, 2.0, 1.0, 0.0];
    let score = ndcg_at_k(&ranked, &ideal, 4);
    assert!(
        score < 0.8,
        "reversed ranking should have low NDCG, got {score}"
    );
    assert!(score > 0.0, "reversed ranking should have positive NDCG");
}

#[test]
fn metric_ndcg_empty() {
    let score = ndcg_at_k(&[], &[], 5);
    assert!((score - 1.0).abs() < 1e-9, "empty should be vacuously 1.0");
}

#[test]
fn metric_mrr_first_position() {
    let ranked = vec![3.0, 0.0, 0.0];
    assert!((mrr(&ranked) - 1.0).abs() < 1e-9);
}

#[test]
fn metric_mrr_third_position() {
    let ranked = vec![0.0, 0.0, 2.0, 1.0];
    assert!((mrr(&ranked) - 1.0 / 3.0).abs() < 1e-9);
}

#[test]
fn metric_mrr_no_relevant() {
    let ranked = vec![0.0, 0.0, 0.0];
    assert!((mrr(&ranked)).abs() < 1e-9);
}

#[test]
fn metric_precision_at_k_all_relevant() {
    let ranked = vec![2.0, 3.0, 1.0];
    assert!((precision_at_k(&ranked, 3) - 1.0).abs() < 1e-9);
}

#[test]
fn metric_precision_at_k_none_relevant() {
    let ranked = vec![0.0, 0.0, 0.0];
    assert!((precision_at_k(&ranked, 3)).abs() < 1e-9);
}

#[test]
fn metric_precision_at_k_mixed() {
    let ranked = vec![1.0, 0.0, 2.0, 0.0];
    assert!((precision_at_k(&ranked, 4) - 0.5).abs() < 1e-9);
}

#[test]
fn metric_recall_at_k_full() {
    let ranked = vec![2.0, 1.0, 3.0];
    assert!((recall_at_k(&ranked, 3, 3) - 1.0).abs() < 1e-9);
}

#[test]
fn metric_recall_at_k_partial() {
    let ranked = vec![2.0, 0.0, 0.0];
    assert!((recall_at_k(&ranked, 3, 3) - 1.0 / 3.0).abs() < 1e-9);
}
