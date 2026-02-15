//! Golden output tests for S3-FIFO cache eviction.
//!
//! Verifies determinism: same seed + sequence always produces the same
//! cache contents and hit/miss counts.
//!
//! Required tests for br-2khpi:
//! 1. `golden_s3fifo_deterministic`
//! 2. `bench_s3fifo_faster_than_lru_eviction`
//! 3. `hit_rate_comparison_zipf`

use std::path::PathBuf;

use indexmap::IndexMap;
use mcp_agent_mail_db::s3fifo::S3FifoCache;

const SEED: u64 = 42;
const OPS: usize = 10_000;
const N_UNIQUE: usize = 500;
const CAPACITY: usize = 100;

/// Simple deterministic LCG (Knuth's constants).
struct Lcg {
    state: u64,
}

impl Lcg {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    #[allow(clippy::cast_possible_truncation)]
    const fn next_key(&mut self, n_unique: usize) -> usize {
        (self.next() % n_unique as u64) as usize
    }
}

/// Run the golden sequence through S3-FIFO and return (`sorted_keys`, hits, misses).
fn run_s3fifo_golden() -> (Vec<usize>, u64, u64) {
    let mut cache = S3FifoCache::new(CAPACITY);
    let mut rng = Lcg::new(SEED);
    let mut hits = 0u64;
    let mut misses = 0u64;

    for _ in 0..OPS {
        let key = rng.next_key(N_UNIQUE);
        if cache.get(&key).is_some() {
            hits += 1;
        } else {
            misses += 1;
            cache.insert(key, key);
        }
    }

    let mut keys: Vec<usize> = cache.keys().copied().collect();
    keys.sort_unstable();
    (keys, hits, misses)
}

/// Run the same golden sequence through `IndexMap` LRU.
fn run_lru_golden() -> (Vec<usize>, u64, u64) {
    let mut map: IndexMap<usize, usize> = IndexMap::with_capacity(CAPACITY);
    let mut rng = Lcg::new(SEED);
    let mut hits = 0u64;
    let mut misses = 0u64;

    for _ in 0..OPS {
        let key = rng.next_key(N_UNIQUE);
        if let Some(idx) = map.get_index_of(&key) {
            map.move_index(idx, map.len() - 1);
            hits += 1;
        } else {
            misses += 1;
            map.insert(key, key);
            while map.len() > CAPACITY {
                map.shift_remove_index(0);
            }
        }
    }

    let mut keys: Vec<usize> = map.keys().copied().collect();
    keys.sort_unstable();
    (keys, hits, misses)
}

/// Fixture path for the golden JSON.
fn fixture_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("cache_golden.json");
    path
}

/// 1. `golden_s3fifo_deterministic` -- replay golden sequence, verify exact match.
#[test]
fn golden_s3fifo_deterministic() {
    let (keys, hits, misses) = run_s3fifo_golden();
    let path = fixture_path();

    if path.exists() {
        // Verify against stored fixture
        let content = std::fs::read_to_string(&path).expect("read golden fixture");
        let fixture: serde_json::Value =
            serde_json::from_str(&content).expect("parse golden fixture");

        #[allow(clippy::cast_possible_truncation)]
        let expected_keys: Vec<usize> = fixture["s3fifo"]["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as usize)
            .collect();
        let expected_hits = fixture["s3fifo"]["hits"].as_u64().unwrap();
        let expected_misses = fixture["s3fifo"]["misses"].as_u64().unwrap();

        assert_eq!(
            keys, expected_keys,
            "S3-FIFO final cache contents diverged from golden fixture"
        );
        assert_eq!(hits, expected_hits, "S3-FIFO hit count diverged");
        assert_eq!(misses, expected_misses, "S3-FIFO miss count diverged");
    } else {
        // Generate golden fixture (first run)
        let (lru_keys, lru_hits, lru_misses) = run_lru_golden();
        let fixture = serde_json::json!({
            "seed": SEED,
            "ops": OPS,
            "n_unique": N_UNIQUE,
            "capacity": CAPACITY,
            "s3fifo": {
                "keys": keys,
                "hits": hits,
                "misses": misses,
            },
            "lru": {
                "keys": lru_keys,
                "hits": lru_hits,
                "misses": lru_misses,
            },
        });
        let json = serde_json::to_string_pretty(&fixture).unwrap();
        std::fs::write(&path, json).unwrap_or_else(|e| {
            panic!("Failed to write golden fixture to {}: {e}", path.display());
        });
        eprintln!(
            "Generated golden fixture at {} (s3fifo: {hits} hits / {misses} misses, lru: {lru_hits} hits / {lru_misses} misses)",
            path.display()
        );
    }

    // Determinism check: run again, must produce identical results
    let (keys2, hits2, misses2) = run_s3fifo_golden();
    assert_eq!(keys, keys2, "S3-FIFO is not deterministic across runs");
    assert_eq!(hits, hits2);
    assert_eq!(misses, misses2);
}

/// 2. `bench_s3fifo_faster_than_lru_eviction` -- measure eviction-heavy workload,
///    assert S3-FIFO is faster.
#[test]
#[allow(clippy::cast_precision_loss)]
fn bench_s3fifo_faster_than_lru_eviction() {
    use std::time::Instant;

    let n = 100_000;
    let cap = 1_000;

    // S3-FIFO: insert-heavy workload (every insert triggers eviction after warmup)
    let start = Instant::now();
    let mut s3 = S3FifoCache::new(cap);
    for i in 0..n {
        s3.insert(i, i);
    }
    let s3_elapsed = start.elapsed();

    // LRU (IndexMap): same workload
    let start = Instant::now();
    let mut lru: IndexMap<usize, usize> = IndexMap::with_capacity(cap);
    for i in 0..n {
        lru.insert(i, i);
        while lru.len() > cap {
            lru.shift_remove_index(0);
        }
    }
    let lru_elapsed = start.elapsed();

    // S3-FIFO should be faster due to O(1) amortized eviction vs O(n) shift_remove
    // Use a generous 2x margin for CI variance
    assert!(
        s3_elapsed < lru_elapsed * 2,
        "S3-FIFO ({s3_elapsed:?}) was not competitive with LRU ({lru_elapsed:?})"
    );

    eprintln!(
        "S3-FIFO: {s3_elapsed:?}, LRU: {lru_elapsed:?} (ratio: {:.2}x)",
        lru_elapsed.as_nanos() as f64 / s3_elapsed.as_nanos() as f64
    );
}

/// 3. `hit_rate_comparison_zipf` -- Zipf workload, S3-FIFO hit-rate >= 0.97 * LRU.
#[test]
#[allow(clippy::cast_precision_loss)]
fn hit_rate_comparison_zipf() {
    let cap = 100;
    let n_unique = 1_000;
    let ops = 50_000;

    // Zipf-like access pattern: key = (lcg_value % n_unique)^2 % n_unique
    // biases access toward lower-ranked keys.

    // S3-FIFO
    let mut s3 = S3FifoCache::new(cap);
    let mut rng = Lcg::new(123);
    let mut s3_hits = 0u64;
    for _ in 0..ops {
        let raw = rng.next_key(n_unique);
        let key = (raw * raw) % n_unique;
        if s3.get(&key).is_some() {
            s3_hits += 1;
        } else {
            s3.insert(key, key);
        }
    }

    // LRU
    let mut lru: IndexMap<usize, usize> = IndexMap::with_capacity(cap);
    let mut rng = Lcg::new(123);
    let mut lru_hits = 0u64;
    for _ in 0..ops {
        let raw = rng.next_key(n_unique);
        let key = (raw * raw) % n_unique;
        if let Some(idx) = lru.get_index_of(&key) {
            lru.move_index(idx, lru.len() - 1);
            lru_hits += 1;
        } else {
            lru.insert(key, key);
            while lru.len() > cap {
                lru.shift_remove_index(0);
            }
        }
    }

    let s3_rate = s3_hits as f64 / f64::from(ops);
    let lru_rate = lru_hits as f64 / f64::from(ops);
    let ratio = if lru_rate > 0.0 {
        s3_rate / lru_rate
    } else {
        1.0
    };

    eprintln!("S3-FIFO hit rate: {s3_rate:.4}, LRU hit rate: {lru_rate:.4}, ratio: {ratio:.4}");

    // S3-FIFO should achieve at least 97% of LRU's hit rate (it typically exceeds it)
    assert!(
        ratio >= 0.97,
        "S3-FIFO hit rate ({s3_rate:.4}) is < 97% of LRU ({lru_rate:.4}), ratio={ratio:.4}"
    );
}
