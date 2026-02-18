//! Search V3 performance benchmarks (br-2tnl.7.5)
//!
//! Covers:
//! - Tantivy lexical search p50/p95/p99 at 3 corpus sizes (1K, 5K, 15K)
//! - Index build throughput at 3 corpus sizes
//! - Incremental document add throughput
//! - Disk overhead per document
//! - Budget enforcement via `MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1`
//!
//! Run:
//! ```bash
//! cargo bench -p mcp-agent-mail-db --bench search_v3_bench
//! ```
//!
//! Enforce budgets (CI):
//! ```bash
//! MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1 \
//!   cargo bench -p mcp-agent-mail-db --bench search_v3_bench
//! ```

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mcp_agent_mail_db::search_planner::SearchQuery;
use mcp_agent_mail_db::search_v3::TantivyBridge;
use serde::Serialize;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tantivy::doc;
use tempfile::TempDir;

// ── Constants ─────────────────────────────────────────────────────────

const VOCAB: [&str; 10] = [
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
];

const NEEDLE_INTERVAL: usize = 97;

// ── Scenarios ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexicalScenario {
    Small,
    Medium,
    Large,
}

impl LexicalScenario {
    const fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    const fn message_count(self) -> usize {
        match self {
            Self::Small => 1_000,
            Self::Medium => 5_000,
            Self::Large => 15_000,
        }
    }

    const fn search_ops(self) -> usize {
        match self {
            Self::Small => 200,
            Self::Medium => 100,
            Self::Large => 50,
        }
    }

    /// Budget thresholds `(p95_us, p99_us)`.
    ///
    /// Tantivy lexical should be faster than FTS5 at all corpus sizes.
    /// These are conservative initial budgets (will tighten after baseline run).
    const fn budget_us(self) -> (u64, u64) {
        match self {
            Self::Small => (1_500, 3_000),
            Self::Medium => (5_000, 10_000),
            Self::Large => (15_000, 25_000),
        }
    }
}

// ── Result types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct ScenarioResult {
    scenario: String,
    message_count: usize,
    ops: usize,
    query: String,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    budget_p95_us: u64,
    budget_p99_us: u64,
    p95_within_budget: bool,
    p99_within_budget: bool,
    p95_delta_us: i64,
    p99_delta_us: i64,
    throughput_qps: f64,
}

#[derive(Debug, Clone, Serialize)]
struct IndexBuildResult {
    doc_count: usize,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    throughput_docs_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
struct DiskOverhead {
    doc_count: usize,
    index_bytes: u64,
    bytes_per_doc: f64,
}

#[derive(Debug, Clone, Serialize)]
struct SearchV3BenchRun {
    run_id: String,
    arch: String,
    os: String,
    budget_regressions: usize,
    lexical_search: Vec<ScenarioResult>,
    index_build: Vec<IndexBuildResult>,
    disk_overhead: Vec<DiskOverhead>,
}

// ── Helpers ───────────────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `crates/mcp-agent-mail-db`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}_{}", now.as_secs(), std::process::id())
}

fn artifact_dir(run_id: &str) -> PathBuf {
    repo_root()
        .join("tests")
        .join("artifacts")
        .join("bench")
        .join("search_v3")
        .join(run_id)
}

fn percentile_us(mut samples: Vec<u64>, pct: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let n = samples.len();
    let max_idx = n.saturating_sub(1);
    let pct = pct.clamp(0.0, 1.0);
    #[allow(clippy::cast_sign_loss)]
    let idx = ((pct * max_idx as f64).round() as usize).min(max_idx);
    samples[idx]
}

fn dir_size_bytes(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    let mut stack = vec![dir.to_path_buf()];
    let mut total = 0_u64;
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Seed a Tantivy index via `TantivyBridge` with `count` documents.
///
/// Every `NEEDLE_INTERVAL`-th document has "needle" in its subject (for selective queries).
fn seed_tantivy_bridge(bridge: &TantivyBridge, count: usize) {
    let handles = bridge.handles();
    let mut writer = bridge
        .index()
        .writer(50_000_000)
        .expect("tantivy writer");

    for i in 0..count {
        let v = VOCAB[i % VOCAB.len()];
        let mut subject = format!("bench {i} {v}");
        if i % NEEDLE_INTERVAL == 0 {
            subject.push_str(" needle");
        }
        let body = format!("body {i} {v} {}", VOCAB[(i * 7 + 3) % VOCAB.len()]);
        let thread_id = format!("bench-{}", i % 100);

        writer
            .add_document(doc!(
                handles.id => i as u64,
                handles.doc_kind => "message",
                handles.subject => subject,
                handles.body => body,
                handles.sender => "BlueLake",
                handles.project_slug => "backend",
                handles.project_id => 1u64,
                handles.thread_id => thread_id,
                handles.importance => "normal",
                handles.created_ts => 1_700_000_000_000_000i64 + (i as i64)
            ))
            .expect("add document");
    }
    writer.commit().expect("commit");
}

fn create_seeded_bridge(dir: &Path, count: usize) -> TantivyBridge {
    let bridge = TantivyBridge::open(dir).expect("open bridge");
    seed_tantivy_bridge(&bridge, count);
    bridge
}

// ── Deterministic harness (p50/p95/p99 + artifacts + budget check) ────

#[allow(clippy::too_many_lines)]
fn run_search_v3_harness_once() {
    static DID_RUN: Once = Once::new();
    DID_RUN.call_once(|| {
        let run_id = run_id();
        let out_dir = artifact_dir(&run_id);
        let _ = std::fs::create_dir_all(&out_dir);

        let scenarios = [
            LexicalScenario::Small,
            LexicalScenario::Medium,
            LexicalScenario::Large,
        ];

        let mut search_results = Vec::new();
        let mut build_results = Vec::new();
        let mut disk_results = Vec::new();
        let mut regressions = 0_usize;

        for scenario in &scenarios {
            let tmp = TempDir::new().expect("tempdir");
            let index_dir = tmp.path().join("tantivy_index");
            let count = scenario.message_count();

            // ── Index build measurement ──────────────────────────────────
            let build_ops = 3;
            let mut build_samples = Vec::with_capacity(build_ops);
            for _ in 0..build_ops {
                let build_tmp = TempDir::new().expect("build tempdir");
                let build_dir = build_tmp.path().join("idx");
                let t0 = Instant::now();
                create_seeded_bridge(&build_dir, count);
                build_samples.push(t0.elapsed().as_micros() as u64);
            }
            let build_p50 = percentile_us(build_samples.clone(), 0.50);
            let build_p95 = percentile_us(build_samples.clone(), 0.95);
            let build_total_us: u64 = build_samples.iter().copied().sum();
            let build_throughput = if build_total_us > 0 {
                (count as f64 * build_ops as f64) / (build_total_us as f64 / 1_000_000.0)
            } else {
                0.0
            };
            build_results.push(IndexBuildResult {
                doc_count: count,
                samples_us: build_samples,
                p50_us: build_p50,
                p95_us: build_p95,
                throughput_docs_per_sec: (build_throughput * 100.0).round() / 100.0,
            });

            // ── Disk overhead measurement ────────────────────────────────
            let disk_tmp = TempDir::new().expect("disk tempdir");
            let disk_dir = disk_tmp.path().join("idx");
            create_seeded_bridge(&disk_dir, count);
            let index_bytes = dir_size_bytes(&disk_dir);
            let bytes_per_doc = if count > 0 {
                index_bytes as f64 / count as f64
            } else {
                0.0
            };
            disk_results.push(DiskOverhead {
                doc_count: count,
                index_bytes,
                bytes_per_doc: (bytes_per_doc * 100.0).round() / 100.0,
            });

            // ── Search latency measurement ───────────────────────────────
            let bridge = create_seeded_bridge(&index_dir, count);
            let query = SearchQuery::messages("needle", 1);
            let ops = scenario.search_ops();

            // Warm-up: 3 queries before sampling
            for _ in 0..3 {
                let _ = black_box(bridge.search(&query));
            }

            let mut samples_us: Vec<u64> = Vec::with_capacity(ops);
            for _ in 0..ops {
                let t0 = Instant::now();
                let results = bridge.search(&query);
                let elapsed = t0.elapsed().as_micros() as u64;
                black_box(&results);
                samples_us.push(elapsed);
            }

            let total_us: u64 = samples_us.iter().copied().sum();
            let throughput = if total_us > 0 {
                ops as f64 * 1_000_000.0 / total_us as f64
            } else {
                0.0
            };

            let p50 = percentile_us(samples_us.clone(), 0.50);
            let p95 = percentile_us(samples_us.clone(), 0.95);
            let p99 = percentile_us(samples_us.clone(), 0.99);

            let (budget_p95, budget_p99) = scenario.budget_us();
            let p95_ok = p95 <= budget_p95;
            let p99_ok = p99 <= budget_p99;
            if !p95_ok || !p99_ok {
                regressions += 1;
            }

            let result = ScenarioResult {
                scenario: scenario.name().to_string(),
                message_count: count,
                ops,
                query: "needle".to_string(),
                samples_us: samples_us.clone(),
                p50_us: p50,
                p95_us: p95,
                p99_us: p99,
                budget_p95_us: budget_p95,
                budget_p99_us: budget_p99,
                p95_within_budget: p95_ok,
                p99_within_budget: p99_ok,
                p95_delta_us: p95 as i64 - budget_p95 as i64,
                p99_delta_us: p99 as i64 - budget_p99 as i64,
                throughput_qps: (throughput * 100.0).round() / 100.0,
            };

            let _ = std::fs::write(
                out_dir.join(format!("lexical_{}.json", scenario.name())),
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            );

            search_results.push(result);
        }

        let run = SearchV3BenchRun {
            run_id: run_id.clone(),
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            budget_regressions: regressions,
            lexical_search: search_results,
            index_build: build_results,
            disk_overhead: disk_results,
        };

        let _ = std::fs::write(
            out_dir.join("summary.json"),
            serde_json::to_string_pretty(&run).unwrap_or_default(),
        );

        if std::env::var("MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS")
            .ok()
            .as_deref()
            == Some("1")
            && regressions > 0
        {
            panic!(
                "Search V3 bench budgets exceeded: {regressions} regressions (run_id={run_id})"
            );
        }
    });
}

// ── Criterion: Tantivy lexical search ─────────────────────────────────

fn bench_tantivy_lexical_search(c: &mut Criterion) {
    run_search_v3_harness_once();

    let scenarios = [
        LexicalScenario::Small,
        LexicalScenario::Medium,
        LexicalScenario::Large,
    ];

    let mut group = c.benchmark_group("tantivy_lexical_search");
    group.sample_size(10);

    for scenario in scenarios {
        let tmp = TempDir::new().expect("tempdir");
        let index_dir = tmp.path().join("idx");
        let count = scenario.message_count();
        let bridge = create_seeded_bridge(&index_dir, count);
        let query = SearchQuery::messages("needle", 1);

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new(scenario.name(), count),
            &count,
            |b, _| {
                b.iter(|| {
                    let results = bridge.search(black_box(&query));
                    black_box(&results);
                });
            },
        );

        // Keep bridge alive for the bench iteration.
        drop(bridge);
        drop(tmp);
    }

    group.finish();
}

// ── Criterion: Tantivy search with varying query selectivity ──────────

fn bench_tantivy_query_selectivity(c: &mut Criterion) {
    let tmp = TempDir::new().expect("tempdir");
    let index_dir = tmp.path().join("idx");
    let bridge = create_seeded_bridge(&index_dir, 5_000);

    let queries: &[(&str, &str)] = &[
        ("high_selectivity", "needle"),        // ~1% hit rate (every 97th doc)
        ("medium_selectivity", "alpha beta"),   // ~20% hit rate
        ("low_selectivity", "bench"),           // ~100% hit rate
        ("phrase_query", "\"bench 42 delta\""), // exact phrase
    ];

    let mut group = c.benchmark_group("tantivy_query_selectivity");
    group.sample_size(20);

    for (label, query_text) in queries {
        let query = SearchQuery::messages(*query_text, 1);

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new(*label, query_text), label, |b, _| {
            b.iter(|| {
                let results = bridge.search(black_box(&query));
                black_box(&results);
            });
        });
    }

    group.finish();
}

// ── Criterion: Index build throughput ─────────────────────────────────

fn bench_tantivy_index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("tantivy_index_build");
    group.sample_size(10);

    for count in [1_000, 5_000, 15_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("docs", count), &count, |b, &count| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let tmp = TempDir::new().expect("tempdir");
                    let dir = tmp.path().join("idx");
                    let t0 = Instant::now();
                    create_seeded_bridge(&dir, count);
                    total += t0.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

// ── Criterion: Incremental document add ───────────────────────────────

fn bench_tantivy_incremental_add(c: &mut Criterion) {
    let tmp = TempDir::new().expect("tempdir");
    let index_dir = tmp.path().join("idx");
    let bridge = create_seeded_bridge(&index_dir, 5_000);
    let handles = bridge.handles();

    let mut group = c.benchmark_group("tantivy_incremental_add");
    group.sample_size(20);

    // Batch sizes: 1, 10, 100
    for batch_size in [1_u64, 10, 100] {
        let mut doc_counter: u64 = 100_000;

        group.throughput(Throughput::Elements(batch_size));
        group.bench_with_input(
            BenchmarkId::new("batch", batch_size),
            &batch_size,
            |b, &batch_size| {
                b.iter(|| {
                    let mut writer = bridge.index().writer(15_000_000).expect("writer");
                    for _ in 0..batch_size {
                        let id = doc_counter;
                        doc_counter += 1;
                        writer
                            .add_document(doc!(
                                handles.id => id,
                                handles.doc_kind => "message",
                                handles.subject => "incremental add",
                                handles.body => "body of incremental document",
                                handles.sender => "BlueLake",
                                handles.project_slug => "backend",
                                handles.project_id => 1u64,
                                handles.thread_id => "inc-thread",
                                handles.importance => "normal",
                                handles.created_ts => 1_700_000_000_000_000i64 + (id as i64)
                            ))
                            .expect("add doc");
                    }
                    writer.commit().expect("commit");
                });
            },
        );
    }

    group.finish();
}

// ── Criterion groups ──────────────────────────────────────────────────

criterion_group!(
    lexical_search,
    bench_tantivy_lexical_search,
    bench_tantivy_query_selectivity,
);

criterion_group!(
    index_throughput,
    bench_tantivy_index_build,
    bench_tantivy_incremental_add,
);

criterion_main!(lexical_search, index_throughput);
