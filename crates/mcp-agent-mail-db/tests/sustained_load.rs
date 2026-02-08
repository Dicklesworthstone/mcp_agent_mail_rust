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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
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
            let elapsed_us =
                u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX);
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
    let db_path = dir
        .path()
        .join(format!("sustained_{}.db", unique_suffix()));
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
                let name = format!(
                    "{}{}",
                    cap(adj[i % adj.len()]),
                    cap(noun[i % noun.len()])
                );
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
                    &format!(
                        "body for sustained load test iteration {i} with searchable keywords"
                    ),
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
                    s.elapsed_secs, s.ops_total, s.actual_rps,
                    s.p50_us, s.p95_us, s.p99_us, s.max_us,
                    s.errors, s.rss_kb, s.wal_bytes, s.health_level,
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
                                    Outcome::Err(e) => {
                                        Err(format!("fetch_inbox: {e:?}"))
                                    }
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::SendMessage => {
                            let p = pool.clone();
                            let subj =
                                format!("load w{worker_id} op{local_ops}");
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
                                    Outcome::Err(e) => {
                                        Err(format!("send: {e:?}"))
                                    }
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
                                match queries::search_messages(
                                    &cx, &p, project_id, term, 20,
                                )
                                .await
                                {
                                    Outcome::Ok(_) => Ok(()),
                                    Outcome::Err(e) => {
                                        Err(format!("search: {e:?}"))
                                    }
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Reservation => {
                            let p = pool.clone();
                            let path = format!(
                                "src/worker_{worker_id}/file_{local_ops}.rs"
                            );
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
                                        let ids: Vec<i64> = reservations
                                            .iter()
                                            .filter_map(|r| r.id)
                                            .collect();
                                        if !ids.is_empty() {
                                            let _ =
                                                queries::release_reservations_by_ids(
                                                    &cx, &p, &ids,
                                                )
                                                .await;
                                        }
                                        Ok(())
                                    }
                                    Outcome::Err(e) => {
                                        Err(format!("reservation: {e:?}"))
                                    }
                                    _ => Err("cancelled".into()),
                                }
                            })
                        }
                        OpType::Acknowledge => {
                            if ackables.is_empty() {
                                Ok(())
                            } else {
                                let msg_idx =
                                    (local_ops as usize) % ackables.len();
                                let msg_id = ackables[msg_idx];
                                let p = pool.clone();
                                block_on(|cx| async move {
                                    // Both ops are idempotent; NotFound for
                                    // non-recipients is expected.
                                    let _ = queries::mark_message_read(
                                        &cx, &p, agent_id, msg_id,
                                    )
                                    .await;
                                    match queries::acknowledge_message(
                                        &cx, &p, agent_id, msg_id,
                                    )
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

                    let op_us = u64::try_from(op_start.elapsed().as_micros())
                        .unwrap_or(u64::MAX);
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
    eprintln!(
        "Actual RPS: {final_rps:.1} (target: {target_rps})"
    );
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
    eprintln!("WAL size:   {}B ({:.1}MB)", final_wal, final_wal as f64 / (1024.0 * 1024.0));

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
                let dt = snaps[i].elapsed_secs.saturating_sub(snaps[i - 1].elapsed_secs);
                let dops = snaps[i].ops_total.saturating_sub(snaps[i - 1].ops_total);
                if dt > 0 {
                    dops as f64 / dt as f64
                } else {
                    0.0
                }
            }
        };

        let first_half_rps: f64 =
            (0..mid).map(&interval_rps).sum::<f64>() / mid as f64;
        let second_half_rps: f64 =
            (mid..snaps.len()).map(interval_rps).sum::<f64>()
                / (snaps.len() - mid) as f64;

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
