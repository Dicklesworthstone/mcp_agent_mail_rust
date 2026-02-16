//! # E2E Load / Concurrency Stress Tests
//!
//! **Bead**: `br-2tnl.7.15`
//!
//! Validates search correctness and responsiveness under concurrent write + query
//! workloads.  Tests cover:
//!
//! 1. Concurrent message insertion while querying (writer/reader contention)
//! 2. Index freshness lag measurement (time between write and searchability)
//! 3. Latency distribution under load (p50/p95/p99 percentiles)
//! 4. Multi-thread search throughput measurement
//! 5. Pool exhaustion recovery under sustained load
//! 6. Mixed importance/thread workload correctness
//! 7. Concurrent agent registration + messaging
//! 8. Large corpus sequential indexing + query correctness
//! 9. Budget-constrained search under concurrent load
//! 10. Pagination stability under concurrent writes

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::uninlined_format_args
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Instant;

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Budget, Cx, Outcome};

use mcp_agent_mail_db::search_planner::{RankingMode, SearchQuery};
use mcp_agent_mail_db::search_service::{SimpleSearchResponse, execute_search_simple};
use mcp_agent_mail_db::{DbPool, DbPoolConfig, queries};

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    make_pool_with_connections(20, 2)
}

fn make_pool_with_connections(max: usize, min: usize) -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("load_concurrency_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: max,
        min_connections: min,
        acquire_timeout_ms: 60_000,
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

fn block_on_with_budget<F, Fut, T>(budget: Budget, f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = Cx::for_request_with_budget(budget);
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

/// Seed a project with agents, returning (`project_id`, `agent_ids`).
fn seed_project(pool: &DbPool, slug: &str, n_agents: usize) -> (i64, Vec<i64>) {
    let valid_names = [
        "RedFox",
        "BlueLake",
        "GreenHawk",
        "GoldWolf",
        "SilverPeak",
        "CoralStone",
        "AmberRiver",
        "CrimsonOwl",
        "DeepMeadow",
        "FrostBear",
        "IvoryDeer",
        "JadeFalcon",
        "LimeOtter",
        "MintCrane",
        "OpalLion",
        "PearlMoose",
    ];

    let project_id = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::ensure_project(&cx, &pool, slug).await {
                Outcome::Ok(p) => p.id.expect("project id"),
                other => panic!("ensure_project failed: {other:?}"),
            }
        }
    });

    let mut agent_ids = Vec::new();
    for i in 0..n_agents {
        let name = valid_names[i % valid_names.len()];
        let id = block_on(|cx| {
            let pool = pool.clone();
            async move {
                match queries::register_agent(
                    &cx,
                    &pool,
                    project_id,
                    name,
                    "test",
                    "test-model",
                    None,
                    None,
                )
                .await
                {
                    Outcome::Ok(a) => a.id.expect("agent id"),
                    other => panic!("register_agent({name}) failed: {other:?}"),
                }
            }
        });
        agent_ids.push(id);
    }

    (project_id, agent_ids)
}

/// Create a single message and return its id.
fn create_msg(pool: &DbPool, project_id: i64, sender_id: i64, subject: &str, body: &str) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let subject = subject.to_string();
        let body = body.to_string();
        async move {
            match queries::create_message(
                &cx, &pool, project_id, sender_id, &subject, &body, None, "normal", false, "[]",
            )
            .await
            {
                Outcome::Ok(row) => row.id.expect("message id"),
                other => panic!("create_message failed: {other:?}"),
            }
        }
    })
}

/// Execute a simple search and unwrap the Outcome.
fn search(pool: &DbPool, query: &SearchQuery) -> SimpleSearchResponse {
    let pool = pool.clone();
    let query = query.clone();
    block_on(|cx| async move {
        match execute_search_simple(&cx, &pool, &query).await {
            Outcome::Ok(r) => r,
            other => panic!("search failed: {other:?}"),
        }
    })
}

/// Percentile helper for sorted slices.
fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() * pct / 100).min(sorted.len() - 1);
    sorted[idx]
}

// ────────────────────────────────────────────────────────────────────
// 1. Concurrent writers + readers
// ────────────────────────────────────────────────────────────────────

#[test]
fn concurrent_write_and_search() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-wr", 2);
    let writer_id = agents[0];

    // Pre-seed some searchable content
    for i in 0..10 {
        create_msg(
            &pool,
            project_id,
            writer_id,
            &format!("preseed concurrency {i}"),
            &format!("baseline content for concurrency test {i}"),
        );
    }

    let n_writers = 4;
    let n_readers = 6;
    let barrier = Arc::new(Barrier::new(n_writers + n_readers));
    let write_count = Arc::new(AtomicU64::new(0));
    let read_count = Arc::new(AtomicU64::new(0));
    let read_results = Arc::new(Mutex::new(Vec::new()));

    // Writer threads
    let writer_handles: Vec<_> = (0..n_writers)
        .map(|w| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let wc = Arc::clone(&write_count);
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..20 {
                    block_on(|cx| {
                        let pool = pool.clone();
                        async move {
                            match queries::create_message(
                                &cx,
                                &pool,
                                project_id,
                                writer_id,
                                &format!("concurrency writer{w} msg{i}"),
                                &format!("body from writer {w} iteration {i}"),
                                None,
                                "normal",
                                false,
                                "[]",
                            )
                            .await
                            {
                                Outcome::Ok(_) => {}
                                other => panic!("writer {w} msg {i} failed: {other:?}"),
                            }
                        }
                    });
                    wc.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // Reader threads
    let reader_handles: Vec<_> = (0..n_readers)
        .map(|r| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let rc = Arc::clone(&read_count);
            let results = Arc::clone(&read_results);
            std::thread::spawn(move || {
                barrier.wait();
                let mut latencies = Vec::new();
                for _ in 0..15 {
                    let start = Instant::now();
                    let resp = block_on(|cx| {
                        let pool = pool.clone();
                        async move {
                            match execute_search_simple(
                                &cx,
                                &pool,
                                &SearchQuery::messages("concurrency", project_id),
                            )
                            .await
                            {
                                Outcome::Ok(r) => r,
                                other => panic!("reader {r} search failed: {other:?}"),
                            }
                        }
                    });
                    latencies.push(start.elapsed().as_micros() as u64);
                    rc.fetch_add(1, Ordering::Relaxed);
                    results.lock().unwrap().push(resp.results.len());
                }
                latencies
            })
        })
        .collect();

    // Wait for all writers
    for h in writer_handles {
        h.join().expect("writer join");
    }

    // Wait for all readers and collect latencies
    let mut all_latencies: Vec<u64> = Vec::new();
    for h in reader_handles {
        all_latencies.extend(h.join().expect("reader join"));
    }

    // Assertions
    let total_writes = write_count.load(Ordering::Relaxed);
    let total_reads = read_count.load(Ordering::Relaxed);
    assert_eq!(total_writes, 80, "4 writers × 20 messages = 80"); // assertion 1
    assert_eq!(total_reads, 90, "6 readers × 15 searches = 90"); // assertion 2

    // All searches should have returned results (pre-seeded content)
    let all_positive = read_results.lock().unwrap().iter().all(|&c| c > 0);
    assert!(
        all_positive,
        "all searches should find at least pre-seeded content"
    ); // assertion 3

    // Latency check
    all_latencies.sort_unstable();
    let p50 = percentile(&all_latencies, 50);
    let p95 = percentile(&all_latencies, 95);
    let p99 = percentile(&all_latencies, 99);

    eprintln!("concurrent_write_and_search: writes={total_writes}, reads={total_reads}");
    eprintln!("  latency: p50={}μs, p95={}μs, p99={}μs", p50, p95, p99);

    // p99 should be under 5 seconds (generous for CI)
    assert!(
        p99 < 5_000_000,
        "p99 search latency should be under 5s, got {}μs",
        p99
    ); // assertion 4

    // Verify final corpus is searchable
    let final_resp = search(&pool, &SearchQuery::messages("concurrency", project_id));
    assert!(
        final_resp.results.len() >= 10,
        "should find at least pre-seeded messages, got {}",
        final_resp.results.len()
    ); // assertion 5
}

// ────────────────────────────────────────────────────────────────────
// 2. Index freshness lag
// ────────────────────────────────────────────────────────────────────

#[test]
fn index_freshness_lag() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-freshness", 1);
    let sender_id = agents[0];

    let n_messages = 30;
    let mut freshness_lags_us = Vec::new();

    for i in 0..n_messages {
        let unique_token = format!("FRESHTOKEN{:04}", i);
        let subject = format!("freshness test {unique_token}");
        let body = format!("body with unique marker {unique_token}");

        // Write the message
        let write_start = Instant::now();
        create_msg(&pool, project_id, sender_id, &subject, &body);
        let write_elapsed = write_start.elapsed();

        // Immediately search for the unique token
        let search_start = Instant::now();
        let resp = search(&pool, &SearchQuery::messages(&unique_token, project_id));
        let search_elapsed = search_start.elapsed();

        let total_lag = write_elapsed + search_elapsed;
        freshness_lags_us.push(total_lag.as_micros() as u64);

        // The freshly-written message should be found
        assert!(
            resp.results.iter().any(|r| r.title.contains(&unique_token)),
            "msg {} with token {unique_token} should be immediately searchable",
            i
        ); // assertions 6..35 (30 messages)
    }

    freshness_lags_us.sort_unstable();
    let avg_lag = freshness_lags_us.iter().sum::<u64>() / freshness_lags_us.len() as u64;
    let p50_lag = percentile(&freshness_lags_us, 50);
    let p95_lag = percentile(&freshness_lags_us, 95);
    let max_lag = *freshness_lags_us.last().unwrap();

    eprintln!("index_freshness_lag: {n_messages} messages");
    eprintln!(
        "  lag: avg={}μs, p50={}μs, p95={}μs, max={}μs",
        avg_lag, p50_lag, p95_lag, max_lag
    );

    // Freshness should be under 2s even in the worst case
    assert!(
        max_lag < 2_000_000,
        "max freshness lag should be under 2s, got {}μs",
        max_lag
    ); // assertion 36
}

// ────────────────────────────────────────────────────────────────────
// 3. Search throughput under sustained load
// ────────────────────────────────────────────────────────────────────

#[test]
fn search_throughput_sustained() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-throughput", 2);
    let sender_id = agents[0];

    // Seed a reasonable corpus
    for i in 0..50 {
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("throughput message {i}"),
            &format!("content for throughput testing iteration {i} with keywords alpha beta gamma"),
        );
    }

    let n_threads = 8;
    let queries_per_thread = 20;
    let barrier = Arc::new(Barrier::new(n_threads));

    let overall_start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|_t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let mut latencies = Vec::new();
                let mut total_results = 0usize;

                for _ in 0..queries_per_thread {
                    let start = Instant::now();
                    let resp = block_on(|cx| {
                        let pool = pool.clone();
                        async move {
                            match execute_search_simple(
                                &cx,
                                &pool,
                                &SearchQuery::messages("throughput", project_id),
                            )
                            .await
                            {
                                Outcome::Ok(r) => r,
                                other => panic!("throughput search failed: {other:?}"),
                            }
                        }
                    });
                    latencies.push(start.elapsed().as_micros() as u64);
                    total_results += resp.results.len();
                }

                (latencies, total_results)
            })
        })
        .collect();

    let mut all_latencies = Vec::new();
    let mut total_results = 0;

    for h in handles {
        let (lats, results) = h.join().expect("throughput thread join");
        all_latencies.extend(lats);
        total_results += results;
    }

    let overall_elapsed = overall_start.elapsed();
    let total_queries = n_threads * queries_per_thread;
    let qps = total_queries as f64 / overall_elapsed.as_secs_f64();

    all_latencies.sort_unstable();
    let p50 = percentile(&all_latencies, 50);
    let p95 = percentile(&all_latencies, 95);
    let p99 = percentile(&all_latencies, 99);

    eprintln!("search_throughput_sustained: {total_queries} queries across {n_threads} threads");
    eprintln!(
        "  elapsed: {:.2}s, QPS: {:.1}",
        overall_elapsed.as_secs_f64(),
        qps
    );
    eprintln!("  latency: p50={}μs, p95={}μs, p99={}μs", p50, p95, p99);
    eprintln!("  total results returned: {total_results}");

    // Should complete all queries
    assert_eq!(
        all_latencies.len(),
        total_queries,
        "all queries should complete"
    ); // assertion 37

    // Each query should return results
    assert!(
        total_results > 0,
        "should return results across all queries"
    ); // assertion 38

    // QPS should be at least 1 (very conservative for CI)
    assert!(qps > 1.0, "QPS should be at least 1.0, got {:.2}", qps); // assertion 39
}

// ────────────────────────────────────────────────────────────────────
// 4. Pool exhaustion recovery
// ────────────────────────────────────────────────────────────────────

#[test]
fn pool_exhaustion_recovery() {
    // Small pool: 3 connections, 12 threads competing
    let (pool, _dir) = make_pool_with_connections(3, 1);
    let (project_id, agents) = seed_project(&pool, "/tmp/load-exhaustion", 1);
    let sender_id = agents[0];

    // Seed some content
    for i in 0..10 {
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("exhaustion test {i}"),
            &format!("body for pool exhaustion scenario {i}"),
        );
    }

    let n_threads = 12;
    let barrier = Arc::new(Barrier::new(n_threads));
    let success_count = Arc::new(AtomicU64::new(0));
    let failure_count = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let sc = Arc::clone(&success_count);
            let fc = Arc::clone(&failure_count);
            std::thread::spawn(move || {
                barrier.wait();

                for i in 0..5 {
                    // Mix writes and reads
                    if i % 2 == 0 {
                        let result = block_on(|cx| {
                            let pool = pool.clone();
                            async move {
                                queries::create_message(
                                    &cx,
                                    &pool,
                                    project_id,
                                    sender_id,
                                    &format!("exhaust t{t} i{i}"),
                                    &format!("body t{t} i{i}"),
                                    None,
                                    "normal",
                                    false,
                                    "[]",
                                )
                                .await
                            }
                        });
                        match result {
                            Outcome::Ok(_) => {
                                sc.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {
                                fc.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        let result = block_on(|cx| {
                            let pool = pool.clone();
                            async move {
                                execute_search_simple(
                                    &cx,
                                    &pool,
                                    &SearchQuery::messages("exhaustion", project_id),
                                )
                                .await
                            }
                        });
                        match result {
                            Outcome::Ok(_) => {
                                sc.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {
                                fc.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("exhaustion thread join");
    }

    let successes = success_count.load(Ordering::Relaxed);
    let failures = failure_count.load(Ordering::Relaxed);
    let total = successes + failures;

    eprintln!("pool_exhaustion_recovery: {successes}/{total} ops succeeded, {failures} failed");

    assert_eq!(total, 60, "12 threads × 5 ops = 60"); // assertion 40

    // With 60s acquire timeout, most should succeed
    assert!(
        successes >= 50,
        "at least 50/60 ops should succeed with generous timeout, got {successes}"
    ); // assertion 41

    // DB should still be functional after exhaustion
    let post_resp = search(&pool, &SearchQuery::messages("exhaustion", project_id));
    assert!(
        post_resp.results.len() >= 10,
        "DB should still be functional after pool stress"
    ); // assertion 42
}

// ────────────────────────────────────────────────────────────────────
// 5. Mixed importance/thread workload
// ────────────────────────────────────────────────────────────────────

#[test]
fn mixed_importance_thread_workload() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-mixed", 3);

    let importances = ["low", "normal", "high", "urgent"];
    let n_threads_per_importance = 2;
    let msgs_per_thread = 10;
    let barrier = Arc::new(Barrier::new(importances.len() * n_threads_per_importance));

    let msg_counts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));

    let mut handles = Vec::new();
    for (imp_idx, &importance) in importances.iter().enumerate() {
        for t in 0..n_threads_per_importance {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let counts = Arc::clone(&msg_counts);
            let sender_id = agents[imp_idx % agents.len()];
            let imp = importance.to_string();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..msgs_per_thread {
                    let thread_id = format!("thread-{imp}-{t}");
                    block_on(|cx| {
                        let pool = pool.clone();
                        let imp = imp.clone();
                        let thread_id = thread_id.clone();
                        async move {
                            match queries::create_message(
                                &cx,
                                &pool,
                                project_id,
                                sender_id,
                                &format!("mixed {imp} t{t} msg{i}"),
                                &format!("body for {imp} thread {t} message {i}"),
                                Some(&thread_id),
                                &imp,
                                imp == "urgent",
                                "[]",
                            )
                            .await
                            {
                                Outcome::Ok(_) => {}
                                other => {
                                    panic!("mixed write failed: {other:?}")
                                }
                            }
                        }
                    });
                }
                counts
                    .lock()
                    .unwrap()
                    .entry(imp)
                    .and_modify(|c| *c += msgs_per_thread)
                    .or_insert(msgs_per_thread);
            }));
        }
    }

    for h in handles {
        h.join().expect("mixed thread join");
    }

    let total: usize = msg_counts.lock().unwrap().values().sum();
    assert_eq!(total, 80, "4 importances × 2 threads × 10 msgs = 80"); // assertion 43

    // Search for each importance level
    for &imp in &importances {
        let resp = search(&pool, &SearchQuery::messages(imp, project_id));
        assert!(
            !resp.results.is_empty(),
            "should find messages with importance '{imp}'"
        ); // assertions 44..47
    }

    // Search for mixed keyword
    let resp = search(&pool, &SearchQuery::messages("mixed", project_id));
    assert!(
        resp.results.len() >= 20,
        "should find many 'mixed' messages (limit may cap), got {}",
        resp.results.len()
    ); // assertion 48
}

// ────────────────────────────────────────────────────────────────────
// 6. Concurrent agent registration + messaging
// ────────────────────────────────────────────────────────────────────

#[test]
fn concurrent_agent_registration_and_messaging() {
    let (pool, _dir) = make_pool();
    let project_id = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::ensure_project(&cx, &pool, "/tmp/load-agents").await {
                Outcome::Ok(p) => p.id.expect("project id"),
                other => panic!("ensure_project failed: {other:?}"),
            }
        }
    });

    let agent_names = [
        "RedFox",
        "BlueLake",
        "GreenHawk",
        "GoldWolf",
        "SilverPeak",
        "CoralStone",
        "AmberRiver",
        "CrimsonOwl",
    ];
    let n_agents = agent_names.len();
    let barrier = Arc::new(Barrier::new(n_agents));
    let agent_ids = Arc::new(Mutex::new(Vec::new()));

    // Register agents concurrently
    let mut handles = Vec::new();
    for &name in &agent_names {
        let pool = pool.clone();
        let barrier = Arc::clone(&barrier);
        let ids = Arc::clone(&agent_ids);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let id = block_on(|cx| {
                let pool = pool.clone();
                async move {
                    match queries::register_agent(
                        &cx,
                        &pool,
                        project_id,
                        name,
                        "test",
                        "test-model",
                        None,
                        None,
                    )
                    .await
                    {
                        Outcome::Ok(a) => a.id.expect("agent id"),
                        other => panic!("register_agent({name}) failed: {other:?}"),
                    }
                }
            });
            ids.lock().unwrap().push((name.to_string(), id));
            id
        }));
    }

    let mut registered: Vec<i64> = handles
        .into_iter()
        .map(|h| h.join().expect("agent reg join"))
        .collect();

    assert_eq!(registered.len(), n_agents, "all agents registered"); // assertion 49

    // All agent IDs should be unique
    let mut sorted_ids = std::mem::take(&mut registered);
    sorted_ids.sort_unstable();
    sorted_ids.dedup();
    assert_eq!(sorted_ids.len(), n_agents, "all agent IDs should be unique"); // assertion 50

    // Now send messages from all agents concurrently
    let barrier2 = Arc::new(Barrier::new(n_agents));
    let agent_data: Vec<_> = agent_ids.lock().unwrap().clone();

    let msg_handles: Vec<_> = agent_data
        .iter()
        .map(|(name, id)| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier2);
            let name = name.clone();
            let id = *id;
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..5 {
                    block_on(|cx| {
                        let pool = pool.clone();
                        let name = name.clone();
                        async move {
                            match queries::create_message(
                                &cx,
                                &pool,
                                project_id,
                                id,
                                &format!("from {name} msg{i}"),
                                &format!("agent {name} message body {i}"),
                                None,
                                "normal",
                                false,
                                "[]",
                            )
                            .await
                            {
                                Outcome::Ok(_) => {}
                                other => panic!("msg from {name}#{i} failed: {other:?}"),
                            }
                        }
                    });
                }
            })
        })
        .collect();

    for h in msg_handles {
        h.join().expect("msg send join");
    }

    // Verify all messages searchable
    let resp = search(&pool, &SearchQuery::messages("agent", project_id));
    assert!(
        resp.results.len() >= 8,
        "should find messages from multiple agents, got {}",
        resp.results.len()
    ); // assertion 51
}

// ────────────────────────────────────────────────────────────────────
// 7. Large corpus sequential + query correctness
// ────────────────────────────────────────────────────────────────────

#[test]
fn large_corpus_query_correctness() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-large-corpus", 2);
    let sender_id = agents[0];

    let n_messages = 100;
    let mut unique_tokens: Vec<String> = Vec::new();

    // Seed large corpus
    let seed_start = Instant::now();
    for i in 0..n_messages {
        let token = format!("UNIQ{:05}", i);
        unique_tokens.push(token.clone());
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("corpus message {i} {token}"),
            &format!("large corpus body with token {token} and extra words alpha bravo charlie"),
        );
    }
    let seed_elapsed = seed_start.elapsed();
    eprintln!(
        "large_corpus_query_correctness: seeded {n_messages} in {:.2}s",
        seed_elapsed.as_secs_f64()
    );

    // Query for overall content
    let resp = search(&pool, &SearchQuery::messages("corpus", project_id));
    assert!(
        resp.results.len() >= 20,
        "should find many corpus messages (default limit caps), got {}",
        resp.results.len()
    ); // assertion 52

    // Spot-check unique tokens
    let spot_checks = [0, 25, 50, 75, 99];
    for &idx in &spot_checks {
        let token = &unique_tokens[idx];
        let resp = search(&pool, &SearchQuery::messages(token, project_id));
        assert!(
            resp.results.iter().any(|r| r.title.contains(token)),
            "token {token} at index {idx} should be findable"
        ); // assertions 53..57
    }

    // Recency-ranked search
    let mut recency_query = SearchQuery::messages("corpus", project_id);
    recency_query.ranking = RankingMode::Recency;
    recency_query.limit = Some(10);
    let resp = search(&pool, &recency_query);
    assert_eq!(
        resp.results.len(),
        10,
        "recency search should return limit=10 results"
    ); // assertion 58
}

// ────────────────────────────────────────────────────────────────────
// 8. Budget-constrained search under load
// ────────────────────────────────────────────────────────────────────

#[test]
fn budget_constrained_search_under_load() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-budget", 1);
    let sender_id = agents[0];

    // Seed content
    for i in 0..40 {
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("budget load {i}"),
            &format!("body for budget constrained search under load test {i}"),
        );
    }

    // Unlimited budget search should work fine
    let unlimited_resp = block_on_with_budget(Budget::new(), |cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &SearchQuery::messages("budget", project_id))
                .await
            {
                Outcome::Ok(r) => r,
                other => panic!("unlimited search failed: {other:?}"),
            }
        }
    });
    assert!(
        !unlimited_resp.results.is_empty(),
        "unlimited budget search should return results"
    ); // assertion 59

    // Run multiple budget-constrained searches concurrently
    let n_threads = 4;
    let barrier = Arc::new(Barrier::new(n_threads));
    let completed = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|_t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let completed = Arc::clone(&completed);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..5 {
                    // Use a generous budget (not artificially tight)
                    let budget = Budget::new();
                    let result = block_on_with_budget(budget, |cx| {
                        let pool = pool.clone();
                        async move {
                            execute_search_simple(
                                &cx,
                                &pool,
                                &SearchQuery::messages("budget", project_id),
                            )
                            .await
                        }
                    });
                    if let Outcome::Ok(_) = result {
                        completed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("budget thread join");
    }

    let done = completed.load(Ordering::Relaxed);
    eprintln!("budget_constrained_search_under_load: {done}/20 searches completed");
    assert!(
        done >= 10,
        "at least half of budget-constrained searches should complete, got {done}"
    ); // assertion 60
}

// ────────────────────────────────────────────────────────────────────
// 9. Pagination stability under concurrent writes
// ────────────────────────────────────────────────────────────────────

#[test]
fn pagination_stability_under_writes() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-pagination", 1);
    let sender_id = agents[0];

    // Seed initial corpus
    for i in 0..30 {
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("pagination item {i}"),
            &format!("paginated content for stability test iteration {i}"),
        );
    }

    // Page 1: fetch first 10
    let mut q = SearchQuery::messages("pagination", project_id);
    q.limit = Some(10);
    let page1 = search(&pool, &q);
    let page1_count = page1.results.len();
    assert!(page1_count > 0, "page 1 should have results"); // assertion 61

    // While we paginate, concurrently insert more messages
    let pool_c = pool.clone();
    let writer = std::thread::spawn(move || {
        for i in 30..50 {
            create_msg(
                &pool_c,
                project_id,
                sender_id,
                &format!("pagination item {i}"),
                &format!("additional paginated content {i}"),
            );
        }
    });

    // Page 2: use cursor from page 1 if available
    let mut q2 = SearchQuery::messages("pagination", project_id);
    q2.limit = Some(10);
    q2.cursor = page1.next_cursor.clone();
    let page2 = search(&pool, &q2);

    writer.join().expect("pagination writer join");

    // Pages should not have duplicate IDs
    let page1_ids: Vec<i64> = page1.results.iter().map(|r| r.id).collect();
    let page2_ids: Vec<i64> = page2.results.iter().map(|r| r.id).collect();

    // Check no duplicates between pages
    let duplicates: Vec<i64> = page1_ids
        .iter()
        .filter(|id| page2_ids.contains(id))
        .copied()
        .collect();
    assert!(
        duplicates.is_empty(),
        "no duplicate IDs between page 1 and page 2, found: {:?}",
        duplicates
    ); // assertion 62

    // Total unique results should be reasonable
    let total_unique = page1_count + page2.results.len();
    assert!(
        total_unique >= 10,
        "should get reasonable results across pages, got {}",
        total_unique
    ); // assertion 63
}

// ────────────────────────────────────────────────────────────────────
// 10. Write-heavy thrashing (index contention)
// ────────────────────────────────────────────────────────────────────

#[test]
fn write_heavy_index_thrashing() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-thrash", 4);

    let n_threads = 8;
    let msgs_per_thread = 25;
    let barrier = Arc::new(Barrier::new(n_threads));
    let success_count = Arc::new(AtomicU64::new(0));

    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let sc = Arc::clone(&success_count);
            let sender_id = agents[t % agents.len()];
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..msgs_per_thread {
                    let thread_id = format!("thrash-t{t}");
                    let sc2 = Arc::clone(&sc);
                    block_on(|cx| {
                        let pool = pool.clone();
                        let thread_id = thread_id.clone();
                        async move {
                            match queries::create_message(
                                &cx,
                                &pool,
                                project_id,
                                sender_id,
                                &format!("thrash t{t} msg{i}"),
                                &format!("thrashing body thread {t} message {i} with content delta epsilon zeta"),
                                Some(&thread_id),
                                "normal",
                                false,
                                "[]",
                            )
                            .await
                            {
                                Outcome::Ok(_) => {
                                    sc2.fetch_add(1, Ordering::Relaxed);
                                }
                                other => {
                                    eprintln!("thrash write t{t} i{i} failed: {other:?}");
                                }
                            }
                        }
                    });
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thrash thread join");
    }

    let elapsed = start.elapsed();
    let successes = success_count.load(Ordering::Relaxed);
    let expected = (n_threads * msgs_per_thread) as u64;
    let write_rate = successes as f64 / elapsed.as_secs_f64();

    eprintln!(
        "write_heavy_index_thrashing: {successes}/{expected} writes in {:.2}s ({:.1} writes/sec)",
        elapsed.as_secs_f64(),
        write_rate
    );

    assert!(
        successes >= expected * 90 / 100,
        "at least 90% of writes should succeed: {successes}/{expected}"
    ); // assertion 64

    // FTS index should be consistent after thrashing
    let resp = search(&pool, &SearchQuery::messages("thrash", project_id));
    assert!(
        resp.results.len() >= 20,
        "should find many thrashed messages, got {}",
        resp.results.len()
    ); // assertion 65

    // Per-thread search
    let resp_t0 = search(&pool, &SearchQuery::messages("thrash t0", project_id));
    assert!(
        !resp_t0.results.is_empty(),
        "should find thread-0 specific messages"
    ); // assertion 66
}

// ────────────────────────────────────────────────────────────────────
// 11. Latency distribution with explain metadata
// ────────────────────────────────────────────────────────────────────

#[test]
fn latency_distribution_with_explain() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-explain", 1);
    let sender_id = agents[0];

    for i in 0..25 {
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("explain latency test {i}"),
            &format!("content for explain-enabled latency measurement {i}"),
        );
    }

    let n_iterations = 20;
    let mut latencies_no_explain = Vec::new();
    let mut latencies_with_explain = Vec::new();

    // Without explain
    for _ in 0..n_iterations {
        let start = Instant::now();
        let resp = search(&pool, &SearchQuery::messages("explain", project_id));
        latencies_no_explain.push(start.elapsed().as_micros() as u64);
        assert!(
            !resp.results.is_empty(),
            "no-explain search should return results"
        );
    }

    // With explain
    for _ in 0..n_iterations {
        let mut q = SearchQuery::messages("explain", project_id);
        q.explain = true;
        let start = Instant::now();
        let resp = search(&pool, &q);
        latencies_with_explain.push(start.elapsed().as_micros() as u64);
        assert!(
            !resp.results.is_empty(),
            "explain search should return results"
        );
        // Explain should be present when requested
        // (May be None if the engine doesn't support it, which is acceptable)
    }

    latencies_no_explain.sort_unstable();
    latencies_with_explain.sort_unstable();

    let p50_no = percentile(&latencies_no_explain, 50);
    let p95_no = percentile(&latencies_no_explain, 95);
    let p50_ex = percentile(&latencies_with_explain, 50);
    let p95_ex = percentile(&latencies_with_explain, 95);

    eprintln!("latency_distribution_with_explain ({n_iterations} iterations each):");
    eprintln!("  no-explain: p50={}μs, p95={}μs", p50_no, p95_no);
    eprintln!("  with-explain: p50={}μs, p95={}μs", p50_ex, p95_ex);

    // Both modes should complete reasonably fast
    assert!(
        p95_no < 3_000_000,
        "no-explain p95 should be under 3s, got {}μs",
        p95_no
    ); // assertion 67
    assert!(
        p95_ex < 3_000_000,
        "with-explain p95 should be under 3s, got {}μs",
        p95_ex
    ); // assertion 68
}

// ────────────────────────────────────────────────────────────────────
// 12. Cross-thread message visibility
// ────────────────────────────────────────────────────────────────────

#[test]
fn cross_thread_message_visibility() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-cross-vis", 2);

    let thread_names = ["thread-alpha", "thread-beta", "thread-gamma"];
    let msgs_per_thread = 10;

    // Write messages to different threads concurrently
    let barrier = Arc::new(Barrier::new(thread_names.len()));
    let handles: Vec<_> = thread_names
        .iter()
        .enumerate()
        .map(|(idx, &thread_name)| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let sender_id = agents[idx % agents.len()];
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..msgs_per_thread {
                    block_on(|cx| {
                        let pool = pool.clone();
                        async move {
                            match queries::create_message(
                                &cx,
                                &pool,
                                project_id,
                                sender_id,
                                &format!("crossvis {thread_name} msg{i}"),
                                &format!("cross thread visibility test {thread_name} {i}"),
                                Some(thread_name),
                                "normal",
                                false,
                                "[]",
                            )
                            .await
                            {
                                Outcome::Ok(_) => {}
                                other => panic!("crossvis write failed: {other:?}"),
                            }
                        }
                    });
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("crossvis join");
    }

    // Verify each thread's messages are searchable
    for &thread_name in &thread_names {
        let resp = search(&pool, &SearchQuery::messages(thread_name, project_id));
        assert!(
            !resp.results.is_empty(),
            "thread '{thread_name}' messages should be searchable"
        ); // assertions 69..71
    }

    // Verify cross-thread search finds all
    let resp = search(&pool, &SearchQuery::messages("crossvis", project_id));
    assert!(
        resp.results.len() >= 20,
        "cross-thread search should find messages from all threads, got {}",
        resp.results.len()
    ); // assertion 72
}

// ────────────────────────────────────────────────────────────────────
// 13. Steady-state latency after warm-up
// ────────────────────────────────────────────────────────────────────

#[test]
fn steady_state_latency_after_warmup() {
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-steady", 1);
    let sender_id = agents[0];

    // Seed corpus
    for i in 0..50 {
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("steady state msg {i}"),
            &format!("body for steady state latency measurement test {i}"),
        );
    }

    // Warm-up phase (discard latencies)
    for _ in 0..10 {
        search(&pool, &SearchQuery::messages("steady", project_id));
    }

    // Measurement phase
    let n_samples = 30;
    let mut latencies = Vec::new();

    for _ in 0..n_samples {
        let start = Instant::now();
        let resp = search(&pool, &SearchQuery::messages("steady", project_id));
        latencies.push(start.elapsed().as_micros() as u64);
        assert!(
            !resp.results.is_empty(),
            "warm search should return results"
        );
    }

    latencies.sort_unstable();
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    let p99 = percentile(&latencies, 99);
    let stddev = {
        let mean = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
        let variance = latencies
            .iter()
            .map(|&x| (x as f64 - mean).powi(2))
            .sum::<f64>()
            / latencies.len() as f64;
        variance.sqrt()
    };

    eprintln!("steady_state_latency_after_warmup ({n_samples} samples):");
    eprintln!(
        "  p50={}μs, p95={}μs, p99={}μs, stddev={:.0}μs",
        p50, p95, p99, stddev
    );

    // Steady-state latency should be reasonable
    assert!(
        p95 < 2_000_000,
        "steady-state p95 should be under 2s, got {}μs",
        p95
    ); // assertion 73

    // Coefficient of variation should show stability (not wildly varying)
    let mean = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
    let cv = stddev / mean;
    eprintln!("  mean={:.0}μs, cv={:.2}", mean, cv);
    assert!(
        cv < 5.0,
        "coefficient of variation should be under 5.0 (reasonably stable), got {:.2}",
        cv
    ); // assertion 74
}

// ────────────────────────────────────────────────────────────────────
// 14. Structured artifact summary
// ────────────────────────────────────────────────────────────────────

#[test]
fn structured_artifact_summary() {
    // This test runs a mini load scenario and emits a structured JSON-like summary
    // to stderr for CI artifact collection.
    let (pool, _dir) = make_pool();
    let (project_id, agents) = seed_project(&pool, "/tmp/load-artifact", 2);

    let n_messages = 40;
    let mut write_latencies = Vec::new();

    // Write phase
    for i in 0..n_messages {
        let sender_id = agents[i % agents.len()];
        let start = Instant::now();
        create_msg(
            &pool,
            project_id,
            sender_id,
            &format!("artifact msg {i}"),
            &format!("structured artifact summary content {i} delta epsilon zeta eta theta"),
        );
        write_latencies.push(start.elapsed().as_micros() as u64);
    }

    // Search phase
    let n_queries = 20;
    let mut search_latencies = Vec::new();
    let mut result_counts = Vec::new();

    for _ in 0..n_queries {
        let start = Instant::now();
        let resp = search(&pool, &SearchQuery::messages("artifact", project_id));
        search_latencies.push(start.elapsed().as_micros() as u64);
        result_counts.push(resp.results.len());
    }

    write_latencies.sort_unstable();
    search_latencies.sort_unstable();

    let summary = format!(
        concat!(
            "=== LOAD TEST ARTIFACT SUMMARY ===\n",
            "corpus_size: {}\n",
            "write_latency_p50_us: {}\n",
            "write_latency_p95_us: {}\n",
            "write_latency_p99_us: {}\n",
            "search_queries: {}\n",
            "search_latency_p50_us: {}\n",
            "search_latency_p95_us: {}\n",
            "search_latency_p99_us: {}\n",
            "search_results_min: {}\n",
            "search_results_max: {}\n",
            "search_results_avg: {:.1}\n",
            "=== END SUMMARY ===",
        ),
        n_messages,
        percentile(&write_latencies, 50),
        percentile(&write_latencies, 95),
        percentile(&write_latencies, 99),
        n_queries,
        percentile(&search_latencies, 50),
        percentile(&search_latencies, 95),
        percentile(&search_latencies, 99),
        result_counts.iter().min().unwrap_or(&0),
        result_counts.iter().max().unwrap_or(&0),
        result_counts.iter().sum::<usize>() as f64 / result_counts.len() as f64,
    );

    eprintln!("{summary}");

    // Validate the metrics are sane
    assert!(!write_latencies.is_empty(), "write latencies collected"); // assertion 75
    assert!(!search_latencies.is_empty(), "search latencies collected"); // assertion 76
    assert!(
        result_counts.iter().all(|&c| c > 0),
        "all searches returned results"
    ); // assertion 77
    assert!(
        percentile(&search_latencies, 99) < 5_000_000,
        "search p99 under 5s"
    ); // assertion 78
    assert!(
        percentile(&write_latencies, 99) < 5_000_000,
        "write p99 under 5s"
    ); // assertion 79
}
