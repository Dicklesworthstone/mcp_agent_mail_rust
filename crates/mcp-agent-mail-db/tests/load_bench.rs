//! 1000-agent load simulation benchmarks (br-15dv.7.2).
//!
//! Four scenarios exercising the DB layer under realistic concurrent load:
//!
//! - **Scenario A**: Registration storm — 1000 agents register across 50 threads.
//! - **Scenario B**: Message burst — 100 agents send 10 messages each.
//! - **Scenario C**: Mixed workload — 60s sustained mixed read/write operations.
//! - **Scenario D**: Thundering herd — 500 concurrent `fetch_inbox` on one project.
//!
//! Each scenario collects per-operation latencies, reports p50/p95/p99/max,
//! and asserts SLO budgets from br-15dv.10.
//!
//! # Running
//!
//! ```sh
//! cargo test -p mcp-agent-mail-db --test load_bench -- --ignored --nocapture
//! ```

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_collect
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::models::{VALID_ADJECTIVES, VALID_NOUNS};
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, QUERY_TRACKER, read_cache};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
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

fn block_on_with_retry<F, Fut, T>(max_retries: usize, f: F) -> T
where
    F: Fn(Cx) -> Fut,
    Fut: std::future::Future<Output = Outcome<T, mcp_agent_mail_db::DbError>>,
{
    for attempt in 0..=max_retries {
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        match rt.block_on(f(cx)) {
            Outcome::Ok(val) => return val,
            Outcome::Err(e) if attempt < max_retries => {
                let msg = format!("{e:?}");
                if msg.contains("locked") || msg.contains("busy") {
                    std::thread::sleep(Duration::from_millis(10 * (attempt as u64 + 1)));
                    continue;
                }
                panic!("non-retryable error on attempt {attempt}: {e:?}");
            }
            Outcome::Err(e) => panic!("failed after {max_retries} retries: {e:?}"),
            _ => panic!("unexpected outcome"),
        }
    }
    unreachable!()
}

fn make_load_pool(max_connections: usize) -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("load_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections,
        min_connections: 4_usize.min(max_connections),
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        let mut out: String = f.to_uppercase().collect();
        out.extend(c);
        out
    })
}

fn generate_agent_names(count: usize) -> Vec<String> {
    let mut names = Vec::with_capacity(count);
    'name_gen: for adj in VALID_ADJECTIVES {
        for noun in VALID_NOUNS {
            names.push(format!("{}{}", cap(adj), cap(noun)));
            if names.len() >= count {
                break 'name_gen;
            }
        }
    }
    assert!(
        names.len() >= count,
        "need {count} unique agent names, got {}",
        names.len()
    );
    names.truncate(count);
    names
}

/// Compute percentiles from a sorted slice of microsecond latencies.
struct LatencyReport {
    count: usize,
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
    errors: u64,
}

impl LatencyReport {
    fn from_latencies(latencies: &mut [u64], errors: u64) -> Self {
        latencies.sort_unstable();
        let n = latencies.len();
        if n == 0 {
            return Self {
                count: 0,
                p50: 0,
                p95: 0,
                p99: 0,
                max: 0,
                errors,
            };
        }
        Self {
            count: n,
            p50: latencies[n * 50 / 100],
            p95: latencies[n * 95 / 100],
            p99: latencies[n * 99 / 100],
            max: latencies[n - 1],
            errors,
        }
    }

    fn print(&self, label: &str) {
        eprintln!(
            "  {label}: n={}, p50={:.1}ms, p95={:.1}ms, p99={:.1}ms, max={:.1}ms, errors={}",
            self.count,
            self.p50 as f64 / 1000.0,
            self.p95 as f64 / 1000.0,
            self.p99 as f64 / 1000.0,
            self.max as f64 / 1000.0,
            self.errors,
        );
    }
}

fn run_inbox_stats_polling_phase(
    pool: &DbPool,
    receiver_id: i64,
    polls: usize,
    force_invalidate_each_poll: bool,
) -> (LatencyReport, u64) {
    let mut latencies: Vec<u64> = Vec::with_capacity(polls);
    for _ in 0..polls {
        if force_invalidate_each_poll {
            read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);
        }

        let t0 = Instant::now();
        let outcome = block_on(|cx| {
            let pp = pool.clone();
            async move { queries::get_inbox_stats(&cx, &pp, receiver_id).await }
        });
        match outcome {
            Outcome::Ok(Some(_)) => {
                latencies.push(t0.elapsed().as_micros() as u64);
            }
            other => panic!("get_inbox_stats polling failed: {other:?}"),
        }
    }

    let snapshot = QUERY_TRACKER.snapshot();
    let inbox_stats_queries = snapshot.per_table.get("inbox_stats").copied().unwrap_or(0);
    (
        LatencyReport::from_latencies(&mut latencies, 0),
        inbox_stats_queries,
    )
}

// ---------------------------------------------------------------------------
// Scenario A: Registration storm
// ---------------------------------------------------------------------------
// 1000 agents register across 50 concurrent threads (20 agents per thread).
// Budget: p95 < 50ms per registration, 0 failures.

#[test]
#[ignore = "heavy load bench: 1000-agent registration storm"]
fn load_scenario_a_registration_storm() {
    let (pool, _dir) = make_load_pool(100);
    let names = generate_agent_names(1000);
    let n_threads: usize = 50;
    let agents_per_thread: usize = 20;
    let barrier = Arc::new(Barrier::new(n_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let chunk: Vec<String> =
                names[t * agents_per_thread..(t + 1) * agents_per_thread].to_vec();

            std::thread::spawn(move || {
                let mut latencies = Vec::with_capacity(agents_per_thread);
                let mut errors: u64 = 0;

                // Ensure project first
                let human_key = format!("/data/load/reg_p{t}_{}", unique_suffix());
                let project_id = block_on_with_retry(5, |cx| {
                    let pp = pool.clone();
                    let k = human_key.clone();
                    async move { queries::ensure_project(&cx, &pp, &k).await }
                })
                .id
                .unwrap();

                barrier.wait();

                for name in &chunk {
                    let t0 = Instant::now();
                    match block_on(|cx| {
                        let pp = pool.clone();
                        let n = name.clone();
                        async move {
                            queries::register_agent(
                                &cx,
                                &pp,
                                project_id,
                                &n,
                                "load-bench",
                                "model",
                                None,
                                None,
                            )
                            .await
                        }
                    }) {
                        Outcome::Ok(_) => {
                            latencies.push(t0.elapsed().as_micros() as u64);
                        }
                        _ => errors += 1,
                    }
                }
                (latencies, errors)
            })
        })
        .collect();

    let mut all_latencies = Vec::with_capacity(1000);
    let mut total_errors: u64 = 0;
    for h in handles {
        let (lats, errs) = h.join().expect("thread should not panic");
        all_latencies.extend(lats);
        total_errors += errs;
    }

    let elapsed = start.elapsed();
    let report = LatencyReport::from_latencies(&mut all_latencies, total_errors);

    eprintln!("\n=== Scenario A: Registration Storm ===");
    eprintln!("  Total time: {:.2}s", elapsed.as_secs_f64());
    report.print("register_agent");
    eprintln!(
        "  Throughput: {:.0} registrations/s",
        report.count as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert_eq!(report.count, 1000, "expected 1000 registrations");
    assert!(
        report.p95 < 50_000,
        "SLO: p95 < 50ms, got {:.1}ms",
        report.p95 as f64 / 1000.0
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "expected < 10s, took {:.1}s",
        elapsed.as_secs_f64()
    );
}

// ---------------------------------------------------------------------------
// Scenario B: Message burst
// ---------------------------------------------------------------------------
// 100 agents send 10 messages each simultaneously (20 threads × 50 messages).
// Budget: p95 < 100ms per send, p99 < 500ms, 0 lost messages.

#[test]
#[ignore = "heavy load bench: 100-agent message burst"]
fn load_scenario_b_message_burst() {
    let (pool, _dir) = make_load_pool(100);
    let names = generate_agent_names(100);
    let n_agents: usize = 100;
    let msgs_per_agent: usize = 10;
    let n_threads: usize = 20;
    let agents_per_thread: usize = n_agents / n_threads;

    // Setup: create one project and register all agents
    let project_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        let k = format!("/data/load/burst_{}", unique_suffix());
        async move { queries::ensure_project(&cx, &pp, &k).await }
    })
    .id
    .unwrap();

    let mut agent_ids: Vec<i64> = Vec::with_capacity(n_agents);
    for name in &names {
        let aid = block_on_with_retry(5, |cx| {
            let pp = pool.clone();
            let n = name.clone();
            async move {
                queries::register_agent(&cx, &pp, project_id, &n, "load-bench", "model", None, None)
                    .await
            }
        })
        .id
        .unwrap();
        agent_ids.push(aid);
    }

    let agent_ids = Arc::new(agent_ids);
    let barrier = Arc::new(Barrier::new(n_threads));
    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let agent_ids = Arc::clone(&agent_ids);
            let start_idx = t * agents_per_thread;

            std::thread::spawn(move || {
                let mut latencies = Vec::with_capacity(agents_per_thread * msgs_per_agent);
                let mut errors: u64 = 0;

                barrier.wait();

                for a in start_idx..start_idx + agents_per_thread {
                    let sender_id = agent_ids[a];
                    for m in 0..msgs_per_agent {
                        let receiver_idx = (a + m + 1) % n_agents;
                        let receiver_id = agent_ids[receiver_idx];

                        let t0 = Instant::now();
                        match block_on(|cx| {
                            let pp = pool.clone();
                            async move {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pp,
                                    project_id,
                                    sender_id,
                                    &format!("burst-a{a}-m{m}"),
                                    &format!("body {a}-{m}"),
                                    None,
                                    "normal",
                                    false,
                                    "",
                                    &[(receiver_id, "to")],
                                )
                                .await
                            }
                        }) {
                            Outcome::Ok(_) => {
                                latencies.push(t0.elapsed().as_micros() as u64);
                            }
                            _ => errors += 1,
                        }
                    }
                }
                (latencies, errors)
            })
        })
        .collect();

    let mut all_latencies = Vec::with_capacity(n_agents * msgs_per_agent);
    let mut total_errors: u64 = 0;
    for h in handles {
        let (lats, errs) = h.join().expect("thread should not panic");
        all_latencies.extend(lats);
        total_errors += errs;
    }

    let elapsed = start.elapsed();
    let report = LatencyReport::from_latencies(&mut all_latencies, total_errors);

    eprintln!("\n=== Scenario B: Message Burst ===");
    eprintln!("  Total time: {:.2}s", elapsed.as_secs_f64());
    report.print("send_message");
    eprintln!(
        "  Throughput: {:.0} messages/s",
        report.count as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert_eq!(
        report.count,
        n_agents * msgs_per_agent,
        "expected {} messages",
        n_agents * msgs_per_agent
    );
    assert!(
        report.p95 < 100_000,
        "SLO: p95 < 100ms, got {:.1}ms",
        report.p95 as f64 / 1000.0
    );
    assert!(
        report.p99 < 500_000,
        "SLO: p99 < 500ms, got {:.1}ms",
        report.p99 as f64 / 1000.0
    );
}

// ---------------------------------------------------------------------------
// Scenario C: Mixed workload
// ---------------------------------------------------------------------------
// 1000 agents across 50 projects cycle through mixed operations for 30 seconds.
// Operation mix: 40% fetch_inbox, 30% send_message, 15% search,
//                10% file_reservations, 5% acknowledge.
// Budget: p95 < 200ms, p99 < 1s, 0 errors.

#[test]
#[ignore = "heavy load bench: 30s sustained mixed workload"]
fn load_scenario_c_mixed_workload() {
    let (pool, _dir) = make_load_pool(100);
    let names = generate_agent_names(1000);

    let n_projects: usize = 50;
    let agents_per_project: usize = 20;
    let n_threads: usize = 50;
    let duration = Duration::from_secs(30);

    // Setup: create projects and register agents
    let mut project_data: Vec<(i64, Vec<i64>)> = Vec::with_capacity(n_projects);
    for p in 0..n_projects {
        let project_id = block_on_with_retry(5, |cx| {
            let pp = pool.clone();
            let k = format!("/data/load/mixed_p{p}_{}", unique_suffix());
            async move { queries::ensure_project(&cx, &pp, &k).await }
        })
        .id
        .unwrap();

        let mut agent_ids = Vec::with_capacity(agents_per_project);
        for a in 0..agents_per_project {
            let name = &names[p * agents_per_project + a];
            let aid = block_on_with_retry(5, |cx| {
                let pp = pool.clone();
                let n = name.clone();
                async move {
                    queries::register_agent(
                        &cx,
                        &pp,
                        project_id,
                        &n,
                        "load-bench",
                        "model",
                        None,
                        None,
                    )
                    .await
                }
            })
            .id
            .unwrap();
            agent_ids.push(aid);
        }
        project_data.push((project_id, agent_ids));
    }

    // Seed some messages for fetch/search/ack operations
    for (project_id, agent_ids) in &project_data {
        for a in 0..agent_ids.len().min(5) {
            let sender = agent_ids[a];
            let receiver = agent_ids[(a + 1) % agent_ids.len()];
            let _ = block_on(|cx| {
                let pp = pool.clone();
                let pid = *project_id;
                async move {
                    queries::create_message_with_recipients(
                        &cx,
                        &pp,
                        pid,
                        sender,
                        &format!("seed-{a}"),
                        "seed body",
                        None,
                        "normal",
                        true,
                        "",
                        &[(receiver, "to")],
                    )
                    .await
                }
            });
        }
    }

    let project_data = Arc::new(project_data);
    let barrier = Arc::new(Barrier::new(n_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let project_data = Arc::clone(&project_data);

            std::thread::spawn(move || {
                let mut fetch_lats = Vec::new();
                let mut send_lats = Vec::new();
                let mut search_lats = Vec::new();
                let mut reserve_lats = Vec::new();
                let mut ack_lats = Vec::new();
                let mut errors: u64 = 0;
                let mut op_counter: u64 = 0;

                barrier.wait();

                let (project_id, agent_ids) = &project_data[t % n_projects];
                let agent_id = agent_ids[t % agent_ids.len()];
                let project_id = *project_id;

                while start.elapsed() < duration {
                    // Deterministic operation selection based on counter
                    let op = op_counter % 20;
                    op_counter += 1;

                    match op {
                        // 40% fetch_inbox (0-7)
                        0..=7 => {
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                async move {
                                    queries::fetch_inbox(
                                        &cx, &pp, project_id, agent_id, false, None, 20,
                                    )
                                    .await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    fetch_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 30% send_message (8-13)
                        8..=13 => {
                            let receiver = agent_ids[(t + op_counter as usize) % agent_ids.len()];
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                let sub = format!("mixed-t{t}-{op_counter}");
                                async move {
                                    queries::create_message_with_recipients(
                                        &cx,
                                        &pp,
                                        project_id,
                                        agent_id,
                                        &sub,
                                        "mixed workload body",
                                        None,
                                        "normal",
                                        false,
                                        "",
                                        &[(receiver, "to")],
                                    )
                                    .await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    send_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 15% search_messages (14-16)
                        14..=16 => {
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                async move {
                                    queries::search_messages(&cx, &pp, project_id, "seed", 10).await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    search_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 10% file_reservations (17-18)
                        17..=18 => {
                            let t0 = Instant::now();
                            match block_on(|cx| {
                                let pp = pool.clone();
                                let pat = format!("src/file_{op_counter}.rs");
                                async move {
                                    queries::create_file_reservations(
                                        &cx,
                                        &pp,
                                        project_id,
                                        agent_id,
                                        &[pat.as_str()],
                                        3600,
                                        true,
                                        "",
                                    )
                                    .await
                                }
                            }) {
                                Outcome::Ok(_) => {
                                    reserve_lats.push(t0.elapsed().as_micros() as u64);
                                }
                                _ => errors += 1,
                            }
                        }
                        // 5% acknowledge (19)
                        _ => {
                            // Fetch inbox first to find a message to ack
                            if let Outcome::Ok(msgs) = block_on(|cx| {
                                let pp = pool.clone();
                                async move {
                                    queries::fetch_inbox(
                                        &cx, &pp, project_id, agent_id, false, None, 1,
                                    )
                                    .await
                                }
                            }) {
                                if let Some(msg) = msgs.first() {
                                    let mid = msg.message.id.unwrap();
                                    let t0 = Instant::now();
                                    match block_on(|cx| {
                                        let pp = pool.clone();
                                        async move {
                                            queries::acknowledge_message(&cx, &pp, agent_id, mid)
                                                .await
                                        }
                                    }) {
                                        Outcome::Ok(_) => {
                                            ack_lats.push(t0.elapsed().as_micros() as u64);
                                        }
                                        _ => errors += 1,
                                    }
                                }
                            }
                        }
                    }
                }
                (
                    fetch_lats,
                    send_lats,
                    search_lats,
                    reserve_lats,
                    ack_lats,
                    errors,
                )
            })
        })
        .collect();

    let mut all_fetch = Vec::new();
    let mut all_send = Vec::new();
    let mut all_search = Vec::new();
    let mut all_reserve = Vec::new();
    let mut all_ack = Vec::new();
    let mut total_errors: u64 = 0;

    for h in handles {
        let (fetch, send, search, reserve, ack, errs) = h.join().expect("thread should not panic");
        all_fetch.extend(fetch);
        all_send.extend(send);
        all_search.extend(search);
        all_reserve.extend(reserve);
        all_ack.extend(ack);
        total_errors += errs;
    }

    let elapsed = start.elapsed();
    let total_ops =
        all_fetch.len() + all_send.len() + all_search.len() + all_reserve.len() + all_ack.len();

    let fetch_r = LatencyReport::from_latencies(&mut all_fetch, 0);
    let send_r = LatencyReport::from_latencies(&mut all_send, 0);
    let search_r = LatencyReport::from_latencies(&mut all_search, 0);
    let reserve_r = LatencyReport::from_latencies(&mut all_reserve, 0);
    let ack_r = LatencyReport::from_latencies(&mut all_ack, 0);

    // Compute combined p95/p99
    let mut combined: Vec<u64> = Vec::with_capacity(total_ops);
    combined.extend(&all_fetch);
    combined.extend(&all_send);
    combined.extend(&all_search);
    combined.extend(&all_reserve);
    combined.extend(&all_ack);
    let combined_r = LatencyReport::from_latencies(&mut combined, total_errors);

    eprintln!("\n=== Scenario C: Mixed Workload (30s sustained) ===");
    eprintln!("  Duration: {:.1}s", elapsed.as_secs_f64());
    eprintln!("  Total ops: {total_ops}");
    eprintln!(
        "  Throughput: {:.0} ops/s",
        total_ops as f64 / elapsed.as_secs_f64()
    );
    fetch_r.print("fetch_inbox (40%)");
    send_r.print("send_message (30%)");
    search_r.print("search_messages (15%)");
    reserve_r.print("file_reservation (10%)");
    ack_r.print("acknowledge (5%)");
    combined_r.print("COMBINED");

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert!(
        combined_r.p95 < 200_000,
        "SLO: combined p95 < 200ms, got {:.1}ms",
        combined_r.p95 as f64 / 1000.0
    );
    assert!(
        combined_r.p99 < 1_000_000,
        "SLO: combined p99 < 1s, got {:.1}ms",
        combined_r.p99 as f64 / 1000.0
    );
}

// ---------------------------------------------------------------------------
// Scenario D: Thundering herd
// ---------------------------------------------------------------------------
// 500 concurrent threads all call `fetch_inbox` on the same project at once.
// Budget: p95 < 500ms, 0 errors.

#[test]
#[ignore = "heavy load bench: 500-thread thundering herd"]
fn load_scenario_d_thundering_herd() {
    let (pool, _dir) = make_load_pool(100);

    // Setup: one project with 500 agents and some seeded messages
    let project_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        let k = format!("/data/load/herd_{}", unique_suffix());
        async move { queries::ensure_project(&cx, &pp, &k).await }
    })
    .id
    .unwrap();

    let names = generate_agent_names(500);
    let mut agent_ids: Vec<i64> = Vec::with_capacity(500);
    for name in &names {
        let aid = block_on_with_retry(5, |cx| {
            let pp = pool.clone();
            let n = name.clone();
            async move {
                queries::register_agent(&cx, &pp, project_id, &n, "load-bench", "model", None, None)
                    .await
            }
        })
        .id
        .unwrap();
        agent_ids.push(aid);
    }

    // Seed 50 messages so inboxes aren't trivially empty
    for i in 0..50 {
        let sender = agent_ids[i % agent_ids.len()];
        let receiver = agent_ids[(i + 1) % agent_ids.len()];
        let _ = block_on(|cx| {
            let pp = pool.clone();
            async move {
                queries::create_message_with_recipients(
                    &cx,
                    &pp,
                    project_id,
                    sender,
                    &format!("herd-seed-{i}"),
                    "herd seed body",
                    None,
                    "normal",
                    false,
                    "",
                    &[(receiver, "to")],
                )
                .await
            }
        });
    }

    let n_threads: usize = 500;
    let agent_ids = Arc::new(agent_ids);
    let barrier = Arc::new(Barrier::new(n_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let agent_ids = Arc::clone(&agent_ids);

            std::thread::spawn(move || {
                let agent_id = agent_ids[t];

                barrier.wait();

                let t0 = Instant::now();
                let result = block_on(|cx| {
                    let pp = pool.clone();
                    async move {
                        queries::fetch_inbox(&cx, &pp, project_id, agent_id, false, None, 20).await
                    }
                });

                let latency = t0.elapsed().as_micros() as u64;
                let error = !matches!(result, Outcome::Ok(_));
                (latency, error)
            })
        })
        .collect();

    let mut latencies = Vec::with_capacity(n_threads);
    let mut total_errors: u64 = 0;
    for h in handles {
        let (lat, err) = h.join().expect("thread should not panic");
        latencies.push(lat);
        if err {
            total_errors += 1;
        }
    }

    let elapsed = start.elapsed();
    let report = LatencyReport::from_latencies(&mut latencies, total_errors);

    eprintln!("\n=== Scenario D: Thundering Herd (500 concurrent) ===");
    eprintln!("  Total time: {:.2}s", elapsed.as_secs_f64());
    report.print("fetch_inbox");
    eprintln!(
        "  Throughput: {:.0} ops/s",
        report.count as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(total_errors, 0, "expected 0 errors, got {total_errors}");
    assert_eq!(report.count, 500, "expected 500 fetch_inbox calls");
    assert!(
        report.p95 < 500_000,
        "SLO: p95 < 500ms, got {:.1}ms",
        report.p95 as f64 / 1000.0
    );
}

// ---------------------------------------------------------------------------
// Scenario E: Inbox-stats polling cache effectiveness
// ---------------------------------------------------------------------------
// Compare two polling patterns for get_inbox_stats:
//   1) forced-miss polling (invalidate before each poll)
//   2) warm-cache polling (single cold miss, then repeated hits)
//
// Emits structured JSON so CI artifacts can be consumed by tooling.

#[test]
#[ignore = "benchmark scenario: inbox-stats polling cache effectiveness"]
fn load_scenario_e_inbox_stats_polling_cache_effectiveness() {
    let (pool, _dir) = make_load_pool(32);
    let polls: usize = 1000;
    let polls_u64 = u64::try_from(polls).expect("poll count fits u64");

    let project_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        let key = format!("/data/load/inbox_stats_polling_{}", unique_suffix());
        async move { queries::ensure_project(&cx, &pp, &key).await }
    })
    .id
    .unwrap();

    let sender_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        async move {
            queries::register_agent(
                &cx,
                &pp,
                project_id,
                "BoldCastle",
                "load-bench",
                "model",
                None,
                None,
            )
            .await
        }
    })
    .id
    .unwrap();

    let receiver_id = block_on_with_retry(5, |cx| {
        let pp = pool.clone();
        async move {
            queries::register_agent(
                &cx,
                &pp,
                project_id,
                "QuietLake",
                "load-bench",
                "model",
                None,
                None,
            )
            .await
        }
    })
    .id
    .unwrap();

    // Seed inbox_stats materialized row with a realistic payload.
    for i in 0..50 {
        let required_ack = i % 2 == 0;
        let out = block_on(|cx| {
            let pp = pool.clone();
            async move {
                queries::create_message_with_recipients(
                    &cx,
                    &pp,
                    project_id,
                    sender_id,
                    &format!("polling-seed-{i}"),
                    "seed body for inbox stats polling benchmark",
                    None,
                    "normal",
                    required_ack,
                    "",
                    &[(receiver_id, "to")],
                )
                .await
            }
        });
        assert!(
            matches!(out, Outcome::Ok(_)),
            "seed message creation failed at index {i}"
        );
    }

    QUERY_TRACKER.enable(None);
    QUERY_TRACKER.reset();

    read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);
    let forced_start = Instant::now();
    let (forced_report, forced_db_queries) =
        run_inbox_stats_polling_phase(&pool, receiver_id, polls, true);
    let forced_elapsed = forced_start.elapsed();

    QUERY_TRACKER.reset();
    read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);
    let warm_start = Instant::now();
    let (warm_report, warm_db_queries) =
        run_inbox_stats_polling_phase(&pool, receiver_id, polls, false);
    let warm_elapsed = warm_start.elapsed();

    QUERY_TRACKER.disable();
    QUERY_TRACKER.reset();
    read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), receiver_id);

    let forced_hit_ratio = (polls_u64.saturating_sub(forced_db_queries)) as f64 / polls_u64 as f64;
    let warm_hit_ratio = (polls_u64.saturating_sub(warm_db_queries)) as f64 / polls_u64 as f64;
    let query_reduction_factor = if warm_db_queries == 0 {
        forced_db_queries as f64
    } else {
        forced_db_queries as f64 / warm_db_queries as f64
    };

    eprintln!("\n=== Scenario E: Inbox Stats Polling Cache Effectiveness ===");
    forced_report.print("forced-miss polling");
    warm_report.print("warm-cache polling");
    eprintln!(
        "  forced elapsed={:.2}ms, warm elapsed={:.2}ms",
        forced_elapsed.as_secs_f64() * 1000.0,
        warm_elapsed.as_secs_f64() * 1000.0
    );
    eprintln!(
        "  DB queries (inbox_stats): forced={forced_db_queries}, warm={warm_db_queries}, reduction={query_reduction_factor:.2}x"
    );
    eprintln!(
        "  estimated hit ratio: forced={:.2}%, warm={:.2}%",
        forced_hit_ratio * 100.0,
        warm_hit_ratio * 100.0
    );

    let metrics = serde_json::json!({
        "scenario": "load_scenario_e_inbox_stats_polling_cache_effectiveness",
        "polls": polls,
        "forced_miss": {
            "count": forced_report.count,
            "p50_ms": forced_report.p50 as f64 / 1000.0,
            "p95_ms": forced_report.p95 as f64 / 1000.0,
            "p99_ms": forced_report.p99 as f64 / 1000.0,
            "max_ms": forced_report.max as f64 / 1000.0,
            "elapsed_ms": forced_elapsed.as_secs_f64() * 1000.0,
            "db_queries_inbox_stats": forced_db_queries,
            "estimated_cache_hit_ratio": forced_hit_ratio
        },
        "warm_cache": {
            "count": warm_report.count,
            "p50_ms": warm_report.p50 as f64 / 1000.0,
            "p95_ms": warm_report.p95 as f64 / 1000.0,
            "p99_ms": warm_report.p99 as f64 / 1000.0,
            "max_ms": warm_report.max as f64 / 1000.0,
            "elapsed_ms": warm_elapsed.as_secs_f64() * 1000.0,
            "db_queries_inbox_stats": warm_db_queries,
            "estimated_cache_hit_ratio": warm_hit_ratio
        },
        "comparison": {
            "query_reduction_factor": query_reduction_factor,
            "warm_vs_forced_p50_ratio": if forced_report.p50 == 0 {
                0.0
            } else {
                warm_report.p50 as f64 / forced_report.p50 as f64
            }
        }
    });
    eprintln!("BENCH_JSON {metrics}");

    assert!(
        forced_db_queries >= polls_u64.saturating_mul(95) / 100,
        "forced-miss polling should issue DB queries on almost every poll (got {forced_db_queries}/{polls})"
    );
    assert!(
        warm_db_queries <= polls_u64 / 20 + 2,
        "warm-cache polling should issue very few DB queries (got {warm_db_queries}/{polls})"
    );
    assert!(
        warm_hit_ratio > forced_hit_ratio,
        "warm-cache polling should yield a higher hit ratio (forced={forced_hit_ratio:.4}, warm={warm_hit_ratio:.4})"
    );
}
