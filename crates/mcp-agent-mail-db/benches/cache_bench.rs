//! Criterion benchmarks for cache eviction strategies.
//!
//! Compares S3-FIFO (three-queue frequency-based) against `IndexMap` LRU
//! (insertion-order with move-to-back) on insert, lookup, and mixed workloads.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use indexmap::IndexMap;
use mcp_agent_mail_db::s3fifo::S3FifoCache;

const CAP: usize = 1_000;
const OPS: usize = 100_000;

/// Simple deterministic Zipf-like sampler.
///
/// Given a step index, produces a key biased toward lower values
/// (simulating popular items accessed more frequently).
const fn zipf_key(step: usize, n_unique: usize) -> usize {
    // Hash-like scramble to avoid trivial patterns
    let h = step.wrapping_mul(2_654_435_761) >> 16;
    // Square to bias toward low ranks (Zipf-like)
    let rank = h % n_unique;
    (rank * rank) % n_unique
}

// ---------------------------------------------------------------------------
// Insert benchmarks (100K inserts into capacity-1000 cache)
// ---------------------------------------------------------------------------

fn bench_s3fifo_insert_100k(c: &mut Criterion) {
    c.bench_function("s3fifo_insert_100k", |b| {
        b.iter(|| {
            let mut cache = S3FifoCache::new(CAP);
            for i in 0..OPS {
                cache.insert(black_box(i), i);
            }
            black_box(cache.len());
        });
    });
}

fn bench_lru_insert_100k(c: &mut Criterion) {
    c.bench_function("lru_insert_100k", |b| {
        b.iter(|| {
            let mut map: IndexMap<usize, usize> = IndexMap::with_capacity(CAP);
            for i in 0..OPS {
                map.insert(i, i);
                // LRU eviction: remove oldest while over capacity
                while map.len() > CAP {
                    map.shift_remove_index(0);
                }
            }
            black_box(map.len());
        });
    });
}

// ---------------------------------------------------------------------------
// Get benchmarks (Zipf distribution, 100K lookups)
// ---------------------------------------------------------------------------

fn bench_s3fifo_get_zipf_100k(c: &mut Criterion) {
    c.bench_function("s3fifo_get_zipf_100k", |b| {
        let n_unique = 5_000;
        b.iter(|| {
            let mut cache = S3FifoCache::new(CAP);
            // Pre-fill
            for i in 0..n_unique {
                cache.insert(i, i);
            }
            let mut hits = 0u64;
            for step in 0..OPS {
                let key = zipf_key(step, n_unique);
                if cache.get(&key).is_some() {
                    hits += 1;
                } else {
                    cache.insert(key, key);
                }
            }
            black_box(hits);
        });
    });
}

fn bench_lru_get_zipf_100k(c: &mut Criterion) {
    c.bench_function("lru_get_zipf_100k", |b| {
        let n_unique = 5_000;
        b.iter(|| {
            let mut map: IndexMap<usize, usize> = IndexMap::with_capacity(CAP);
            // Pre-fill
            for i in 0..n_unique {
                map.insert(i, i);
                while map.len() > CAP {
                    map.shift_remove_index(0);
                }
            }
            let mut hits = 0u64;
            for step in 0..OPS {
                let key = zipf_key(step, n_unique);
                if let Some(idx) = map.get_index_of(&key) {
                    map.move_index(idx, map.len() - 1);
                    hits += 1;
                } else {
                    map.insert(key, key);
                    while map.len() > CAP {
                        map.shift_remove_index(0);
                    }
                }
            }
            black_box(hits);
        });
    });
}

// ---------------------------------------------------------------------------
// Mixed benchmarks (70% get / 30% insert, Zipf)
// ---------------------------------------------------------------------------

fn bench_s3fifo_mixed_100k(c: &mut Criterion) {
    c.bench_function("s3fifo_mixed_100k", |b| {
        let n_unique = 5_000;
        b.iter(|| {
            let mut cache = S3FifoCache::new(CAP);
            for i in 0..CAP {
                cache.insert(i, i);
            }
            let mut hits = 0u64;
            for step in 0..OPS {
                let key = zipf_key(step, n_unique);
                if step % 10 < 7 {
                    // 70% reads
                    if cache.get(&key).is_some() {
                        hits += 1;
                    }
                } else {
                    // 30% writes
                    cache.insert(key, key);
                }
            }
            black_box(hits);
        });
    });
}

fn bench_lru_mixed_100k(c: &mut Criterion) {
    c.bench_function("lru_mixed_100k", |b| {
        let n_unique = 5_000;
        b.iter(|| {
            let mut map: IndexMap<usize, usize> = IndexMap::with_capacity(CAP);
            for i in 0..CAP {
                map.insert(i, i);
            }
            let mut hits = 0u64;
            for step in 0..OPS {
                let key = zipf_key(step, n_unique);
                if step % 10 < 7 {
                    if let Some(idx) = map.get_index_of(&key) {
                        map.move_index(idx, map.len() - 1);
                        hits += 1;
                    }
                } else {
                    map.insert(key, key);
                    while map.len() > CAP {
                        map.shift_remove_index(0);
                    }
                }
            }
            black_box(hits);
        });
    });
}

criterion_group!(
    benches,
    bench_s3fifo_insert_100k,
    bench_lru_insert_100k,
    bench_s3fifo_get_zipf_100k,
    bench_lru_get_zipf_100k,
    bench_s3fifo_mixed_100k,
    bench_lru_mixed_100k,
);
criterion_main!(benches);
