//! Benchmark suite for two-tier search performance (br-2tnl.5.10.10)
//!
//! Covers:
//! - Micro-benchmarks: dot product, normalization, f16 conversion, score blending
//! - Index benchmarks: fast search at various corpus sizes (100, 1K, 10K)
//! - Index construction: build from entries
//! - Memory validation: per-document overhead

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use half::f16;
use mcp_agent_mail_core::DocKind;
use mcp_agent_mail_db::search_two_tier::{
    TwoTierConfig, TwoTierEntry, TwoTierIndex, blend_scores, dot_product_f16_simd, normalize_scores,
};
use std::hint::black_box;

// ── Helpers ────────────────────────────────────────────────────────

#[allow(clippy::cast_precision_loss)]
fn make_f16_embedding(dim: usize, seed: usize) -> Vec<f16> {
    (0..dim)
        .map(|j| f16::from_f32(((seed + j) as f32 * 0.01).sin()))
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn make_f32_query(dim: usize, seed: usize) -> Vec<f32> {
    let mut q: Vec<f32> = (0..dim).map(|j| ((seed + j) as f32 * 0.01).cos()).collect();
    // L2 normalize
    let norm: f32 = q.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut q {
            *x /= norm;
        }
    }
    q
}

#[allow(clippy::cast_precision_loss)]
fn make_test_entries(count: usize, config: &TwoTierConfig) -> Vec<TwoTierEntry> {
    (0..count)
        .map(|i| TwoTierEntry {
            doc_id: i as u64,
            doc_kind: DocKind::Message,
            project_id: Some(1),
            fast_embedding: make_f16_embedding(config.fast_dimension, i),
            quality_embedding: make_f16_embedding(config.quality_dimension, i),
            has_quality: true,
        })
        .collect()
}

fn build_index(size: usize) -> TwoTierIndex {
    let config = TwoTierConfig::default();
    let entries = make_test_entries(size, &config);
    TwoTierIndex::build("bench-fast", "bench-quality", &config, entries)
        .expect("index build should succeed")
}

// ── Micro-benchmarks ───────────────────────────────────────────────

fn bench_dot_product(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_f16_simd");

    for dim in [64, 128, 256, 384] {
        let embedding = make_f16_embedding(dim, 42);
        let query = make_f32_query(dim, 99);

        group.bench_with_input(BenchmarkId::new("dim", dim), &dim, |b, _| {
            b.iter(|| dot_product_f16_simd(black_box(&embedding), black_box(&query)));
        });
    }
    group.finish();
}

#[allow(clippy::cast_precision_loss)]
fn bench_normalize_scores(c: &mut Criterion) {
    let mut group = c.benchmark_group("normalize_scores");

    for len in [10, 100, 1000] {
        let scores: Vec<f32> = (0..len).map(|i| (i as f32) * 0.1).collect();

        group.bench_with_input(BenchmarkId::new("len", len), &len, |b, _| {
            b.iter(|| normalize_scores(black_box(&scores)));
        });
    }
    group.finish();
}

#[allow(clippy::cast_precision_loss)]
fn bench_blend_scores(c: &mut Criterion) {
    let mut group = c.benchmark_group("blend_scores");

    for len in [10, 100, 1000] {
        let fast: Vec<f32> = (0..len).map(|i| (i as f32) * 0.1).collect();
        let quality: Vec<f32> = (0..len).map(|i| (i as f32) * 0.2).collect();
        let weight = 0.7_f32;

        group.bench_with_input(BenchmarkId::new("len", len), &len, |b, _| {
            b.iter(|| blend_scores(black_box(&fast), black_box(&quality), black_box(weight)));
        });
    }
    group.finish();
}

fn bench_f16_to_f32_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("f16_to_f32");

    for dim in [256, 384] {
        let embedding = make_f16_embedding(dim, 42);

        group.bench_with_input(BenchmarkId::new("dim", dim), &dim, |b, _| {
            b.iter(|| {
                let _: Vec<f32> = black_box(&embedding)
                    .iter()
                    .map(|&x| f32::from(x))
                    .collect();
            });
        });
    }
    group.finish();
}

// ── Index construction ─────────────────────────────────────────────

fn bench_index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_build");
    group.sample_size(10);

    for size in [100, 1000] {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(size, &config);

        group.bench_with_input(BenchmarkId::new("docs", size), &size, |b, _| {
            b.iter(|| {
                TwoTierIndex::build(
                    "bench-fast",
                    "bench-quality",
                    black_box(&config),
                    black_box(entries.clone()),
                )
                .unwrap()
            });
        });
    }
    group.finish();
}

fn bench_index_add_entry(c: &mut Criterion) {
    let config = TwoTierConfig::default();

    c.bench_function("add_entry_single", |b| {
        let entry = TwoTierEntry {
            doc_id: 0,
            doc_kind: DocKind::Message,
            project_id: Some(1),
            fast_embedding: make_f16_embedding(config.fast_dimension, 0),
            quality_embedding: make_f16_embedding(config.quality_dimension, 0),
            has_quality: true,
        };
        b.iter(|| {
            let mut index = TwoTierIndex::new(black_box(&config));
            index.add_entry(black_box(entry.clone())).unwrap();
        });
    });
}

// ── Search benchmarks ──────────────────────────────────────────────

fn bench_search_fast(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_fast");

    for size in [100, 1000, 10_000] {
        let index = build_index(size);
        let config = TwoTierConfig::default();
        let query = make_f32_query(config.fast_dimension, 42);

        group.bench_with_input(BenchmarkId::new("docs", size), &size, |b, _| {
            b.iter(|| index.search_fast(black_box(&query), black_box(10)));
        });
    }
    group.finish();
}

fn bench_search_quality(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_quality");

    for size in [100, 1000, 10_000] {
        let index = build_index(size);
        let config = TwoTierConfig::default();
        let query = make_f32_query(config.quality_dimension, 42);

        group.bench_with_input(BenchmarkId::new("docs", size), &size, |b, _| {
            b.iter(|| index.search_quality(black_box(&query), black_box(10)));
        });
    }
    group.finish();
}

fn bench_search_fast_varying_k(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_fast_k");
    let index = build_index(1000);
    let config = TwoTierConfig::default();
    let query = make_f32_query(config.fast_dimension, 42);

    for k in [1, 10, 50, 100] {
        group.bench_with_input(BenchmarkId::new("k", k), &k, |b, &k| {
            b.iter(|| index.search_fast(black_box(&query), black_box(k)));
        });
    }
    group.finish();
}

fn bench_quality_scores_for_indices(c: &mut Criterion) {
    let mut group = c.benchmark_group("quality_scores_for_indices");
    let config = TwoTierConfig::default();

    for size in [100, 1000] {
        let index = build_index(size);
        let query = make_f32_query(config.quality_dimension, 42);
        let indices: Vec<usize> = (0..10.min(size)).collect();

        group.bench_with_input(BenchmarkId::new("index_size", size), &size, |b, _| {
            b.iter(|| index.quality_scores_for_indices(black_box(&query), black_box(&indices)));
        });
    }
    group.finish();
}

// ── Memory benchmarks ──────────────────────────────────────────────

fn bench_index_metrics(c: &mut Criterion) {
    let index = build_index(1000);

    c.bench_function("index_metrics_1k", |b| {
        b.iter(|| black_box(index.metrics()));
    });
}

// ── Criterion groups ───────────────────────────────────────────────

criterion_group!(
    micro,
    bench_dot_product,
    bench_normalize_scores,
    bench_blend_scores,
    bench_f16_to_f32_conversion,
);

criterion_group!(construction, bench_index_build, bench_index_add_entry,);

criterion_group!(
    search,
    bench_search_fast,
    bench_search_quality,
    bench_search_fast_varying_k,
    bench_quality_scores_for_indices,
    bench_index_metrics,
);

criterion_main!(micro, construction, search);
