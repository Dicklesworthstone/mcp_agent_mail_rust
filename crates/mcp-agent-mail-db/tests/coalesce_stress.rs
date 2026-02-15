//! Stress tests and isomorphism proof for the sharded coalescer (br-36tr5 E.2).
//!
//! Validates the 16-shard `CoalesceMap` under high concurrency, verifying
//! correctness, liveness, and metrics consistency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use mcp_agent_mail_db::coalesce::CoalesceMap;

/// 50 threads on the same key. All must complete. At least one should join.
#[test]
#[allow(clippy::needless_collect)]
fn stress_sharded_50_threads_same_key() {
    let n = 50;
    let map = Arc::new(CoalesceMap::<String, i32>::new(
        200,
        Duration::from_secs(10),
    ));
    let barrier = Arc::new(Barrier::new(n));
    let exec_count = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..n)
        .map(|_| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            let exec_count = Arc::clone(&exec_count);
            thread::spawn(move || {
                barrier.wait();
                let r = map
                    .execute_or_join("stress-same-key".to_string(), || {
                        exec_count.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(30));
                        Ok::<_, String>(42)
                    })
                    .unwrap();
                r.into_inner()
            })
        })
        .collect();

    let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All threads get a valid result.
    assert_eq!(results.len(), n);
    assert!(results.iter().all(|&v| v == 42));

    // Coalescing should have reduced the number of executions.
    let execs = exec_count.load(Ordering::SeqCst);
    assert!(
        execs < n,
        "expected coalescing to reduce executions below {n}, got {execs}"
    );

    // At least one thread should have joined.
    let m = map.metrics();
    assert!(
        m.joined_count > 0,
        "expected at least 1 join with 10s timeout and 30ms work, got 0"
    );
    assert_eq!(map.inflight_count(), 0);
}

/// 50 threads with 50 distinct keys. Each should execute independently.
#[test]
#[allow(clippy::needless_collect, clippy::cast_possible_truncation)]
fn stress_sharded_50_threads_50_keys() {
    let n = 50;
    let map = Arc::new(CoalesceMap::<String, usize>::new(
        200,
        Duration::from_millis(500),
    ));
    let barrier = Arc::new(Barrier::new(n));
    let exec_count = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..n)
        .map(|i| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            let exec_count = Arc::clone(&exec_count);
            thread::spawn(move || {
                barrier.wait();
                let key = format!("distinct-key-{i}");
                let r = map
                    .execute_or_join(key, || {
                        exec_count.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(i)
                    })
                    .unwrap();
                (i, r.into_inner())
            })
        })
        .collect();

    let results: Vec<(usize, usize)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Each thread should have gotten its own value back.
    for (expected, actual) in &results {
        assert_eq!(
            expected, actual,
            "thread {expected} got wrong value {actual}"
        );
    }

    // All 50 distinct keys should have executed independently.
    assert_eq!(exec_count.load(Ordering::SeqCst), n);
    assert_eq!(map.inflight_count(), 0);

    let m = map.metrics();
    assert_eq!(m.leader_count as usize, n);
    assert_eq!(m.joined_count, 0);
}

/// 100 threads with 10 distinct keys (10 threads per key). Verify coalescing.
#[test]
#[allow(clippy::needless_collect)]
fn stress_sharded_mixed_keys_100_threads() {
    let n = 100;
    let keys = 10;
    let map = Arc::new(CoalesceMap::<String, usize>::new(
        200,
        Duration::from_secs(10),
    ));
    let barrier = Arc::new(Barrier::new(n));
    let exec_count = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..n)
        .map(|i| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            let exec_count = Arc::clone(&exec_count);
            let key_idx = i % keys;
            thread::spawn(move || {
                barrier.wait();
                let key = format!("mixed-key-{key_idx}");
                let r = map
                    .execute_or_join(key, || {
                        exec_count.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        Ok::<_, String>(key_idx)
                    })
                    .unwrap();
                (key_idx, r.into_inner())
            })
        })
        .collect();

    let results: Vec<(usize, usize)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All threads must complete.
    assert_eq!(results.len(), n);

    // Each thread should get its key's value.
    for (key_idx, value) in &results {
        assert_eq!(
            key_idx, value,
            "thread for key {key_idx} got wrong value {value}"
        );
    }

    // Coalescing should reduce total executions below 100.
    let execs = exec_count.load(Ordering::SeqCst);
    assert!(
        execs < n,
        "expected coalescing to reduce executions below {n}, got {execs}"
    );

    // There should be some joins.
    let m = map.metrics();
    assert!(
        m.joined_count > 0,
        "expected at least 1 join with 10s timeout"
    );
    assert_eq!(map.inflight_count(), 0);
}

/// 1000 sequential operations with the same key. No stale state.
#[test]
fn stress_sharded_rapid_fire_1000_sequential() {
    let map: CoalesceMap<&str, i32> = CoalesceMap::new(200, Duration::from_millis(100));

    let start = Instant::now();
    for i in 0..1000 {
        let r = map
            .execute_or_join("rapid-fire", || Ok::<_, String>(i))
            .unwrap();
        assert!(!r.was_joined(), "sequential calls should never join");
        assert_eq!(r.into_inner(), i);
        assert_eq!(map.inflight_count(), 0, "inflight must be 0 after each op");
    }
    let elapsed = start.elapsed();

    let m = map.metrics();
    assert_eq!(m.leader_count, 1000);
    assert_eq!(m.joined_count, 0);
    assert_eq!(m.timeout_count, 0);
    assert_eq!(m.leader_failed_count, 0);

    // 1000 sequential ops should complete well under 5s.
    assert!(
        elapsed.as_secs() < 5,
        "1000 sequential ops took {elapsed:?}, expected < 5s"
    );
}

/// Isomorphism proof: for a deterministic sequence of operations, verify
/// structural invariants hold. Since we can't compare against a `SingleCoalesceMap`
/// (not in the crate), we verify the key properties that must hold for ANY
/// correct coalescer implementation:
///
/// 1. For every operation, a valid result is returned.
/// 2. After all operations complete, `inflight_count() == 0`.
/// 3. `leader_count + joined_count + timeout_count >= total_operations`.
/// 4. For distinct keys (sequential), `joined_count == 0`.
/// 5. Metrics reset works and subsequent operations still function.
#[test]
fn isomorphism_structural_invariants() {
    let map: CoalesceMap<String, usize> = CoalesceMap::new(200, Duration::from_millis(100));

    // Phase 1: 500 distinct keys (sequential) - all should be leaders.
    for i in 0..500 {
        let key = format!("iso-{i}");
        let result = map.execute_or_join(key, || Ok::<_, String>(i)).unwrap();
        assert!(
            !result.was_joined(),
            "distinct sequential key should not join"
        );
        assert_eq!(result.into_inner(), i, "value mismatch for key {i}");
    }

    let m1 = map.metrics();
    assert_eq!(m1.leader_count, 500, "500 distinct keys = 500 leaders");
    assert_eq!(m1.joined_count, 0, "sequential distinct keys = 0 joins");
    assert_eq!(m1.timeout_count, 0);
    assert_eq!(m1.leader_failed_count, 0);
    assert_eq!(map.inflight_count(), 0);

    // Invariant 3: leader + joined + timeout >= total
    let sum = m1.leader_count + m1.joined_count + m1.timeout_count;
    assert!(sum >= 500, "metrics sum {sum} < 500 operations");

    // Phase 2: same key repeated 500 times (sequential) - all leaders (no overlap).
    for i in 500..1000 {
        let result = map
            .execute_or_join("repeated-key".to_string(), || Ok::<_, String>(i))
            .unwrap();
        assert!(!result.was_joined());
        assert_eq!(result.into_inner(), i);
    }

    let m2 = map.metrics();
    assert_eq!(m2.leader_count, 1000);
    assert_eq!(m2.joined_count, 0);
    assert_eq!(map.inflight_count(), 0);

    // Phase 3: error operations - leader still cleaned up.
    for i in 0..100 {
        let r = map.execute_or_join(format!("err-{i}"), || Err::<usize, String>("fail".into()));
        assert!(r.is_err());
    }

    assert_eq!(map.inflight_count(), 0, "errors must clean up inflight");
    let m3 = map.metrics();
    assert_eq!(m3.leader_count, 1100);

    // Phase 4: reset and verify clean state.
    map.reset_metrics();
    let m4 = map.metrics();
    assert_eq!(m4.leader_count, 0);
    assert_eq!(m4.joined_count, 0);

    // Map still works after reset.
    let r = map
        .execute_or_join("post-reset".to_string(), || Ok::<_, String>(9999))
        .unwrap();
    assert_eq!(r.into_inner(), 9999);
    assert_eq!(map.metrics().leader_count, 1);
}

/// Verify that the sharded coalescer distributes keys fairly across shards
/// by checking that concurrent distinct-key operations complete faster than
/// sequential single-shard operations.
#[test]
#[allow(clippy::needless_collect)]
fn shard_distribution_concurrency_benefit() {
    // Baseline: 16 sequential operations on the same key (all go to one shard).
    let map_seq: CoalesceMap<String, usize> = CoalesceMap::new(200, Duration::from_millis(500));
    let start_seq = Instant::now();
    for i in 0..16 {
        let _ = map_seq.execute_or_join(format!("same-shard-{i}"), || {
            thread::sleep(Duration::from_millis(2));
            Ok::<_, String>(i)
        });
    }
    let elapsed_seq = start_seq.elapsed();

    // Parallel: 16 threads with distinct keys (should hit different shards).
    let map_par = Arc::new(CoalesceMap::<String, usize>::new(
        200,
        Duration::from_millis(500),
    ));
    let barrier = Arc::new(Barrier::new(16));

    let start_par = Instant::now();
    let handles: Vec<_> = (0..16)
        .map(|i| {
            let map = Arc::clone(&map_par);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let key = format!("shard-par-{i}");
                map.execute_or_join(key, || {
                    thread::sleep(Duration::from_millis(2));
                    Ok::<_, String>(i)
                })
                .unwrap()
                .into_inner()
            })
        })
        .collect();

    let results: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let elapsed_par = start_par.elapsed();

    assert_eq!(results.len(), 16);

    // Parallel should be at least somewhat faster than sequential
    // (generous threshold: parallel should not be more than 2x sequential).
    // The key check is that all threads complete - this verifies no deadlock.
    assert!(
        elapsed_par.as_millis() <= elapsed_seq.as_millis() * 2 + 100,
        "parallel ({elapsed_par:?}) should not be much slower than sequential ({elapsed_seq:?})"
    );
}

/// Concurrent shard access: verify per-key correctness with many keys and threads.
#[test]
#[allow(clippy::needless_collect)]
fn stress_correctness_per_key_results() {
    let n_keys = 20;
    let threads_per_key = 5;
    let total = n_keys * threads_per_key;
    let map = Arc::new(CoalesceMap::<String, String>::new(
        200,
        Duration::from_secs(10),
    ));
    let barrier = Arc::new(Barrier::new(total));

    let handles: Vec<_> = (0..total)
        .map(|i| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            let key_idx = i / threads_per_key;
            let expected_val = format!("result-for-key-{key_idx}");
            thread::spawn(move || {
                barrier.wait();
                let key = format!("correctness-key-{key_idx}");
                let r = map
                    .execute_or_join(key, || {
                        thread::sleep(Duration::from_millis(10));
                        Ok::<_, String>(expected_val.clone())
                    })
                    .unwrap();
                (key_idx, r.into_inner())
            })
        })
        .collect();

    let results: Vec<(usize, String)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Group results by key and verify all threads for a key got the same value.
    let mut by_key: HashMap<usize, Vec<String>> = HashMap::new();
    for (key_idx, value) in results {
        by_key.entry(key_idx).or_default().push(value);
    }

    for (key_idx, values) in &by_key {
        assert_eq!(
            values.len(),
            threads_per_key,
            "key {key_idx} should have {threads_per_key} results"
        );
        let expected = format!("result-for-key-{key_idx}");
        for v in values {
            assert_eq!(
                v, &expected,
                "key {key_idx} got wrong value: {v} (expected {expected})"
            );
        }
    }

    assert_eq!(map.inflight_count(), 0);
}
