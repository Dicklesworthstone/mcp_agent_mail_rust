//! Criterion benchmarks for the sharded coalescer (br-36tr5 E.2).
//!
//! Measures throughput under varying thread counts to demonstrate that
//! the 16-shard design reduces contention at scale.

use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use mcp_agent_mail_db::coalesce::CoalesceMap;

const OPS_PER_THREAD: usize = 1000;

/// Run `n_threads` threads each doing `OPS_PER_THREAD` operations on distinct keys.
/// Returns total wall-clock duration.
fn bench_distinct_keys(n_threads: usize, map: &Arc<CoalesceMap<String, usize>>) {
    let barrier = Arc::new(Barrier::new(n_threads));
    let global_idx = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let map = Arc::clone(map);
            let barrier = Arc::clone(&barrier);
            let global_idx = Arc::clone(&global_idx);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..OPS_PER_THREAD {
                    let idx = global_idx.fetch_add(1, Ordering::Relaxed);
                    let key = format!("key-{idx}");
                    let r = map.execute_or_join(key, || Ok::<_, String>(idx)).unwrap();
                    black_box(r.into_inner());
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

/// Run `n_threads` threads each doing `OPS_PER_THREAD` operations on the SAME key.
/// This tests the coalescing hot path.
fn bench_same_key(n_threads: usize, map: &Arc<CoalesceMap<String, usize>>) {
    let barrier = Arc::new(Barrier::new(n_threads));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let map = Arc::clone(map);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..OPS_PER_THREAD {
                    let r = map
                        .execute_or_join("hot-key".to_string(), || Ok::<_, String>(i))
                        .unwrap();
                    black_box(r.into_inner());
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

fn bench_sharded_distinct_10_threads(c: &mut Criterion) {
    c.bench_function("sharded_distinct_10_threads", |b| {
        b.iter(|| {
            let map = Arc::new(CoalesceMap::<String, usize>::new(
                10_000,
                Duration::from_millis(100),
            ));
            bench_distinct_keys(10, &map);
            assert_eq!(map.inflight_count(), 0);
        });
    });
}

fn bench_sharded_distinct_50_threads(c: &mut Criterion) {
    c.bench_function("sharded_distinct_50_threads", |b| {
        b.iter(|| {
            let map = Arc::new(CoalesceMap::<String, usize>::new(
                50_000,
                Duration::from_millis(100),
            ));
            bench_distinct_keys(50, &map);
            assert_eq!(map.inflight_count(), 0);
        });
    });
}

fn bench_sharded_same_key_10_threads(c: &mut Criterion) {
    c.bench_function("sharded_same_key_10_threads", |b| {
        b.iter(|| {
            let map = Arc::new(CoalesceMap::<String, usize>::new(
                10_000,
                Duration::from_millis(100),
            ));
            bench_same_key(10, &map);
            assert_eq!(map.inflight_count(), 0);
        });
    });
}

fn bench_sharded_same_key_50_threads(c: &mut Criterion) {
    c.bench_function("sharded_same_key_50_threads", |b| {
        b.iter(|| {
            let map = Arc::new(CoalesceMap::<String, usize>::new(
                50_000,
                Duration::from_millis(100),
            ));
            bench_same_key(50, &map);
            assert_eq!(map.inflight_count(), 0);
        });
    });
}

fn bench_sequential_baseline(c: &mut Criterion) {
    c.bench_function("sequential_baseline_1000_ops", |b| {
        b.iter(|| {
            let map = CoalesceMap::<String, usize>::new(10_000, Duration::from_millis(100));
            for i in 0..1000 {
                let key = format!("seq-key-{i}");
                let r = map.execute_or_join(key, || Ok::<_, String>(i)).unwrap();
                black_box(r.into_inner());
            }
            assert_eq!(map.inflight_count(), 0);
        });
    });
}

criterion_group!(
    benches,
    bench_sequential_baseline,
    bench_sharded_distinct_10_threads,
    bench_sharded_distinct_50_threads,
    bench_sharded_same_key_10_threads,
    bench_sharded_same_key_50_threads,
);

criterion_main!(benches);
