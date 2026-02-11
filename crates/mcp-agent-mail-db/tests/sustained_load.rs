//! Sustained load test: 100 RPS for configurable duration with stability assertions.
//!
//! Tests sustained throughput over time to reveal problems that burst tests miss:
//! memory leaks, cache degradation, pool connection aging, WAL growth.
//!
//! Run:
//!   cargo test --test sustained_load -- --ignored --nocapture
//!
//! Extended (300 seconds, per bead spec):
//!   SUSTAINED_LOAD_SECS=300 cargo test --test sustained_load -- --ignored --nocapture
//!
//! Custom rate:
//!   SUSTAINED_LOAD_RPS=200 SUSTAINED_LOAD_SECS=60 cargo test --test sustained_load -- --ignored --nocapture

#![allow(
    clippy::needless_collect,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::manual_let_else,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::redundant_closure_for_method_calls,
    clippy::significant_drop_tightening
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::metrics::Log2Histogram;
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ===========================================================================
// Helpers
// ===========================================================================

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

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        let mut out: String = f.to_uppercase().collect();
        out.extend(c);
        out
    })
}

/// Read resident set size from /proc/self/statm (Linux only).
fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages * 4) // 4 KB pages
}

/// Read `SQLite` WAL file size.
fn wal_size_bytes(db_path: &str) -> u64 {
    let wal = format!("{db_path}-wal");
    std::fs::metadata(&wal).map_or(0, |m| m.len())
}

// ===========================================================================
// Timestamp-based rate limiter (lock-free token bucket)
// ===========================================================================

struct RateLimiter {
    start: Instant,
    consumed: AtomicU64,
    rate_per_sec: u64,
}

impl RateLimiter {
    fn new(rate_per_sec: u64) -> Self {
        Self {
            start: Instant::now(),
            consumed: AtomicU64::new(0),
            rate_per_sec,
        }
    }

    /// Block until a token is available, maintaining the target rate.
    fn wait_for_token(&self) {
        loop {
            let elapsed_us = u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX);
            let available = elapsed_us * self.rate_per_sec / 1_000_000;
            let consumed = self.consumed.load(Ordering::Relaxed);
            if consumed < available {
                if self
                    .consumed
                    .compare_exchange_weak(
                        consumed,
                        consumed + 1,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return;
                }
            } else {
                // Sleep until the next token is due
                let next_at_us = (consumed + 1) * 1_000_000 / self.rate_per_sec;
                let wait_us = next_at_us.saturating_sub(elapsed_us);
                if wait_us > 0 {
                    std::thread::sleep(Duration::from_micros(wait_us.min(50_000)));
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }

    fn ops_done(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }
}

// ===========================================================================
// Operation classification (weighted distribution)
// ===========================================================================

#[derive(Clone, Copy)]
enum OpType {
    FetchInbox,  // 40%
    SendMessage, // 30%
    Search,      // 15%
    Reservation, // 10%
    Acknowledge, // 5%
}

impl OpType {
    const fn from_index(i: u64) -> Self {
        match i % 100 {
            0..=39 => Self::FetchInbox,
            40..=69 => Self::SendMessage,
            70..=84 => Self::Search,
            85..=94 => Self::Reservation,
            _ => Self::Acknowledge,
        }
    }
}

// ===========================================================================
// Periodic measurement snapshot
// ===========================================================================

#[derive(Debug)]
struct Snapshot {
    elapsed_secs: u64,
    ops_total: u64,
    actual_rps: f64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    errors: u64,
    rss_kb: u64,
    wal_bytes: u64,
    health_level: String,
}

// ===========================================================================
// Main test
// ===========================================================================

#[test]
#[ignore = "sustained load test: 100 RPS for 30-300 seconds"]
fn sustained_100_rps_load_test() {
    let duration_secs: u64 = std::env::var("SUSTAINED_LOAD_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let target_rps: u64 = std::env::var("SUSTAINED_LOAD_RPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let n_workers: usize = 50;

    eprintln!(
        "\n=== Sustained load test: {target_rps} RPS for {duration_secs}s with {n_workers} workers ==="
    );

    // ── Pool setup ──
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("sustained_{}.db", unique_suffix()));
    let db_path_str = db_path.display().to_string();
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{db_path_str}"),
        max_connections: 100,
        min_connections: 25,
        acquire_timeout_ms: 15_000,
        max_lifetime_ms: 1_800_000,
        run_migrations: true,
        warmup_connections: 10,
    };
    let pool = DbPool::new(&config).expect("create pool");
    std::mem::forget(dir); // prevent cleanup while threads running

    // Warmup pool connections
    block_on(|cx| {
        let p = pool.clone();
        async move {
            let _ = p.warmup(&cx, 10, Duration::from_secs(10)).await;
        }
    });

    // ── Create project + agents ──
    let n_agents: usize = 20;
    let human_key = format!("/data/sustained/proj_{}", unique_suffix());
    let (project_id, agent_ids) = {
        let p = pool.clone();
        block_on(|cx| async move {
            let proj = match queries::ensure_project(&cx, &p, &human_key).await {
                Outcome::Ok(r) => r,
                other => panic!("ensure_project: {other:?}"),
            };
            let pid = proj.id.unwrap();

            let adj = mcp_agent_mail_core::VALID_ADJECTIVES;
            let noun = mcp_agent_mail_core::VALID_NOUNS;
            let mut ids = Vec::with_capacity(n_agents);
            for i in 0..n_agents {
                let name = format!("{}{}", cap(adj[i % adj.len()]), cap(noun[i % noun.len()]));
                let agent = match queries::register_agent(
                    &cx,
                    &p,
                    pid,
                    &name,
                    "load-test",
                    "load-model",
                    Some("sustained load worker"),
                    None,
                )
                .await
                {
                    Outcome::Ok(a) => a,
                    other => panic!("register_agent {name}: {other:?}"),
                };
                ids.push(agent.id.unwrap());
            }
            (pid, ids)
        })
    };

    // ── Pre-populate messages for search + ack ──
    let pre_msg_count: usize = 200;
    let ackable_msg_ids: Vec<i64> = {
        let p = pool.clone();
        let aids = agent_ids.clone();
        block_on(|cx| async move {
            let mut ackable = Vec::new();
            for i in 0..pre_msg_count {
                let sender_idx = i % n_agents;
                let receiver_idx = (i + 1) % n_agents;
                let ack = i % 10 == 0; // 10% are ack-required
                let msg = match queries::create_message_with_recipients(
                    &cx,
                    &p,
                    project_id,
                    aids[sender_idx],
                    &format!("stress sustained test message {i}"),
                    &format!("body for sustained load test iteration {i} with searchable keywords"),
                    None,
                    "normal",
                    ack,
                    "",
                    &[(aids[receiver_idx], "to")],
                )
                .await
                {
                    Outcome::Ok(m) => m,
                    other => panic!("pre-populate msg {i}: {other:?}"),
                };
                if ack {
                    ackable.push(msg.id.unwrap());
                }
            }
            ackable
        })
    };

    let ackable_ids: Arc<Vec<i64>> = Arc::new(ackable_msg_ids);
    let agent_ids: Arc<Vec<i64>> = Arc::new(agent_ids);

    eprintln!(
        "Setup: project_id={project_id}, agents={n_agents}, pre-populated={pre_msg_count} msgs, ackable={}",
        ackable_ids.len()
    );

    // ── Shared metrics ──
    let op_latency = Arc::new(Log2Histogram::new());
    let ops_completed = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let rate_limiter = Arc::new(RateLimiter::new(target_rps));

    let initial_rss = rss_kb();

    // ── Monitor thread (samples every 10s) ──
    let snapshots: Arc<std::sync::Mutex<Vec<Snapshot>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let monitor_handle = {
        let running = Arc::clone(&running);
        let ops = Arc::clone(&ops_completed);
        let errors = Arc::clone(&error_count);
        let latency = Arc::clone(&op_latency);
        let snaps = Arc::clone(&snapshots);
        let db_path_s = db_path_str.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let interval = Duration::from_secs(10);
            while running.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = start.elapsed().as_secs();
                let total_ops = ops.load(Ordering::Relaxed);
                let rps = if elapsed > 0 {
                    total_ops as f64 / elapsed as f64
                } else {
                    0.0
                };
                let snap = latency.snapshot();
                let health = mcp_agent_mail_core::cached_health_level();
                let s = Snapshot {
                    elapsed_secs: elapsed,
                    ops_total: total_ops,
                    actual_rps: rps,
                    p50_us: snap.p50,
                    p95_us: snap.p95,
                    p99_us: snap.p99,
                    max_us: snap.max,
                    errors: errors.load(Ordering::Relaxed),
                    rss_kb: rss_kb(),
                    wal_bytes: wal_size_bytes(&db_path_s),
                    health_level: health.as_str().to_string(),
                };
                eprintln!(
                    "  [{:>4}s] ops={:<6} rps={:<7.1} p50={:<7}μs p95={:<7}μs p99={:<7}μs max={:<7}μs errs={} rss={}KB wal={}B health={}",
                    s.elapsed_secs,
                    s.ops_total,
                    s.actual_rps,
                    s.p50_us,
                    s.p95_us,
                    s.p99_us,
                    s.max_us,
                    s.errors,
                    s.rss_kb,
                    s.wal_bytes,
                    s.health_level,
                );
                snaps.lock().unwrap().push(s);
            }
        })
    };

    // ── Worker threads ──
    let start = Instant::now();
    let deadline = Duration::from_secs(duration_secs);

    let handles: Vec<_> = (0..n_workers)
        .map(|worker_id| {
            let pool = pool.clone();
            let limiter = Arc::clone(&rate_limiter);
            let ops = Arc::clone(&ops_completed);
            let errors = Arc::clone(&error_count);
            let latency = Arc::clone(&op_latency);
            let running = Arc::clone(&running);
            let agents = Arc::clone(&agent_ids);
            let ackables = Arc::clone(&ackable_ids);

            std::thread::spawn(move || {
                let mut local_ops: u64 = 0;

                while running.load(Ordering::Relaxed) && start.elapsed() < deadline {
                    limiter.wait_for_token();
                    if !running.load(Ordering::Relaxed) || start.elapsed() >= deadline {
                        break;
                    }

                    let op_idx = limiter.ops_done();
                    let op = OpType::from_index(op_idx);
                    let op_start = Instant::now();

                    // Extract IDs before closures to avoid moving Arc
                    let n = agents.len();
                    let agent_idx = (worker_id + local_ops as usize) % n;
                    let agent_id = agents[agent_idx];
                    let receiver_id = agents[(agent_idx + 1) % n];

                    let result: Result<(), String> = match op {
                        OpType::FetchInbox => {
                            let p = pool.clone();
                            block_on(|cx| async move {
                                match queries::fetch_inbox(
                                    &cx, &p, project_id, agent_id, false, None, 20,
                                )
                                .await
                                {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => Err(format!("fetch_inbox: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::SendMessage => {
                            let p = pool.clone();
                            let subj = format!("load w{worker_id} op{local_ops}");
                            let body = format!(
                                "sustained load body from worker {worker_id} op {local_ops}"
                            );
                            block_on(|cx| async move {
                                match queries::create_message_with_recipients(
                                    &cx,
                                    &p,
                                    project_id,
                                    agent_id,
                                    &subj,
                                    &body,
                                    None,
                                    "normal",
                                    false,
                                    "",
                                    &[(receiver_id, "to")],
                                )
                                .await
                                {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => Err(format!("send: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Search => {
                            let p = pool.clone();
                            let term = match local_ops % 4 {
                                0 => "stress",
                                1 => "sustained",
                                2 => "test",
                                _ => "load",
                            };
                            block_on(|cx| async move {
                                match queries::search_messages(&cx, &p, project_id, term, 20).await
                                {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => Err(format!("search: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Reservation => {
                            let p = pool.clone();
                            let path = format!("src/worker_{worker_id}/file_{local_ops}.rs");
                            block_on(|cx| async move {
                                match queries::create_file_reservations(
                                    &cx,
                                    &p,
                                    project_id,
                                    agent_id,
                                    &[path.as_str()],
                                    300,
                                    true,
                                    "load test",
                                )
                                .await
                                {
                                    Outcome::Ok(reservations) => {
                                        let ids: Vec<i64> =
                                            reservations.iter().filter_map(|r| r.id).collect();
                                        if !ids.is_empty() {
                                            let _ =
                                                queries::release_reservations_by_ids(&cx, &p, &ids)
                                                    .await;
                                        }
                                        Ok(())
                                    }
                                    Outcome::Err(e) => Err(format!("reservation: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Acknowledge => {
                            if ackables.is_empty() {
                                Ok(())
                            } else {
                                let msg_idx = (local_ops as usize) % ackables.len();
                                let msg_id = ackables[msg_idx];
                                let p = pool.clone();
                                block_on(|cx| async move {
                                    // Both ops are idempotent; NotFound for
                                    // non-recipients is expected.
                                    let _ =
                                        queries::mark_message_read(&cx, &p, agent_id, msg_id).await;
                                    match queries::acknowledge_message(&cx, &p, agent_id, msg_id)
                                        .await
                                    {
                                        Outcome::Ok(_) => Ok(()),
                                        Outcome::Err(e) => {
                                            // Agent may not be recipient of this
                                            // message; that's expected under
                                            // random assignment.
                                            let msg = format!("{e:?}");
                                            if msg.contains("NotFound")
                                                || msg.contains("not a recipient")
                                                || msg.contains("no row")
                                            {
                                                Ok(())
                                            } else {
                                                Err(format!("ack: {e:?}"))
                                            }
                                        }
                                        _ => Err("cancelled".into()),
                                    }
                                })
                            }
                        }
                    };

                    let op_us = u64::try_from(op_start.elapsed().as_micros()).unwrap_or(u64::MAX);
                    latency.record(op_us);

                    match result {
                        Ok(()) => {
                            ops.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("ERROR [w{worker_id}]: {e}");
                            errors.fetch_add(1, Ordering::Relaxed);
                            ops.fetch_add(1, Ordering::Relaxed); // count errors as ops
                        }
                    }

                    local_ops += 1;
                }
            })
        })
        .collect();

    // Wait for workers
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    running.store(false, Ordering::Relaxed);
    monitor_handle.join().expect("monitor thread");

    // ── Collect final metrics ──
    let total_ops = ops_completed.load(Ordering::Relaxed);
    let total_errors = error_count.load(Ordering::Relaxed);
    let elapsed = start.elapsed();
    // Compute RPS over the configured active window, not the drain period.
    // Workers stop initiating ops at deadline but may still be completing in-flight ops.
    let active_secs = elapsed.as_secs_f64().min(duration_secs as f64 + 1.0);
    let final_rps = total_ops as f64 / active_secs;
    let final_snap = op_latency.snapshot();
    let final_rss = rss_kb();
    let rss_growth_kb = final_rss.saturating_sub(initial_rss);
    let final_wal = wal_size_bytes(&db_path_str);

    eprintln!("\n=== Final Results ===");
    eprintln!("Duration:   {:.1}s", elapsed.as_secs_f64());
    eprintln!("Total ops:  {total_ops}");
    eprintln!("Actual RPS: {final_rps:.1} (target: {target_rps})");
    eprintln!("Errors:     {total_errors}");
    eprintln!(
        "Latency:    p50={}μs p95={}μs p99={}μs max={}μs",
        final_snap.p50, final_snap.p95, final_snap.p99, final_snap.max
    );
    eprintln!(
        "RSS growth: {}KB ({:.1}MB)",
        rss_growth_kb,
        rss_growth_kb as f64 / 1024.0
    );
    eprintln!(
        "WAL size:   {}B ({:.1}MB)",
        final_wal,
        final_wal as f64 / (1024.0 * 1024.0)
    );

    // ── Time-series summary ──
    let snaps = snapshots.lock().unwrap();
    if !snaps.is_empty() {
        eprintln!(
            "\n{:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10} {:>8}",
            "secs", "ops", "rps", "p50μs", "p95μs", "p99μs", "maxμs", "rss_KB", "health"
        );
        for s in snaps.iter() {
            eprintln!(
                "{:>6} {:>8} {:>8.1} {:>8} {:>8} {:>8} {:>8} {:>10} {:>8}",
                s.elapsed_secs,
                s.ops_total,
                s.actual_rps,
                s.p50_us,
                s.p95_us,
                s.p99_us,
                s.max_us,
                s.rss_kb,
                s.health_level,
            );
        }
    }

    // ── Assertions ──

    // 1. Zero errors
    assert_eq!(total_errors, 0, "expected zero errors, got {total_errors}");

    // 2. Throughput within 10% of target (average over entire run)
    let min_rps = target_rps as f64 * 0.9;
    assert!(
        final_rps >= min_rps,
        "average RPS {final_rps:.1} below 90% of target ({min_rps:.1})"
    );

    // 3. P99 latency < 2 seconds (2,000,000 microseconds)
    let max_p99_us: u64 = 2_000_000;
    assert!(
        final_snap.p99 <= max_p99_us,
        "P99 latency {}μs ({:.1}ms) exceeds 2s limit",
        final_snap.p99,
        final_snap.p99 as f64 / 1000.0,
    );

    // 4. Memory RSS growth < 100MB (102,400 KB)
    let max_rss_growth_kb: u64 = 100 * 1024;
    assert!(
        rss_growth_kb <= max_rss_growth_kb,
        "RSS grew by {}KB ({:.1}MB), exceeds 100MB limit",
        rss_growth_kb,
        rss_growth_kb as f64 / 1024.0,
    );

    // 5. No throughput degradation over time
    //    Compare first-half average RPS vs second-half: second half should not
    //    be more than 20% slower than first half.
    if snaps.len() >= 4 {
        let mid = snaps.len() / 2;
        // Compute interval RPS from delta ops / delta time between snapshots
        let interval_rps = |i: usize| -> f64 {
            if i == 0 {
                if snaps[0].elapsed_secs > 0 {
                    snaps[0].ops_total as f64 / snaps[0].elapsed_secs as f64
                } else {
                    0.0
                }
            } else {
                let dt = snaps[i]
                    .elapsed_secs
                    .saturating_sub(snaps[i - 1].elapsed_secs);
                let dops = snaps[i].ops_total.saturating_sub(snaps[i - 1].ops_total);
                if dt > 0 { dops as f64 / dt as f64 } else { 0.0 }
            }
        };

        let first_half_rps: f64 = (0..mid).map(&interval_rps).sum::<f64>() / mid as f64;
        let second_half_rps: f64 =
            (mid..snaps.len()).map(interval_rps).sum::<f64>() / (snaps.len() - mid) as f64;

        if first_half_rps > 10.0 {
            let degradation = (first_half_rps - second_half_rps) / first_half_rps;
            assert!(
                degradation < 0.2,
                "throughput degraded {:.1}% over time (first half: {first_half_rps:.1} RPS, \
                 second half: {second_half_rps:.1} RPS)",
                degradation * 100.0,
            );
        }
    }

    eprintln!("\n=== PASS: sustained {target_rps} RPS for {duration_secs}s ===");
}

// ===========================================================================
// Multi-project soak test (br-3vwi.9.3)
// ===========================================================================
//
// Replays multi-project workloads across N projects × M agents with:
// - Deterministic seeding via `SOAK_SEED` env var
// - JSON artifact output for CI consumption
// - Threshold rules for leak/drift/degradation detection
//
// Run:
//   cargo test --test sustained_load multi_project_soak -- --ignored --nocapture
//
// Configuration:
//   SOAK_SEED=42                  Deterministic operation sequence (default: 0)
//   SOAK_PROJECTS=10              Number of projects (default: 10)
//   SOAK_AGENTS_PER_PROJECT=10   Agents per project (default: 10)
//   SUSTAINED_LOAD_RPS=100       Target RPS (default: 100)
//   SUSTAINED_LOAD_SECS=60       Duration in seconds (default: 60)
//
// Threshold budgets (fail with actionable diagnostics):
//   - P99 latency < 3s
//   - RSS growth < 150MB
//   - Zero errors
//   - Throughput degradation < 25% (second half vs first half)
//   - WAL growth < 200MB

/// Deterministic pseudo-random number generator (xorshift64).
/// Seeded for reproducible workload sequences.
struct Rng64 {
    state: u64,
}

impl Rng64 {
    const fn new(seed: u64) -> Self {
        // Ensure non-zero state
        Self {
            state: if seed == 0 { 0x517c_c1b7_2722_0a95 } else { seed },
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Return a value in `[0, bound)`.
    fn next_bounded(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        self.next() % bound
    }
}

/// JSON-serializable soak report for CI artifact consumption.
#[derive(serde::Serialize)]
struct MultiProjectSoakReport {
    generated_at: String,
    bead: &'static str,
    seed: u64,
    n_projects: usize,
    agents_per_project: usize,
    target_rps: u64,
    duration_secs: u64,
    actual_duration_secs: f64,
    total_ops: u64,
    actual_rps: f64,
    errors: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    baseline_rss_kb: u64,
    final_rss_kb: u64,
    rss_growth_kb: u64,
    wal_bytes: u64,
    per_project_ops: Vec<u64>,
    snapshots: Vec<SoakTimeSeriesEntry>,
    thresholds: ThresholdResults,
    verdict: String,
}

#[derive(serde::Serialize)]
struct SoakTimeSeriesEntry {
    elapsed_secs: u64,
    ops_total: u64,
    actual_rps: f64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    errors: u64,
    rss_kb: u64,
    wal_bytes: u64,
    health_level: String,
}

#[derive(Clone, serde::Serialize)]
struct ThresholdResults {
    p99_budget_us: u64,
    p99_actual_us: u64,
    p99_pass: bool,
    rss_budget_kb: u64,
    rss_growth_kb: u64,
    rss_pass: bool,
    wal_budget_bytes: u64,
    wal_actual_bytes: u64,
    wal_pass: bool,
    errors_pass: bool,
    degradation_budget_pct: f64,
    degradation_actual_pct: f64,
    degradation_pass: bool,
}

fn save_soak_artifact(report: &MultiProjectSoakReport) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .join(format!(
            "tests/artifacts/soak/multi_project/{ts}_seed{}_{}",
            report.seed,
            std::process::id()
        ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("report.json");
    let json = serde_json::to_string_pretty(report).unwrap_or_default();
    let _ = std::fs::write(&path, &json);
    eprintln!("soak artifact: {}", path.display());
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "multi-project soak: N projects × M agents for configurable duration"]
fn multi_project_soak_replay() {
    let seed = env_u64("SOAK_SEED", 0);
    let n_projects = env_u64("SOAK_PROJECTS", 10) as usize;
    let agents_per_project = env_u64("SOAK_AGENTS_PER_PROJECT", 10) as usize;
    let target_rps = env_u64("SUSTAINED_LOAD_RPS", 100);
    let duration_secs = env_u64("SUSTAINED_LOAD_SECS", 60);
    let n_workers: usize = 50;
    let total_agents = n_projects * agents_per_project;

    // Threshold budgets
    let p99_budget_us: u64 = 3_000_000;     // 3s
    let rss_budget_kb: u64 = 150 * 1024;    // 150MB
    let wal_budget_bytes: u64 = 200 * 1024 * 1024; // 200MB
    let degradation_budget_pct: f64 = 25.0;

    eprintln!(
        "\n=== Multi-project soak (br-3vwi.9.3): seed={seed}, {n_projects} projects × {agents_per_project} agents, {target_rps} RPS for {duration_secs}s ==="
    );

    // ── Pool setup ──
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("soak_multi_{}.db", unique_suffix()));
    let db_path_str = db_path.display().to_string();
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{db_path_str}"),
        max_connections: 100,
        min_connections: 25,
        acquire_timeout_ms: 15_000,
        max_lifetime_ms: 1_800_000,
        run_migrations: true,
        warmup_connections: 10,
    };
    let pool = DbPool::new(&config).expect("create pool");
    std::mem::forget(dir);

    // Warmup pool connections
    block_on(|cx| {
        let p = pool.clone();
        async move {
            let _ = p.warmup(&cx, 10, Duration::from_secs(10)).await;
        }
    });

    // ── Setup projects + agents with deterministic seeding ──
    let adj = mcp_agent_mail_core::VALID_ADJECTIVES;
    let noun = mcp_agent_mail_core::VALID_NOUNS;

    struct ProjectContext {
        project_id: i64,
        agent_ids: Vec<i64>,
    }

    let mut projects: Vec<ProjectContext> = Vec::with_capacity(n_projects);

    for p_idx in 0..n_projects {
        let human_key = format!("/data/soak/proj_s{seed}_p{p_idx}_{}", unique_suffix());
        let project_id = {
            let p = pool.clone();
            block_on(|cx| async move {
                match queries::ensure_project(&cx, &p, &human_key).await {
                    Outcome::Ok(r) => r.id.unwrap(),
                    other => panic!("ensure_project p{p_idx}: {other:?}"),
                }
            })
        };

        let mut agent_ids = Vec::with_capacity(agents_per_project);
        for a_idx in 0..agents_per_project {
            let global_idx = p_idx * agents_per_project + a_idx;
            let name = format!(
                "{}{}",
                cap(adj[global_idx % adj.len()]),
                cap(noun[global_idx % noun.len()])
            );
            let aid = {
                let p = pool.clone();
                block_on(|cx| async move {
                    match queries::register_agent(
                        &cx, &p, project_id, &name, "soak-test", "soak-model",
                        Some("multi-project soak worker"), None,
                    )
                    .await
                    {
                        Outcome::Ok(a) => a.id.unwrap(),
                        other => panic!("register_agent {name}: {other:?}"),
                    }
                })
            };
            agent_ids.push(aid);
        }

        // Seed messages per project (10 per project for search + ack targets)
        for i in 0..10_usize.min(agents_per_project) {
            let sender = agent_ids[i];
            let receiver = agent_ids[(i + 1) % agent_ids.len()];
            let p = pool.clone();
            block_on(|cx| async move {
                let _ = queries::create_message_with_recipients(
                    &cx, &p, project_id, sender,
                    &format!("soak-seed-p{p_idx}-{i}"),
                    &format!("seed body for soak project {p_idx} message {i}"),
                    None, "normal", i % 3 == 0, "",
                    &[(receiver, "to")],
                )
                .await;
            });
        }

        projects.push(ProjectContext {
            project_id,
            agent_ids,
        });
    }

    eprintln!(
        "Setup complete: {} projects, {} total agents, seeded {} messages",
        n_projects,
        total_agents,
        n_projects * 10_usize.min(agents_per_project),
    );

    // ── Shared metrics ──
    let op_latency = Arc::new(Log2Histogram::new());
    let ops_completed = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let rate_limiter = Arc::new(RateLimiter::new(target_rps));
    let per_project_ops: Arc<Vec<AtomicU64>> = Arc::new(
        (0..n_projects)
            .map(|_| AtomicU64::new(0))
            .collect(),
    );

    let initial_rss = rss_kb();

    // ── Monitor thread ──
    let snapshots: Arc<std::sync::Mutex<Vec<SoakTimeSeriesEntry>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let monitor_handle = {
        let running = Arc::clone(&running);
        let ops = Arc::clone(&ops_completed);
        let errors = Arc::clone(&error_count);
        let latency = Arc::clone(&op_latency);
        let snaps = Arc::clone(&snapshots);
        let db_path_s = db_path_str.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let interval = Duration::from_secs(5);
            while running.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = start.elapsed().as_secs();
                let total_ops = ops.load(Ordering::Relaxed);
                let rps = if elapsed > 0 {
                    total_ops as f64 / elapsed as f64
                } else {
                    0.0
                };
                let snap = latency.snapshot();
                let health = mcp_agent_mail_core::cached_health_level();
                let entry = SoakTimeSeriesEntry {
                    elapsed_secs: elapsed,
                    ops_total: total_ops,
                    actual_rps: rps,
                    p50_us: snap.p50,
                    p95_us: snap.p95,
                    p99_us: snap.p99,
                    max_us: snap.max,
                    errors: errors.load(Ordering::Relaxed),
                    rss_kb: rss_kb(),
                    wal_bytes: wal_size_bytes(&db_path_s),
                    health_level: health.as_str().to_string(),
                };
                eprintln!(
                    "  [{:>4}s] ops={:<7} rps={:<7.1} p50={:<7}μs p95={:<7}μs p99={:<7}μs errs={} rss={}KB wal={:.1}MB health={}",
                    entry.elapsed_secs, entry.ops_total, entry.actual_rps,
                    entry.p50_us, entry.p95_us, entry.p99_us,
                    entry.errors, entry.rss_kb,
                    entry.wal_bytes as f64 / (1024.0 * 1024.0),
                    entry.health_level,
                );
                snaps.lock().unwrap().push(entry);
            }
        })
    };

    // ── Build shared project data for workers ──
    struct SharedProject {
        project_id: i64,
        agent_ids: Vec<i64>,
    }
    let shared_projects: Arc<Vec<SharedProject>> = Arc::new(
        projects
            .into_iter()
            .map(|pc| SharedProject {
                project_id: pc.project_id,
                agent_ids: pc.agent_ids,
            })
            .collect(),
    );

    // ── Worker threads ──
    let start = Instant::now();
    let deadline = Duration::from_secs(duration_secs);

    let handles: Vec<_> = (0..n_workers)
        .map(|worker_id| {
            let pool = pool.clone();
            let limiter = Arc::clone(&rate_limiter);
            let ops = Arc::clone(&ops_completed);
            let errors = Arc::clone(&error_count);
            let latency = Arc::clone(&op_latency);
            let running = Arc::clone(&running);
            let projects = Arc::clone(&shared_projects);
            let pp_ops = Arc::clone(&per_project_ops);

            std::thread::spawn(move || {
                // Per-worker deterministic RNG
                let mut rng = Rng64::new(seed.wrapping_add(worker_id as u64).wrapping_add(1));
                let mut local_ops: u64 = 0;

                while running.load(Ordering::Relaxed) && start.elapsed() < deadline {
                    limiter.wait_for_token();
                    if !running.load(Ordering::Relaxed) || start.elapsed() >= deadline {
                        break;
                    }

                    // Pick project and agent deterministically from seed
                    let proj_idx = rng.next_bounded(projects.len() as u64) as usize;
                    let proj = &projects[proj_idx];
                    let agent_idx = rng.next_bounded(proj.agent_ids.len() as u64) as usize;
                    let agent_id = proj.agent_ids[agent_idx];
                    let receiver_idx = (agent_idx + 1) % proj.agent_ids.len();
                    let receiver_id = proj.agent_ids[receiver_idx];
                    let project_id = proj.project_id;

                    // Deterministic operation selection (same weighted distribution)
                    let op = OpType::from_index(rng.next());
                    let op_start = Instant::now();

                    let result: Result<(), String> = match op {
                        OpType::FetchInbox => {
                            let p = pool.clone();
                            block_on(|cx| async move {
                                match queries::fetch_inbox(
                                    &cx, &p, project_id, agent_id, false, None, 20,
                                )
                                .await
                                {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => Err(format!("fetch_inbox: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::SendMessage => {
                            let p = pool.clone();
                            let subj = format!("soak-w{worker_id}-p{proj_idx}-{local_ops}");
                            let body = format!(
                                "soak body w{worker_id} p{proj_idx} op{local_ops}"
                            );
                            block_on(|cx| async move {
                                match queries::create_message_with_recipients(
                                    &cx, &p, project_id, agent_id,
                                    &subj, &body, None, "normal", false, "",
                                    &[(receiver_id, "to")],
                                )
                                .await
                                {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => Err(format!("send: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Search => {
                            let p = pool.clone();
                            let terms = ["soak", "seed", "body", "project"];
                            let term = terms[rng.next_bounded(terms.len() as u64) as usize];
                            block_on(|cx| async move {
                                match queries::search_messages(&cx, &p, project_id, term, 20).await
                                {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => Err(format!("search: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Reservation => {
                            let p = pool.clone();
                            let path = format!(
                                "src/p{proj_idx}/w{worker_id}/file_{local_ops}.rs"
                            );
                            block_on(|cx| async move {
                                match queries::create_file_reservations(
                                    &cx, &p, project_id, agent_id,
                                    &[path.as_str()], 300, true, "soak test",
                                )
                                .await
                                {
                                    Outcome::Ok(reservations) => {
                                        let ids: Vec<i64> =
                                            reservations.iter().filter_map(|r| r.id).collect();
                                        if !ids.is_empty() {
                                            let _ = queries::release_reservations_by_ids(
                                                &cx, &p, &ids,
                                            )
                                            .await;
                                        }
                                        Ok(())
                                    }
                                    Outcome::Err(e) => Err(format!("reservation: {e:?}")),
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Acknowledge => {
                            let p = pool.clone();
                            block_on(|cx| async move {
                                let _ = queries::mark_message_read(&cx, &p, agent_id, 1).await;
                                match queries::acknowledge_message(&cx, &p, agent_id, 1).await {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => {
                                        let msg = format!("{e:?}");
                                        if msg.contains("NotFound")
                                            || msg.contains("not a recipient")
                                            || msg.contains("no row")
                                        {
                                            Ok(())
                                        } else {
                                            Err(format!("ack: {e:?}"))
                                        }
                                    }
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                    };

                    let op_us =
                        u64::try_from(op_start.elapsed().as_micros()).unwrap_or(u64::MAX);
                    latency.record(op_us);

                    match result {
                        Ok(()) => {
                            ops.fetch_add(1, Ordering::Relaxed);
                            pp_ops[proj_idx].fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("ERROR [w{worker_id}]: {e}");
                            errors.fetch_add(1, Ordering::Relaxed);
                            ops.fetch_add(1, Ordering::Relaxed);
                            pp_ops[proj_idx].fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    local_ops += 1;
                }
            })
        })
        .collect();

    // Wait for workers
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    running.store(false, Ordering::Relaxed);
    monitor_handle.join().expect("monitor thread");

    // ── Collect final metrics ──
    let total_ops = ops_completed.load(Ordering::Relaxed);
    let total_errors = error_count.load(Ordering::Relaxed);
    let elapsed = start.elapsed();
    let active_secs = elapsed.as_secs_f64().min(duration_secs as f64 + 1.0);
    let final_rps = total_ops as f64 / active_secs;
    let final_snap = op_latency.snapshot();
    let final_rss = rss_kb();
    let rss_growth_kb = final_rss.saturating_sub(initial_rss);
    let final_wal = wal_size_bytes(&db_path_str);

    let pp_ops_vec: Vec<u64> = per_project_ops
        .iter()
        .map(|a| a.load(Ordering::Relaxed))
        .collect();

    // ── Compute degradation ──
    let snaps = snapshots.lock().unwrap();
    let degradation_pct = if snaps.len() >= 4 {
        let mid = snaps.len() / 2;
        let interval_rps = |i: usize| -> f64 {
            if i == 0 {
                if snaps[0].elapsed_secs > 0 {
                    snaps[0].ops_total as f64 / snaps[0].elapsed_secs as f64
                } else {
                    0.0
                }
            } else {
                let dt = snaps[i]
                    .elapsed_secs
                    .saturating_sub(snaps[i - 1].elapsed_secs);
                let dops = snaps[i].ops_total.saturating_sub(snaps[i - 1].ops_total);
                if dt > 0 { dops as f64 / dt as f64 } else { 0.0 }
            }
        };

        let first_half_rps: f64 = (0..mid).map(&interval_rps).sum::<f64>() / mid as f64;
        let second_half_rps: f64 =
            (mid..snaps.len()).map(interval_rps).sum::<f64>() / (snaps.len() - mid) as f64;

        if first_half_rps > 10.0 {
            ((first_half_rps - second_half_rps) / first_half_rps * 100.0).max(0.0)
        } else {
            0.0
        }
    } else {
        0.0
    };

    // ── Build threshold results ──
    let thresholds = ThresholdResults {
        p99_budget_us: p99_budget_us,
        p99_actual_us: final_snap.p99,
        p99_pass: final_snap.p99 <= p99_budget_us,
        rss_budget_kb: rss_budget_kb,
        rss_growth_kb,
        rss_pass: rss_growth_kb <= rss_budget_kb,
        wal_budget_bytes: wal_budget_bytes,
        wal_actual_bytes: final_wal,
        wal_pass: final_wal <= wal_budget_bytes,
        errors_pass: total_errors == 0,
        degradation_budget_pct,
        degradation_actual_pct: degradation_pct,
        degradation_pass: degradation_pct < degradation_budget_pct,
    };

    let all_pass = thresholds.p99_pass
        && thresholds.rss_pass
        && thresholds.wal_pass
        && thresholds.errors_pass
        && thresholds.degradation_pass;

    let mut verdict_parts: Vec<&str> = Vec::new();
    if !thresholds.p99_pass {
        verdict_parts.push("P99_EXCEEDED");
    }
    if !thresholds.rss_pass {
        verdict_parts.push("RSS_LEAK");
    }
    if !thresholds.wal_pass {
        verdict_parts.push("WAL_GROWTH");
    }
    if !thresholds.errors_pass {
        verdict_parts.push("ERRORS");
    }
    if !thresholds.degradation_pass {
        verdict_parts.push("DEGRADATION");
    }

    let verdict = if all_pass {
        "PASS".to_string()
    } else {
        format!("FAIL: {}", verdict_parts.join(", "))
    };

    // ── Build and save report ──
    let report = MultiProjectSoakReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        bead: "br-3vwi.9.3",
        seed,
        n_projects,
        agents_per_project,
        target_rps,
        duration_secs,
        actual_duration_secs: elapsed.as_secs_f64(),
        total_ops,
        actual_rps: final_rps,
        errors: total_errors,
        p50_us: final_snap.p50,
        p95_us: final_snap.p95,
        p99_us: final_snap.p99,
        max_us: final_snap.max,
        baseline_rss_kb: initial_rss,
        final_rss_kb: final_rss,
        rss_growth_kb,
        wal_bytes: final_wal,
        per_project_ops: pp_ops_vec.clone(),
        snapshots: snaps.iter().map(|s| SoakTimeSeriesEntry {
            elapsed_secs: s.elapsed_secs,
            ops_total: s.ops_total,
            actual_rps: s.actual_rps,
            p50_us: s.p50_us,
            p95_us: s.p95_us,
            p99_us: s.p99_us,
            max_us: s.max_us,
            errors: s.errors,
            rss_kb: s.rss_kb,
            wal_bytes: s.wal_bytes,
            health_level: s.health_level.clone(),
        }).collect(),
        thresholds: thresholds.clone(),
        verdict: verdict.clone(),
    };
    drop(snaps);

    save_soak_artifact(&report);

    // ── Print summary ──
    eprintln!("\n=== Multi-Project Soak Report (seed={seed}) ===");
    eprintln!("  Projects:     {n_projects} × {agents_per_project} agents = {total_agents} agents");
    eprintln!("  Duration:     {:.1}s", elapsed.as_secs_f64());
    eprintln!("  Total ops:    {total_ops}");
    eprintln!("  Actual RPS:   {final_rps:.1} (target: {target_rps})");
    eprintln!("  Errors:       {total_errors}");
    eprintln!(
        "  Latency:      p50={}μs p95={}μs p99={}μs max={}μs",
        final_snap.p50, final_snap.p95, final_snap.p99, final_snap.max
    );
    eprintln!(
        "  RSS:          {}KB → {}KB (growth: {}KB = {:.1}MB, budget: {:.0}MB)",
        initial_rss, final_rss, rss_growth_kb,
        rss_growth_kb as f64 / 1024.0,
        rss_budget_kb as f64 / 1024.0,
    );
    eprintln!(
        "  WAL:          {:.1}MB (budget: {:.0}MB)",
        final_wal as f64 / (1024.0 * 1024.0),
        wal_budget_bytes as f64 / (1024.0 * 1024.0),
    );
    eprintln!("  Degradation:  {degradation_pct:.1}% (budget: {degradation_budget_pct:.0}%)");

    // Per-project distribution
    if !pp_ops_vec.is_empty() {
        let min_ops = pp_ops_vec.iter().copied().min().unwrap_or(0);
        let max_ops = pp_ops_vec.iter().copied().max().unwrap_or(0);
        let avg_ops = total_ops / n_projects as u64;
        eprintln!(
            "  Per-project:  min={min_ops} avg={avg_ops} max={max_ops} (spread: {:.1}%)",
            if avg_ops > 0 {
                (max_ops - min_ops) as f64 / avg_ops as f64 * 100.0
            } else {
                0.0
            }
        );
    }

    eprintln!("  Verdict:      {verdict}");

    // ── Assertions with actionable diagnostics ──
    assert!(
        thresholds.errors_pass,
        "THRESHOLD FAIL: {total_errors} errors detected.\n\
         Action: Check error logs above for root cause. Common issues:\n\
         - SQLite busy/locked: increase pool size or acquire_timeout_ms\n\
         - Operation-specific failures: check per-op error messages"
    );
    assert!(
        thresholds.p99_pass,
        "THRESHOLD FAIL: P99 latency {}μs ({:.1}ms) exceeds {}μs ({:.0}ms) budget.\n\
         Action: Profile hot-path queries. Check:\n\
         - WAL size (large WAL slows reads)\n\
         - Pool exhaustion (increase max_connections)\n\
         - FTS query complexity (optimize search_messages)",
        final_snap.p99, final_snap.p99 as f64 / 1000.0,
        p99_budget_us, p99_budget_us as f64 / 1000.0,
    );
    assert!(
        thresholds.rss_pass,
        "THRESHOLD FAIL: RSS grew by {}KB ({:.1}MB), exceeds {}KB ({:.0}MB) budget.\n\
         Action: Profile memory allocation. Check:\n\
         - Read cache growth (invalidation frequency)\n\
         - Event ring buffer sizing\n\
         - Per-query allocation patterns",
        rss_growth_kb, rss_growth_kb as f64 / 1024.0,
        rss_budget_kb, rss_budget_kb as f64 / 1024.0,
    );
    assert!(
        thresholds.wal_pass,
        "THRESHOLD FAIL: WAL grew to {}B ({:.1}MB), exceeds {:.0}MB budget.\n\
         Action: Check WAL checkpoint frequency. Consider:\n\
         - PRAGMA wal_autocheckpoint tuning\n\
         - Write batching in deferred touch handler\n\
         - Message volume rate limiting",
        final_wal, final_wal as f64 / (1024.0 * 1024.0),
        wal_budget_bytes as f64 / (1024.0 * 1024.0),
    );
    assert!(
        thresholds.degradation_pass,
        "THRESHOLD FAIL: Throughput degraded {degradation_pct:.1}% (budget: {degradation_budget_pct:.0}%).\n\
         Action: Investigate progressive slowdown. Check:\n\
         - Table scan growth with message count\n\
         - Index effectiveness (EXPLAIN QUERY PLAN)\n\
         - Cache eviction thrashing under multi-project load",
    );

    eprintln!("\n=== PASS: multi-project soak {n_projects}p×{agents_per_project}a at {target_rps} RPS for {duration_secs}s (seed={seed}) ===");
}
