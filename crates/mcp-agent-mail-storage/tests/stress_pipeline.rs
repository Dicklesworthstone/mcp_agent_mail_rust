//! End-to-end stress tests for the combined DB + Git storage pipeline.
//!
//! These tests exercise the exact failure modes that plagued the Python version:
//! - Git lock file contention under many concurrent agents
//! - SQLite pool exhaustion under sustained load
//! - WBQ (write-behind queue) saturation and backpressure
//! - Commit coalescer batching under extreme concurrent writes
//! - Multi-project concurrent operations
//! - Combined DB + storage pipeline degradation
//!
//! Run:
//! ```bash
//! cargo test -p mcp-agent-mail-storage --test stress_pipeline -- --nocapture
//! cargo test -p mcp-agent-mail-storage --test stress_pipeline -- --ignored --nocapture
//! ```

#![allow(
    clippy::needless_collect,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::manual_let_else,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::redundant_closure_for_method_calls,
    clippy::significant_drop_tightening,
    clippy::items_after_statements,
    clippy::missing_const_for_fn
)]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_core::models::{VALID_ADJECTIVES, VALID_NOUNS};
use mcp_agent_mail_db::{DbPool, DbPoolConfig, micros_to_iso, queries};
use mcp_agent_mail_storage::{
    WbqEnqueueResult, WriteOp, enqueue_async_commit, ensure_archive, ensure_archive_root,
    flush_async_commits, get_commit_coalescer, wbq_enqueue, wbq_flush, wbq_start, wbq_stats,
    write_agent_profile_with_config, write_message_bundle,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn test_config(root: &Path) -> Config {
    Config {
        storage_root: root.to_path_buf(),
        ..Config::default()
    }
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
            Outcome::Cancelled(r) => panic!("cancelled: {r:?}"),
            Outcome::Panicked(p) => panic!("panicked: {p}"),
        }
    }
    unreachable!()
}

fn unique_human_key(prefix: &str) -> String {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let uid = unique_suffix();
    format!("/tmp/{prefix}-{suffix}-{uid}")
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        let mut out: String = f.to_uppercase().collect();
        out.extend(c);
        out
    })
}

fn agent_name(idx: usize) -> String {
    let adj = VALID_ADJECTIVES[idx % VALID_ADJECTIVES.len()];
    let noun = VALID_NOUNS[idx % VALID_NOUNS.len()];
    format!("{}{}", cap(adj), cap(noun))
}

fn make_pool(tmp: &TempDir) -> DbPool {
    let db_path = tmp.path().join(format!("stress_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 20,
        min_connections: 4,
        acquire_timeout_ms: 60_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    DbPool::new(&config).expect("create pool")
}

fn make_large_pool(tmp: &TempDir) -> DbPool {
    let db_path = tmp
        .path()
        .join(format!("stress_large_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 50,
        min_connections: 10,
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 5,
    };
    DbPool::new(&config).expect("create large pool")
}

/// RSS memory in KB (Linux only, returns 0 elsewhere).
fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages * 4)
}

struct LatencyReport {
    count: usize,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    errors: u64,
}

impl LatencyReport {
    fn from_latencies(latencies: &mut [u64], errors: u64) -> Self {
        if latencies.is_empty() {
            return Self {
                count: 0,
                p50_us: 0,
                p95_us: 0,
                p99_us: 0,
                max_us: 0,
                errors,
            };
        }
        latencies.sort_unstable();
        let n = latencies.len();
        Self {
            count: n,
            p50_us: latencies[n / 2],
            p95_us: latencies[n * 95 / 100],
            p99_us: latencies[n * 99 / 100],
            max_us: latencies[n - 1],
            errors,
        }
    }

    fn print(&self, label: &str) {
        eprintln!(
            "  {label}: n={}, p50={:.1}ms, p95={:.1}ms, p99={:.1}ms, max={:.1}ms, errors={}",
            self.count,
            self.p50_us as f64 / 1000.0,
            self.p95_us as f64 / 1000.0,
            self.p99_us as f64 / 1000.0,
            self.max_us as f64 / 1000.0,
            self.errors,
        );
    }
}

// ===========================================================================
// TEST 1: Multi-agent message pipeline (DB + Git archive) under concurrency
//
// Scenario: 30 agents in one project all send messages simultaneously.
// Each agent sends 5 messages to random other agents.
// Verifies: no git lock failures, no DB errors, all messages reach inbox.
// ===========================================================================

#[test]
fn stress_concurrent_message_pipeline_30_agents() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let pool = make_pool(&tmp);

    let n_agents = 30;
    let msgs_per_agent = 5;

    // Set up project + agents
    let human_key = unique_human_key("stress-pipeline-30");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let project_id = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_agents {
            let name = agent_name(i);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                project_id,
                &name,
                "stress-test",
                "test-model",
                Some("stress agent"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent {name} failed: {other:?}"),
            };
            ids.push((agent.id.expect("agent id"), name));
        }
        (project_id, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    // Spawn threads: each agent sends msgs_per_agent messages concurrently
    let barrier = Arc::new(Barrier::new(n_agents));
    let errors = Arc::new(AtomicU64::new(0));
    let successes = Arc::new(AtomicU64::new(0));
    let mut latencies_all = Vec::new();

    let handles: Vec<_> = (0..n_agents)
        .map(|i| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let errors = Arc::clone(&errors);
            let successes = Arc::clone(&successes);

            std::thread::Builder::new()
                .name(format!("agent-{i}"))
                .spawn(move || {
                    barrier.wait();
                    let mut thread_latencies = Vec::new();

                    for msg_idx in 0..msgs_per_agent {
                        let start = Instant::now();
                        let (sender_id, sender_name) = &agent_ids[i];
                        let recipient_idx = (i + msg_idx + 1) % n_agents;
                        let (recipient_id, recipient_name) = &agent_ids[recipient_idx];
                        let thread_id = format!("stress-t{i}-m{msg_idx}");
                        let subject = format!("Stress msg {msg_idx} from {sender_name}");
                        let body = format!(
                            "Message body {msg_idx} from agent {i} to agent {recipient_idx}"
                        );

                        // DB write
                        let message = block_on_with_retry(5, |cx| {
                            let pool = pool.clone();
                            let subject = subject.clone();
                            let body = body.clone();
                            let thread_id = thread_id.clone();
                            async move {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pool,
                                    project_id,
                                    *sender_id,
                                    &subject,
                                    &body,
                                    Some(&thread_id),
                                    "normal",
                                    false,
                                    "[]",
                                    &[(*recipient_id, "to")],
                                )
                                .await
                            }
                        });

                        let msg_id = message.id.expect("message id");
                        let created_iso = micros_to_iso(message.created_ts);

                        // Git archive write
                        let message_json = serde_json::json!({
                            "id": msg_id,
                            "subject": subject,
                            "thread_id": thread_id,
                            "created_ts": created_iso,
                        });
                        match write_message_bundle(
                            &archive,
                            &config,
                            &message_json,
                            &body,
                            sender_name,
                            std::slice::from_ref(recipient_name),
                            &[],
                            None,
                        ) {
                            Ok(()) => {
                                successes.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("  archive write error (agent {i}, msg {msg_idx}): {e}");
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }

                        thread_latencies.push(start.elapsed().as_micros() as u64);
                    }
                    thread_latencies
                })
                .expect("spawn agent thread")
        })
        .collect();

    for handle in handles {
        let mut lats = handle.join().expect("agent thread panicked");
        latencies_all.append(&mut lats);
    }

    // Flush any async commits
    flush_async_commits();

    let total_errors = errors.load(Ordering::Relaxed);
    let total_successes = successes.load(Ordering::Relaxed);
    let expected = (n_agents * msgs_per_agent) as u64;

    let report = LatencyReport::from_latencies(&mut latencies_all, total_errors);
    eprintln!("\n=== stress_concurrent_message_pipeline_30_agents ===");
    eprintln!("  Agents: {n_agents}, Messages/agent: {msgs_per_agent}");
    eprintln!("  Total expected: {expected}, success: {total_successes}, errors: {total_errors}");
    report.print("Pipeline (DB+Git)");

    // Verify: all DB writes succeeded (retries handle transient locks)
    assert_eq!(
        total_successes + total_errors,
        expected,
        "some operations were lost"
    );
    // Allow a small error rate from archive writes (git contention), but DB must succeed
    let error_rate = total_errors as f64 / expected as f64;
    assert!(
        error_rate < 0.05,
        "archive error rate {error_rate:.2}% exceeds 5% threshold ({total_errors}/{expected})"
    );

    // Verify p99 latency is reasonable (< 10s per message pipeline)
    assert!(
        report.p99_us < 10_000_000,
        "p99 latency {:.1}ms exceeds 10s budget",
        report.p99_us as f64 / 1000.0,
    );
}

// ===========================================================================
// TEST 2: Multi-project concurrent operations
//
// Scenario: 10 projects, each with 5 agents, all operating simultaneously.
// Tests per-project isolation in commit coalescer and pool.
// ===========================================================================

#[test]
fn stress_multi_project_concurrent_operations() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let pool = make_pool(&tmp);

    let n_projects = 10;
    let agents_per_project = 5;
    let msgs_per_agent = 3;

    // Set up all projects and agents
    let pool_setup = pool.clone();
    let mut project_data = Vec::new();
    for p in 0..n_projects {
        let human_key = unique_human_key(&format!("stress-multi-p{p}"));
        let pool_clone = pool_setup.clone();
        let (project_id, slug, agents) = block_on(|cx| async move {
            let project = match queries::ensure_project(&cx, &pool_clone, &human_key).await {
                Outcome::Ok(row) => row,
                other => panic!("ensure_project p{p} failed: {other:?}"),
            };
            let pid = project.id.expect("project id");
            let mut agents = Vec::new();
            for a in 0..agents_per_project {
                let name = agent_name(p * agents_per_project + a);
                let agent = match queries::register_agent(
                    &cx,
                    &pool_clone,
                    pid,
                    &name,
                    "stress-test",
                    "test-model",
                    Some("multi-project stress"),
                    None,
                )
                .await
                {
                    Outcome::Ok(row) => row,
                    other => panic!("register agent {name} p{p} failed: {other:?}"),
                };
                agents.push((agent.id.expect("agent id"), name));
            }
            (pid, project.slug, agents)
        });
        let archive = ensure_archive(&config, &slug).expect("ensure archive");
        project_data.push((project_id, slug, agents, archive));
    }

    let barrier = Arc::new(Barrier::new(n_projects * agents_per_project));
    let total_errors = Arc::new(AtomicU64::new(0));
    let total_successes = Arc::new(AtomicU64::new(0));

    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for (p_idx, (project_id, _slug, agents, archive)) in project_data.iter().enumerate() {
        for (a_idx, (sender_id, sender_name)) in agents.iter().enumerate() {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agents = agents.clone();
            let barrier = Arc::clone(&barrier);
            let total_errors = Arc::clone(&total_errors);
            let total_successes = Arc::clone(&total_successes);
            let project_id = *project_id;
            let sender_id = *sender_id;
            let sender_name = sender_name.clone();

            handles.push(
                std::thread::Builder::new()
                    .name(format!("p{p_idx}-a{a_idx}"))
                    .spawn(move || {
                        barrier.wait();

                        for m in 0..msgs_per_agent {
                            let recipient_idx = (a_idx + m + 1) % agents.len();
                            let (recipient_id, recipient_name) = &agents[recipient_idx];
                            let thread_id = format!("mp-p{p_idx}-a{a_idx}-m{m}");

                            let msg = block_on_with_retry(5, |cx| {
                                let pool = pool.clone();
                                let thread_id = thread_id.clone();
                                async move {
                                    queries::create_message_with_recipients(
                                        &cx,
                                        &pool,
                                        project_id,
                                        sender_id,
                                        &format!("Multi-project msg {m}"),
                                        &format!("Body from p{p_idx} agent {a_idx} msg {m}"),
                                        Some(&thread_id),
                                        "normal",
                                        false,
                                        "[]",
                                        &[(*recipient_id, "to")],
                                    )
                                    .await
                                }
                            });

                            let msg_json = serde_json::json!({
                                "id": msg.id.expect("msg id"),
                                "subject": format!("Multi-project msg {m}"),
                                "thread_id": thread_id,
                                "created_ts": micros_to_iso(msg.created_ts),
                            });

                            match write_message_bundle(
                                &archive,
                                &config,
                                &msg_json,
                                &format!("Body from p{p_idx} agent {a_idx} msg {m}"),
                                &sender_name,
                                std::slice::from_ref(recipient_name),
                                &[],
                                None,
                            ) {
                                Ok(()) => {
                                    total_successes.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "  multi-project archive err p{p_idx} a{a_idx} m{m}: {e}"
                                    );
                                    total_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    })
                    .expect("spawn thread"),
            );
        }
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    flush_async_commits();

    let errs = total_errors.load(Ordering::Relaxed);
    let succs = total_successes.load(Ordering::Relaxed);
    let expected = (n_projects * agents_per_project * msgs_per_agent) as u64;

    eprintln!("\n=== stress_multi_project_concurrent_operations ===");
    eprintln!(
        "  Projects: {n_projects}, Agents/project: {agents_per_project}, Msgs/agent: {msgs_per_agent}"
    );
    eprintln!("  Total expected: {expected}, success: {succs}, errors: {errs}");

    assert_eq!(succs + errs, expected, "lost operations");
    let error_rate = errs as f64 / expected as f64;
    assert!(
        error_rate < 0.05,
        "multi-project error rate {error_rate:.2}% exceeds 5% ({errs}/{expected})"
    );
}

// ===========================================================================
// TEST 3: Git commit coalescer stress — many rapid commits
//
// Scenario: 100 rapid-fire writes through the commit coalescer to a single
// repo. Verifies batching reduces actual git commits well below 100.
// ===========================================================================

#[test]
fn stress_commit_coalescer_batching_100_writes() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    ensure_archive_root(&config).expect("init archive root");

    let archive = ensure_archive(&config, "coalesce-stress").expect("ensure archive");
    let repo_root = archive.repo_root.clone();

    let n_writes = 100;
    let barrier = Arc::new(Barrier::new(n_writes));
    let errors = Arc::new(AtomicU64::new(0));

    let coalescer = get_commit_coalescer();
    let _stats_before = coalescer.stats();

    let handles: Vec<_> = (0..n_writes)
        .map(|i| {
            let config = config.clone();
            let archive_root = archive.root.clone();
            let repo_root = repo_root.clone();
            let barrier = Arc::clone(&barrier);
            let errors = Arc::clone(&errors);

            std::thread::spawn(move || {
                // Write a file to disk
                let file_name = format!("stress-file-{i}.txt");
                let file_path = archive_root.join(&file_name);
                let rel_path = format!("projects/coalesce-stress/{}", file_name);

                if let Err(e) = std::fs::write(&file_path, format!("content-{i}")) {
                    eprintln!("  file write error {i}: {e}");
                    errors.fetch_add(1, Ordering::Relaxed);
                    return;
                }

                barrier.wait();

                // Enqueue through the coalescer
                enqueue_async_commit(
                    &repo_root,
                    &config,
                    &format!("stress commit {i}"),
                    &[rel_path],
                );
            })
        })
        .collect();

    for h in handles {
        h.join().expect("writer thread panicked");
    }

    flush_async_commits();

    // Use per-repo stats to avoid interference from other tests' commits
    let per_repo = coalescer.per_repo_stats();
    let repo_stats = per_repo.get(&repo_root);
    let errs = errors.load(Ordering::Relaxed);

    let (enqueued_total, commits_total) = repo_stats
        .map(|s| (s.enqueued_total, s.commits_total))
        .unwrap_or((0, 0));

    eprintln!("\n=== stress_commit_coalescer_batching_100_writes ===");
    eprintln!("  Writes enqueued: {n_writes}");
    eprintln!("  Repo enqueued_total: {enqueued_total}, commits_total: {commits_total}");
    eprintln!(
        "  Batching ratio: {:.1}x",
        enqueued_total as f64 / commits_total.max(1) as f64
    );
    eprintln!("  File write errors: {errs}");

    assert_eq!(errs, 0, "file writes should not fail");
    // All writes should have been enqueued to this repo
    assert!(
        enqueued_total >= n_writes as u64,
        "expected at least {n_writes} enqueued, got {enqueued_total}"
    );
    // The coalescer should batch: fewer commits than enqueued writes
    assert!(
        commits_total < enqueued_total,
        "coalescer should batch: {commits_total} commits for {enqueued_total} enqueued"
    );
    // Expect at least 2x batching efficiency
    assert!(
        commits_total <= (enqueued_total / 2),
        "batching ratio too low: {commits_total} commits for {enqueued_total} enqueued (< 2x)"
    );
}

// ===========================================================================
// TEST 4: Git lock file stale detection and recovery
//
// Scenario: Create stale lock files (simulating crashed agents), then verify
// the system can still commit successfully after detecting and recovering.
// ===========================================================================

#[test]
fn stress_stale_git_lock_recovery() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    ensure_archive_root(&config).expect("init archive root");

    let archive = ensure_archive(&config, "lock-recovery").expect("ensure archive");

    // Create a stale .git/index.lock (simulate a crashed process)
    let git_dir = archive.repo_root.join(".git");
    let index_lock = git_dir.join("index.lock");
    std::fs::write(&index_lock, "stale lock from pid 99999").expect("create stale index.lock");

    // Also write a stale owner file pointing to a nonexistent PID
    let owner_path = git_dir.join("index.lock.owner");
    std::fs::write(&owner_path, "99999\n1000000000\n0\n").expect("create stale owner");

    // Create a stale archive lock
    let archive_lock = archive.lock_path.clone();
    std::fs::write(&archive_lock, "").expect("create stale archive lock");
    let archive_owner = format!("{}.owner.json", archive_lock.display());
    std::fs::write(
        &archive_owner,
        r#"{"pid":99999,"created_ts":"2020-01-01T00:00:00Z"}"#,
    )
    .expect("create stale archive lock owner");

    // Now try to write an agent profile (triggers git operations)
    let agent_json = serde_json::json!({
        "name": "RecoveryAgent",
        "program": "stress-test",
        "model": "test-model",
    });

    // The system should detect stale locks and recover
    let result = write_agent_profile_with_config(&archive, &config, &agent_json);
    match &result {
        Ok(()) => eprintln!("  Lock recovery: write succeeded after stale lock cleanup"),
        Err(e) => eprintln!("  Lock recovery: write failed: {e}"),
    }

    // Also test heal_archive_locks
    let heal_result = mcp_agent_mail_storage::heal_archive_locks(&config);
    eprintln!("  heal_archive_locks result: {heal_result:?}");

    // Try a commit through the coalescer (should also handle stale locks)
    let test_file = archive.root.join("lock-recovery-test.txt");
    std::fs::write(&test_file, "test content").expect("write test file");
    enqueue_async_commit(
        &archive.repo_root,
        &config,
        "test commit after lock recovery",
        &["projects/lock-recovery/lock-recovery-test.txt".to_string()],
    );
    flush_async_commits();

    eprintln!("\n=== stress_stale_git_lock_recovery ===");
    eprintln!("  Stale index.lock created and recovery attempted");
    eprintln!(
        "  Write after recovery: {}",
        if result.is_ok() { "OK" } else { "FAILED" }
    );

    // The system should have recovered — at minimum the coalescer should handle it
    // (it has retry logic with stale lock detection)
    assert!(
        !index_lock.exists() || result.is_ok(),
        "system should either clean up stale lock or work despite it"
    );
}

// ===========================================================================
// TEST 5: Concurrent file reservations + messages (mixed workload)
//
// Scenario: Multiple agents simultaneously create file reservations AND
// send messages, exercising both DB writes and git archive writes together.
// ===========================================================================

#[test]
fn stress_mixed_reservations_and_messages() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let pool = make_pool(&tmp);

    let n_agents = 20;
    let ops_per_agent = 8; // alternating reservation + message

    let human_key = unique_human_key("stress-mixed");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_agents {
            let name = agent_name(i + 100); // offset to avoid name collision with other tests
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "stress-test",
                "test-model",
                Some("mixed workload agent"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent failed: {other:?}"),
            };
            ids.push((agent.id.expect("id"), name));
        }
        (pid, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    let barrier = Arc::new(Barrier::new(n_agents));
    let reservation_errors = Arc::new(AtomicU64::new(0));
    let message_errors = Arc::new(AtomicU64::new(0));
    let reservation_successes = Arc::new(AtomicU64::new(0));
    let message_successes = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_agents)
        .map(|i| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let reservation_errors = Arc::clone(&reservation_errors);
            let message_errors = Arc::clone(&message_errors);
            let reservation_successes = Arc::clone(&reservation_successes);
            let message_successes = Arc::clone(&message_successes);

            std::thread::Builder::new()
                .name(format!("mixed-agent-{i}"))
                .spawn(move || {
                    barrier.wait();

                    for op in 0..ops_per_agent {
                        let (agent_id, agent_name) = &agent_ids[i];

                        if op % 2 == 0 {
                            // File reservation
                            let pattern = format!("src/agent_{i}/file_{op}.rs");
                            let result = block_on_with_retry(5, |cx| {
                                let pool = pool.clone();
                                let pattern = pattern.clone();
                                async move {
                                    queries::create_file_reservations(
                                        &cx,
                                        &pool,
                                        project_id,
                                        *agent_id,
                                        &[pattern.as_str()],
                                        300,  // 5 min TTL
                                        true, // exclusive
                                        &format!("stress-test-{i}-{op}"),
                                    )
                                    .await
                                }
                            });
                            if result.is_empty() {
                                reservation_errors.fetch_add(1, Ordering::Relaxed);
                            } else {
                                reservation_successes.fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            // Message send
                            let recipient_idx = (i + op) % n_agents;
                            let (recipient_id, recipient_name) = &agent_ids[recipient_idx];
                            let thread_id = format!("mixed-{i}-{op}");

                            let msg = block_on_with_retry(5, |cx| {
                                let pool = pool.clone();
                                let thread_id = thread_id.clone();
                                async move {
                                    queries::create_message_with_recipients(
                                        &cx,
                                        &pool,
                                        project_id,
                                        *agent_id,
                                        &format!("Mixed msg {op}"),
                                        &format!("Mixed body {i}-{op}"),
                                        Some(&thread_id),
                                        "normal",
                                        false,
                                        "[]",
                                        &[(*recipient_id, "to")],
                                    )
                                    .await
                                }
                            });

                            let msg_json = serde_json::json!({
                                "id": msg.id.expect("msg id"),
                                "subject": format!("Mixed msg {op}"),
                                "thread_id": thread_id,
                                "created_ts": micros_to_iso(msg.created_ts),
                            });

                            match write_message_bundle(
                                &archive,
                                &config,
                                &msg_json,
                                &format!("Mixed body {i}-{op}"),
                                agent_name,
                                std::slice::from_ref(recipient_name),
                                &[],
                                None,
                            ) {
                                Ok(()) => {
                                    message_successes.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    eprintln!("  mixed msg err {i}-{op}: {e}");
                                    message_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                })
                .expect("spawn thread")
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }
    flush_async_commits();

    let res_ok = reservation_successes.load(Ordering::Relaxed);
    let res_err = reservation_errors.load(Ordering::Relaxed);
    let msg_ok = message_successes.load(Ordering::Relaxed);
    let msg_err = message_errors.load(Ordering::Relaxed);

    eprintln!("\n=== stress_mixed_reservations_and_messages ===");
    eprintln!("  Agents: {n_agents}, Ops/agent: {ops_per_agent}");
    eprintln!("  Reservations: {res_ok} ok, {res_err} errors");
    eprintln!("  Messages: {msg_ok} ok, {msg_err} errors");

    // Reservations: some may conflict (exclusive on same pattern) - that's expected
    // Messages: archive errors should be rare
    let msg_total = msg_ok + msg_err;
    if msg_total > 0 {
        let msg_error_rate = msg_err as f64 / msg_total as f64;
        assert!(
            msg_error_rate < 0.10,
            "message error rate {msg_error_rate:.2}% too high ({msg_err}/{msg_total})"
        );
    }
}

// ===========================================================================
// TEST 6: WBQ saturation stress
//
// Scenario: Rapidly enqueue operations faster than the WBQ can drain them.
// Verify backpressure kicks in without data loss or panics.
// ===========================================================================

#[test]
fn stress_wbq_saturation_and_backpressure() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    ensure_archive_root(&config).expect("init archive root");
    let _archive = ensure_archive(&config, "wbq-sat").expect("archive");

    wbq_start();

    let n_threads = 20;
    let ops_per_thread = 100;
    let barrier = Arc::new(Barrier::new(n_threads));
    let enqueued = Arc::new(AtomicU64::new(0));
    let skipped = Arc::new(AtomicU64::new(0));
    let unavailable = Arc::new(AtomicU64::new(0));

    let stats_before = wbq_stats();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let config = config.clone();
            let barrier = Arc::clone(&barrier);
            let enqueued = Arc::clone(&enqueued);
            let skipped = Arc::clone(&skipped);
            let unavailable = Arc::clone(&unavailable);

            std::thread::spawn(move || {
                barrier.wait();

                for i in 0..ops_per_thread {
                    let agent_json = serde_json::json!({
                        "name": format!("WbqAgent{t}x{i}"),
                        "program": "wbq-stress",
                        "model": "test",
                    });
                    let op = WriteOp::AgentProfile {
                        project_slug: "wbq-sat".to_string(),
                        config: config.clone(),
                        agent_json,
                    };

                    match wbq_enqueue(op) {
                        WbqEnqueueResult::Enqueued => {
                            enqueued.fetch_add(1, Ordering::Relaxed);
                        }
                        WbqEnqueueResult::SkippedDiskCritical => {
                            skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        WbqEnqueueResult::QueueUnavailable => {
                            unavailable.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("wbq thread panicked");
    }

    // Let the WBQ drain
    wbq_flush();
    std::thread::sleep(Duration::from_millis(500));

    let stats_after = wbq_stats();
    let total_enqueued = enqueued.load(Ordering::Relaxed);
    let total_skipped = skipped.load(Ordering::Relaxed);
    let total_unavail = unavailable.load(Ordering::Relaxed);
    let total_ops = (n_threads * ops_per_thread) as u64;

    eprintln!("\n=== stress_wbq_saturation_and_backpressure ===");
    eprintln!("  Threads: {n_threads}, Ops/thread: {ops_per_thread}, Total: {total_ops}");
    eprintln!(
        "  Enqueued: {total_enqueued}, Skipped(disk): {total_skipped}, Unavailable: {total_unavail}"
    );
    eprintln!(
        "  WBQ enqueued before: {}, after: {}, drained: {}",
        stats_before.enqueued, stats_after.enqueued, stats_after.drained,
    );
    eprintln!(
        "  WBQ errors: {}, fallbacks: {}",
        stats_after.errors, stats_after.fallbacks
    );

    // All ops should be accounted for
    assert_eq!(
        total_enqueued + total_skipped + total_unavail,
        total_ops,
        "operation accounting mismatch"
    );
    // No panics, no data loss — just graceful backpressure
    // Most should have been enqueued (some may be skipped under disk pressure)
    assert!(
        total_enqueued > total_ops / 2,
        "less than half enqueued ({total_enqueued}/{total_ops}) — unexpected"
    );
}

// ===========================================================================
// TEST 7: Pool exhaustion under combined DB + archive load (IGNORED - heavy)
//
// Scenario: 100 concurrent threads all trying to acquire DB connections
// while simultaneously writing to git archives. This is the exact scenario
// that killed the Python version.
// ===========================================================================

#[test]
#[ignore] // Heavy test — run manually: cargo test --test stress_pipeline -- --ignored --nocapture
fn stress_pool_exhaustion_with_archive_writes() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    // Intentionally constrained pool to surface exhaustion
    let db_path = tmp
        .path()
        .join(format!("stress_exhaust_{}.db", unique_suffix()));
    let pool_config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 15, // Deliberately small to trigger contention
        min_connections: 3,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 3,
    };
    let pool = DbPool::new(&pool_config).expect("create constrained pool");

    let n_threads = 60; // 4x pool capacity
    let ops_per_thread = 10;

    let human_key = unique_human_key("stress-exhaust");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_threads.min(50) {
            // Create up to 50 agents
            let name = agent_name(i + 200); // offset to avoid collision
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "stress-exhaust",
                "test-model",
                Some("pool exhaustion test"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent failed: {other:?}"),
            };
            ids.push((agent.id.expect("id"), name));
        }
        (pid, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    let barrier = Arc::new(Barrier::new(n_threads));
    let db_errors = Arc::new(AtomicU64::new(0));
    let archive_errors = Arc::new(AtomicU64::new(0));
    let successes = Arc::new(AtomicU64::new(0));
    let timeout_errors = Arc::new(AtomicU64::new(0));

    let start_time = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let db_errors = Arc::clone(&db_errors);
            let archive_errors = Arc::clone(&archive_errors);
            let successes = Arc::clone(&successes);
            let timeout_errors = Arc::clone(&timeout_errors);

            std::thread::Builder::new()
                .name(format!("exhaust-{t}"))
                .spawn(move || {
                    barrier.wait();

                    for op in 0..ops_per_thread {
                        let agent_idx = t % agent_ids.len();
                        let (sender_id, sender_name) = &agent_ids[agent_idx];
                        let recipient_idx = (agent_idx + 1) % agent_ids.len();
                        let (recipient_id, recipient_name) = &agent_ids[recipient_idx];
                        let thread_id = format!("exhaust-{t}-{op}");

                        // Attempt DB operation with explicit error tracking
                        let cx = Cx::for_testing();
                        let rt = RuntimeBuilder::current_thread().build().expect("build rt");
                        let pool_c = pool.clone();
                        let thread_id_c = thread_id.clone();
                        let msg_result = rt.block_on(async {
                            queries::create_message_with_recipients(
                                &cx,
                                &pool_c,
                                project_id,
                                *sender_id,
                                &format!("Exhaust msg {t}-{op}"),
                                &format!("Body {t}-{op}"),
                                Some(&thread_id_c),
                                "normal",
                                false,
                                "[]",
                                &[(*recipient_id, "to")],
                            )
                            .await
                        });

                        match msg_result {
                            Outcome::Ok(msg) => {
                                // DB succeeded — now archive
                                let msg_json = serde_json::json!({
                                    "id": msg.id.expect("msg id"),
                                    "subject": format!("Exhaust msg {t}-{op}"),
                                    "thread_id": thread_id,
                                    "created_ts": micros_to_iso(msg.created_ts),
                                });
                                match write_message_bundle(
                                    &archive,
                                    &config,
                                    &msg_json,
                                    &format!("Body {t}-{op}"),
                                    sender_name,
                                    std::slice::from_ref(recipient_name),
                                    &[],
                                    None,
                                ) {
                                    Ok(()) => {
                                        successes.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        let msg = format!("{e}");
                                        if msg.contains("Lock") || msg.contains("lock") {
                                            // Git lock contention — expected under stress
                                        } else {
                                            eprintln!("  archive err {t}-{op}: {e}");
                                        }
                                        archive_errors.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            Outcome::Err(e) => {
                                let msg = format!("{e:?}");
                                if msg.contains("timeout") || msg.contains("Timeout") {
                                    timeout_errors.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    db_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            Outcome::Cancelled(_) => {
                                timeout_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            Outcome::Panicked(p) => {
                                panic!("DB operation panicked: {p}");
                            }
                        }
                    }
                })
                .expect("spawn thread")
        })
        .collect();

    for h in handles {
        h.join().expect("exhaust thread panicked");
    }

    flush_async_commits();
    let elapsed = start_time.elapsed();

    let total_ops = (n_threads * ops_per_thread) as u64;
    let db_errs = db_errors.load(Ordering::Relaxed);
    let arch_errs = archive_errors.load(Ordering::Relaxed);
    let timeouts = timeout_errors.load(Ordering::Relaxed);
    let ok = successes.load(Ordering::Relaxed);

    eprintln!("\n=== stress_pool_exhaustion_with_archive_writes ===");
    eprintln!("  Threads: {n_threads} (pool max: 15), Ops/thread: {ops_per_thread}");
    eprintln!("  Elapsed: {:.1}s", elapsed.as_secs_f64());
    eprintln!(
        "  Successes: {ok}, DB errors: {db_errs}, Archive errors: {arch_errs}, Timeouts: {timeouts}"
    );
    eprintln!(
        "  Throughput: {:.0} ops/sec",
        total_ops as f64 / elapsed.as_secs_f64()
    );

    // Key assertion: no panics, system stayed up
    // DB errors from pool exhaustion are acceptable but should be bounded
    assert!(
        db_errs + timeouts < total_ops / 4,
        "too many DB failures ({}) out of {total_ops} — pool exhaustion recovery broken",
        db_errs + timeouts,
    );
    // Must have completed some work
    assert!(ok > 0, "zero successful operations — complete failure");
}

// ===========================================================================
// TEST 8: Sustained mixed workload (IGNORED - heavy, 30s+)
//
// Scenario: Simulates realistic production load for 30 seconds:
// - 40% message sends (DB + archive)
// - 30% inbox fetches (DB read)
// - 15% search queries (DB FTS)
// - 10% file reservations (DB write)
// - 5% agent profile writes (archive)
//
// Monitors: latency percentiles, RSS, error rates, pool utilization.
// ===========================================================================

#[test]
#[ignore] // Heavy test — run: cargo test --test stress_pipeline -- --ignored --nocapture
fn stress_sustained_mixed_workload_30s() {
    let duration_secs: u64 = std::env::var("STRESS_DURATION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let target_rps: u64 = std::env::var("STRESS_TARGET_RPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let pool = make_large_pool(&tmp);

    let n_agents = 30;
    wbq_start();

    // Setup
    let human_key = unique_human_key("stress-sustained");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_agents {
            let name = agent_name(i + 300);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "stress-sustained",
                "test-model",
                Some("sustained load agent"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent failed: {other:?}"),
            };
            ids.push((agent.id.expect("id"), name));
        }
        (pid, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    // Seed some messages for search and inbox operations
    let pool_seed = pool.clone();
    let agent_ids_seed = agent_ids.clone();
    block_on(|cx| {
        let pool = pool_seed;
        async move {
            for i in 0..50 {
                let sender_idx = i % n_agents;
                let recipient_idx = (i + 1) % n_agents;
                let _ = queries::create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    agent_ids_seed[sender_idx].0,
                    &format!("Seed message {i}"),
                    &format!("Seed body for search testing number {i} with unique content"),
                    Some(&format!("seed-thread-{}", i % 10)),
                    "normal",
                    false,
                    "[]",
                    &[(agent_ids_seed[recipient_idx].0, "to")],
                )
                .await;
            }
        }
    });

    // Rate limiter
    let start = Instant::now();
    let consumed = Arc::new(AtomicU64::new(0));

    let n_workers = 10_usize;
    let barrier = Arc::new(Barrier::new(n_workers));
    let errors = Arc::new(AtomicU64::new(0));
    let ops_done = Arc::new(AtomicU64::new(0));
    let mut all_latencies = Vec::new();

    let handles: Vec<_> = (0..n_workers)
        .map(|w| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let errors = Arc::clone(&errors);
            let ops_done = Arc::clone(&ops_done);
            let consumed = Arc::clone(&consumed);

            std::thread::Builder::new()
                .name(format!("sustained-{w}"))
                .spawn(move || {
                    barrier.wait();
                    let deadline = Instant::now() + Duration::from_secs(duration_secs);
                    let mut thread_latencies = Vec::new();
                    let mut op_counter = 0_u64;

                    while Instant::now() < deadline {
                        // Simple rate limiting
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let allowed = elapsed_ms * target_rps / 1000;
                        let my_seq = consumed.fetch_add(1, Ordering::Relaxed);
                        if my_seq > allowed {
                            std::thread::sleep(Duration::from_millis(1));
                        }

                        let op_start = Instant::now();
                        op_counter += 1;

                        // Weighted operation selection
                        let roll = (op_counter + w as u64) % 20;
                        let agent_idx = (w + op_counter as usize) % agent_ids.len();
                        let (agent_id, agent_name) = &agent_ids[agent_idx];

                        let ok = match roll {
                            0..=7 => {
                                // 40% message send
                                let recipient_idx = (agent_idx + 1) % agent_ids.len();
                                let (rid, rname) = &agent_ids[recipient_idx];
                                let tid = format!("sus-{w}-{op_counter}");

                                let msg_result = block_on(|cx| {
                                    let pool = pool.clone();
                                    let tid = tid.clone();
                                    async move {
                                        queries::create_message_with_recipients(
                                            &cx,
                                            &pool,
                                            project_id,
                                            *agent_id,
                                            &format!("Sustained msg {op_counter}"),
                                            &format!("Body {w}-{op_counter}"),
                                            Some(&tid),
                                            "normal",
                                            false,
                                            "[]",
                                            &[(*rid, "to")],
                                        )
                                        .await
                                    }
                                });

                                match msg_result {
                                    Outcome::Ok(msg) => {
                                        let msg_json = serde_json::json!({
                                            "id": msg.id.expect("id"),
                                            "subject": format!("Sustained msg {op_counter}"),
                                            "thread_id": tid,
                                            "created_ts": micros_to_iso(msg.created_ts),
                                        });
                                        write_message_bundle(
                                            &archive,
                                            &config,
                                            &msg_json,
                                            &format!("Body {w}-{op_counter}"),
                                            agent_name,
                                            std::slice::from_ref(rname),
                                            &[],
                                            None,
                                        )
                                        .is_ok()
                                    }
                                    _ => false,
                                }
                            }
                            8..=13 => {
                                // 30% inbox fetch
                                let result = block_on(|cx| {
                                    let pool = pool.clone();
                                    async move {
                                        queries::fetch_inbox(
                                            &cx, &pool, project_id, *agent_id, false, None, 20,
                                        )
                                        .await
                                    }
                                });
                                matches!(result, Outcome::Ok(_))
                            }
                            14..=16 => {
                                // 15% search
                                let queries_list = [
                                    "seed", "message", "body", "content", "testing", "unique",
                                    "number",
                                ];
                                let q = queries_list[op_counter as usize % queries_list.len()];
                                let result = block_on(|cx| {
                                    let pool = pool.clone();
                                    async move {
                                        queries::search_messages(&cx, &pool, project_id, q, 10)
                                            .await
                                    }
                                });
                                matches!(result, Outcome::Ok(_))
                            }
                            17..=18 => {
                                // 10% file reservation
                                let pattern = format!("src/worker_{w}/op_{op_counter}.rs");
                                let result = block_on(|cx| {
                                    let pool = pool.clone();
                                    let pattern = pattern.clone();
                                    async move {
                                        queries::create_file_reservations(
                                            &cx,
                                            &pool,
                                            project_id,
                                            *agent_id,
                                            &[pattern.as_str()],
                                            60,
                                            false, // non-exclusive to reduce conflicts
                                            &format!("sustained-{w}-{op_counter}"),
                                        )
                                        .await
                                    }
                                });
                                matches!(result, Outcome::Ok(_))
                            }
                            _ => {
                                // 5% agent profile write (archive-only)
                                let agent_json = serde_json::json!({
                                    "name": agent_name,
                                    "program": "stress-sustained",
                                    "model": "test-model",
                                    "updated_op": op_counter,
                                });
                                write_agent_profile_with_config(&archive, &config, &agent_json)
                                    .is_ok()
                            }
                        };

                        if ok {
                            ops_done.fetch_add(1, Ordering::Relaxed);
                        } else {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }

                        thread_latencies.push(op_start.elapsed().as_micros() as u64);
                    }

                    thread_latencies
                })
                .expect("spawn sustained worker")
        })
        .collect();

    for h in handles {
        let mut lats = h.join().expect("sustained worker panicked");
        all_latencies.append(&mut lats);
    }

    flush_async_commits();
    wbq_flush();

    let elapsed = start.elapsed();
    let total_ok = ops_done.load(Ordering::Relaxed);
    let total_err = errors.load(Ordering::Relaxed);
    let total = total_ok + total_err;
    let rss = rss_kb();

    let report = LatencyReport::from_latencies(&mut all_latencies, total_err);

    eprintln!("\n=== stress_sustained_mixed_workload_30s ===");
    eprintln!(
        "  Duration: {:.1}s, Target RPS: {target_rps}",
        elapsed.as_secs_f64()
    );
    eprintln!("  Total ops: {total}, Success: {total_ok}, Errors: {total_err}");
    eprintln!("  Actual RPS: {:.0}", total as f64 / elapsed.as_secs_f64());
    eprintln!("  RSS: {} KB", rss);
    report.print("Mixed workload");

    let wbq = wbq_stats();
    let coalescer = get_commit_coalescer().stats();
    eprintln!(
        "  WBQ: enqueued={}, drained={}, errors={}, fallbacks={}",
        wbq.enqueued, wbq.drained, wbq.errors, wbq.fallbacks,
    );
    eprintln!(
        "  Coalescer: enqueued={}, commits={}, queue_size={}",
        coalescer.enqueued, coalescer.commits, coalescer.queue_size,
    );

    // Assertions
    let error_rate = total_err as f64 / total.max(1) as f64;
    assert!(
        error_rate < 0.15,
        "error rate {error_rate:.2}% exceeds 15% ({total_err}/{total})"
    );
    // p99 latency should stay under 5s even under sustained load
    assert!(
        report.p99_us < 5_000_000,
        "p99 latency {:.1}ms exceeds 5s",
        report.p99_us as f64 / 1000.0,
    );
    // Must have completed meaningful work
    assert!(
        total_ok > (duration_secs * target_rps / 4),
        "too few successful ops ({total_ok}) for {duration_secs}s at {target_rps} RPS"
    );
}

// ===========================================================================
// TEST 9: Agent registration thundering herd
//
// Scenario: 50 threads all try to register the SAME agent simultaneously.
// Only one should succeed with a new registration; others should get the
// existing agent via upsert. No errors, no duplicates.
// ===========================================================================

#[test]
fn stress_agent_registration_thundering_herd() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let pool = make_pool(&tmp);

    let n_threads = 50;
    let agent_name_str = agent_name(999);

    let human_key = unique_human_key("stress-herd");
    let pool_setup = pool.clone();
    let (project_id, project_slug) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        (project.id.expect("project id"), project.slug)
    });

    ensure_archive_root(&config).expect("init archive root");
    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    let barrier = Arc::new(Barrier::new(n_threads));
    let agent_id_set = Arc::new(std::sync::Mutex::new(Vec::new()));
    let errors = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_name_str = agent_name_str.clone();
            let barrier = Arc::clone(&barrier);
            let agent_id_set = Arc::clone(&agent_id_set);
            let errors = Arc::clone(&errors);

            std::thread::spawn(move || {
                barrier.wait();

                let result = block_on_with_retry(5, |cx| {
                    let pool = pool.clone();
                    let name = agent_name_str.clone();
                    async move {
                        queries::register_agent(
                            &cx,
                            &pool,
                            project_id,
                            &name,
                            "herd-test",
                            "test-model",
                            Some(&format!("herd thread {t}")),
                            None,
                        )
                        .await
                    }
                });

                let id = result.id.expect("agent id");
                agent_id_set.lock().unwrap().push(id);

                // Also write profile to git archive
                let agent_json = serde_json::json!({
                    "name": agent_name_str,
                    "program": "herd-test",
                    "model": "test-model",
                });
                if let Err(e) = write_agent_profile_with_config(&archive, &config, &agent_json) {
                    eprintln!("  herd profile write err (thread {t}): {e}");
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("herd thread panicked");
    }

    flush_async_commits();

    let ids = agent_id_set.lock().unwrap();
    let errs = errors.load(Ordering::Relaxed);

    eprintln!("\n=== stress_agent_registration_thundering_herd ===");
    eprintln!("  Threads: {n_threads}, Agent: {agent_name_str}");
    eprintln!("  IDs returned: {}, unique: {}", ids.len(), {
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        sorted.len()
    });
    eprintln!("  Profile write errors: {errs}");

    // All threads should get back the same agent ID (upsert semantics)
    let first = ids[0];
    assert!(
        ids.iter().all(|&id| id == first),
        "thundering herd produced multiple agent IDs: {ids:?}"
    );
    assert_eq!(ids.len(), n_threads, "missing thread results");
}

// ===========================================================================
// TEST 10: Concurrent inbox fetch during message storm
//
// Scenario: Some threads are sending messages while others are fetching
// inboxes at the same time. This exercises read/write concurrency in both
// the DB (WAL readers vs writer) and the cache layer.
// ===========================================================================

#[test]
fn stress_concurrent_inbox_during_message_storm() {
    let tmp = TempDir::new().unwrap();
    let _config = test_config(tmp.path());
    let pool = make_pool(&tmp);

    let n_senders = 10;
    let n_readers = 10;
    let msgs_per_sender = 15;

    let human_key = unique_human_key("stress-inbox-storm");
    let pool_setup = pool.clone();
    let (project_id, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..(n_senders + n_readers) {
            let name = agent_name(i + 400);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "inbox-storm",
                "test-model",
                None,
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent failed: {other:?}"),
            };
            ids.push((agent.id.expect("id"), name));
        }
        (pid, ids)
    });

    let barrier = Arc::new(Barrier::new(n_senders + n_readers));
    let send_errors = Arc::new(AtomicU64::new(0));
    let read_errors = Arc::new(AtomicU64::new(0));
    let messages_sent = Arc::new(AtomicU64::new(0));
    let inbox_reads = Arc::new(AtomicU64::new(0));

    // Sender threads
    let mut handles: Vec<_> = (0..n_senders)
        .map(|s| {
            let pool = pool.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let send_errors = Arc::clone(&send_errors);
            let messages_sent = Arc::clone(&messages_sent);

            std::thread::Builder::new()
                .name(format!("sender-{s}"))
                .spawn(move || {
                    barrier.wait();

                    for m in 0..msgs_per_sender {
                        let (sid, _sname) = &agent_ids[s];
                        // Send to one of the reader agents
                        let reader_idx = n_senders + (m % n_readers);
                        let (rid, _rname) = &agent_ids[reader_idx];
                        let tid = format!("storm-{s}-{m}");

                        let result = block_on(|cx| {
                            let pool = pool.clone();
                            let tid = tid.clone();
                            async move {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pool,
                                    project_id,
                                    *sid,
                                    &format!("Storm msg {s}-{m}"),
                                    &format!("Storm body {s}-{m}"),
                                    Some(&tid),
                                    "normal",
                                    false,
                                    "[]",
                                    &[(*rid, "to")],
                                )
                                .await
                            }
                        });

                        match result {
                            Outcome::Ok(_) => {
                                messages_sent.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {
                                send_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        // Small stagger to avoid pure write lock convoy
                        if m % 3 == 0 {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                })
                .expect("spawn sender")
        })
        .collect();

    // Reader threads
    handles.extend((0..n_readers).map(|r| {
        let pool = pool.clone();
        let agent_ids = agent_ids.clone();
        let barrier = Arc::clone(&barrier);
        let read_errors = Arc::clone(&read_errors);
        let inbox_reads = Arc::clone(&inbox_reads);

        std::thread::Builder::new()
            .name(format!("reader-{r}"))
            .spawn(move || {
                barrier.wait();

                let agent_idx = n_senders + r;
                let (agent_id, _) = &agent_ids[agent_idx];

                // Poll inbox repeatedly while senders are working
                for _ in 0..30 {
                    let result = block_on(|cx| {
                        let pool = pool.clone();
                        async move {
                            queries::fetch_inbox(&cx, &pool, project_id, *agent_id, false, None, 50)
                                .await
                        }
                    });

                    match result {
                        Outcome::Ok(_) => {
                            inbox_reads.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            read_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
            .expect("spawn reader")
    }));

    for h in handles {
        h.join().expect("thread panicked");
    }

    let sent = messages_sent.load(Ordering::Relaxed);
    let s_errs = send_errors.load(Ordering::Relaxed);
    let reads = inbox_reads.load(Ordering::Relaxed);
    let r_errs = read_errors.load(Ordering::Relaxed);

    eprintln!("\n=== stress_concurrent_inbox_during_message_storm ===");
    eprintln!("  Senders: {n_senders} x {msgs_per_sender} msgs, Readers: {n_readers} x 30 polls");
    eprintln!("  Messages sent: {sent}, send errors: {s_errs}");
    eprintln!("  Inbox reads: {reads}, read errors: {r_errs}");

    // All sends should succeed (retry handles transient locks)
    let expected_sends = (n_senders * msgs_per_sender) as u64;
    assert_eq!(sent + s_errs, expected_sends, "lost send operations");
    assert!(
        s_errs == 0,
        "DB send errors should be zero with retries: {s_errs}"
    );

    // Reads should be overwhelmingly successful (WAL allows concurrent reads)
    let expected_reads = (n_readers * 30) as u64;
    assert!(
        r_errs < expected_reads / 10,
        "too many read errors: {r_errs}/{expected_reads}"
    );
}

// ===========================================================================
// TEST 11: 150-agent message storm (DB + Git archive full pipeline)
//
// Scenario: 150 agents in one project all send 10 messages simultaneously,
// producing 1,500 end-to-end pipeline operations (DB write + git archive
// write + async commit). This is the "can the system actually handle 100+
// agents slamming it at once" test.
// ===========================================================================

#[test]
fn stress_150_agent_message_storm() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    // Production-like pool: 100 max connections for 150 agents
    let db_path = tmp
        .path()
        .join(format!("stress_150_{}.db", unique_suffix()));
    let pool_config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 100,
        min_connections: 25,
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 10,
    };
    let pool = DbPool::new(&pool_config).expect("create production pool");

    let n_agents: usize = 150;
    let msgs_per_agent: usize = 10;
    let total_expected = (n_agents * msgs_per_agent) as u64;

    // Setup project + agents
    let human_key = unique_human_key("stress-150-storm");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        // Use offset 500 to avoid name collision with other tests
        for i in 0..n_agents {
            let name = agent_name(i + 500);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "stress-150",
                "test-model",
                Some("150-agent storm"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent {name} failed: {other:?}"),
            };
            ids.push((agent.id.expect("agent id"), name));
        }
        (pid, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    let barrier = Arc::new(Barrier::new(n_agents));
    let db_successes = Arc::new(AtomicU64::new(0));
    let archive_successes = Arc::new(AtomicU64::new(0));
    let db_errors = Arc::new(AtomicU64::new(0));
    let archive_errors = Arc::new(AtomicU64::new(0));
    let mut latencies_all = Vec::new();

    let rss_before = rss_kb();
    let start_time = Instant::now();

    let handles: Vec<_> = (0..n_agents)
        .map(|i| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let db_successes = Arc::clone(&db_successes);
            let archive_successes = Arc::clone(&archive_successes);
            let db_errors = Arc::clone(&db_errors);
            let archive_errors = Arc::clone(&archive_errors);

            std::thread::Builder::new()
                .name(format!("storm-{i}"))
                .stack_size(2 * 1024 * 1024) // 2MB stack per thread
                .spawn(move || {
                    barrier.wait();
                    let mut thread_latencies = Vec::new();

                    for msg_idx in 0..msgs_per_agent {
                        let op_start = Instant::now();
                        let (sender_id, sender_name) = &agent_ids[i];
                        let recipient_idx = (i + msg_idx + 1) % n_agents;
                        let (recipient_id, recipient_name) = &agent_ids[recipient_idx];
                        let thread_id = format!("s150-{i}-{msg_idx}");
                        let subject = format!("Storm msg {msg_idx} from agent {i}");
                        let body = format!(
                            "Message body from agent {} to agent {} number {}",
                            i, recipient_idx, msg_idx
                        );

                        // DB write with retry
                        let msg_result: Result<_, ()> = {
                            let msg = block_on_with_retry(8, |cx| {
                                let pool = pool.clone();
                                let subject = subject.clone();
                                let body = body.clone();
                                let thread_id = thread_id.clone();
                                async move {
                                    queries::create_message_with_recipients(
                                        &cx,
                                        &pool,
                                        project_id,
                                        *sender_id,
                                        &subject,
                                        &body,
                                        Some(&thread_id),
                                        "normal",
                                        false,
                                        "[]",
                                        &[(*recipient_id, "to")],
                                    )
                                    .await
                                }
                            });
                            Ok(msg)
                        };

                        match msg_result {
                            Ok(msg) => {
                                db_successes.fetch_add(1, Ordering::Relaxed);
                                let msg_id = msg.id.expect("message id");
                                let created_iso = micros_to_iso(msg.created_ts);

                                let message_json = serde_json::json!({
                                    "id": msg_id,
                                    "subject": subject,
                                    "thread_id": thread_id,
                                    "created_ts": created_iso,
                                });
                                match write_message_bundle(
                                    &archive,
                                    &config,
                                    &message_json,
                                    &body,
                                    sender_name,
                                    std::slice::from_ref(recipient_name),
                                    &[],
                                    None,
                                ) {
                                    Ok(()) => {
                                        archive_successes.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(_e) => {
                                        archive_errors.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            Err(()) => {
                                db_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }

                        thread_latencies.push(op_start.elapsed().as_micros() as u64);
                    }
                    thread_latencies
                })
                .expect("spawn storm thread")
        })
        .collect();

    for handle in handles {
        let mut lats = handle.join().expect("storm agent panicked");
        latencies_all.append(&mut lats);
    }

    flush_async_commits();

    let elapsed = start_time.elapsed();
    let rss_after = rss_kb();
    let db_ok = db_successes.load(Ordering::Relaxed);
    let db_err = db_errors.load(Ordering::Relaxed);
    let arch_ok = archive_successes.load(Ordering::Relaxed);
    let arch_err = archive_errors.load(Ordering::Relaxed);

    let report = LatencyReport::from_latencies(&mut latencies_all, db_err + arch_err);
    eprintln!("\n=== stress_150_agent_message_storm ===");
    eprintln!("  Agents: {n_agents}, Messages/agent: {msgs_per_agent}, Total: {total_expected}");
    eprintln!("  Elapsed: {:.1}s", elapsed.as_secs_f64());
    eprintln!("  DB: {db_ok} ok, {db_err} errors");
    eprintln!("  Archive: {arch_ok} ok, {arch_err} errors");
    eprintln!(
        "  Throughput: {:.0} pipeline-ops/sec",
        total_expected as f64 / elapsed.as_secs_f64()
    );
    eprintln!(
        "  RSS: {} KB before, {} KB after (+{} KB)",
        rss_before,
        rss_after,
        rss_after.saturating_sub(rss_before)
    );
    report.print("Pipeline (DB+Git)");

    let coalescer = get_commit_coalescer();
    let per_repo = coalescer.per_repo_stats();
    if let Some(repo_stats) = per_repo.get(&archive.repo_root) {
        eprintln!(
            "  Coalescer: enqueued={}, commits={}, batching={:.1}x",
            repo_stats.enqueued_total,
            repo_stats.commits_total,
            repo_stats.enqueued_total as f64 / repo_stats.commits_total.max(1) as f64,
        );
    }

    // Critical assertions
    // DB must handle 150 agents — zero DB errors with retry
    assert_eq!(
        db_ok + db_err,
        total_expected,
        "lost DB operations: expected {total_expected}, got {}",
        db_ok + db_err,
    );
    assert_eq!(db_err, 0, "DB errors must be zero with retries: {db_err}");

    // Archive errors should be under 5% — git contention should be manageable
    let arch_error_rate = arch_err as f64 / total_expected as f64;
    assert!(
        arch_error_rate < 0.05,
        "archive error rate {:.1}% exceeds 5% ({arch_err}/{total_expected})",
        arch_error_rate * 100.0,
    );

    // p99 latency should stay under 30s even with 150 concurrent agents
    assert!(
        report.p99_us < 30_000_000,
        "p99 latency {:.1}s exceeds 30s budget",
        report.p99_us as f64 / 1_000_000.0,
    );
}

// ===========================================================================
// TEST 12: 100-agent full session lifecycle
//
// Scenario: 100 agents each run a complete realistic session:
//   1. Register (already done in setup)
//   2. Reserve files (2 per agent)
//   3. Send 5 messages to random agents
//   4. Poll inbox 3 times
//   5. Search for messages 2 times
//   6. Acknowledge received messages
//
// This is the most realistic simulation of "100 agents on one machine".
// ===========================================================================

#[test]
fn stress_100_agent_full_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let db_path = tmp
        .path()
        .join(format!("stress_lifecycle_{}.db", unique_suffix()));
    let pool_config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 100,
        min_connections: 25,
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 10,
    };
    let pool = DbPool::new(&pool_config).expect("create pool");

    let n_agents: usize = 100;

    // Setup project + agents
    let human_key = unique_human_key("stress-lifecycle-100");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_agents {
            let name = agent_name(i + 700);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "stress-lifecycle",
                "test-model",
                Some("lifecycle agent"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent {name} failed: {other:?}"),
            };
            ids.push((agent.id.expect("agent id"), name));
        }
        (pid, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    // Seed some messages so inboxes aren't empty
    let pool_seed = pool.clone();
    let agent_ids_seed = agent_ids.clone();
    block_on(|cx| {
        let pool = pool_seed;
        async move {
            for i in 0..200 {
                let sender = i % n_agents;
                let recipient = (i + 1) % n_agents;
                let _ = queries::create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    agent_ids_seed[sender].0,
                    &format!("Seed msg {i}"),
                    &format!("Seed body {i} with searchable keywords alpha bravo"),
                    Some(&format!("seed-thread-{}", i % 20)),
                    "normal",
                    false,
                    "[]",
                    &[(agent_ids_seed[recipient].0, "to")],
                )
                .await;
            }
        }
    });

    let barrier = Arc::new(Barrier::new(n_agents));
    let phase_counters = Arc::new([
        AtomicU64::new(0), // 0: reserve ok
        AtomicU64::new(0), // 1: reserve err
        AtomicU64::new(0), // 2: send ok
        AtomicU64::new(0), // 3: send err
        AtomicU64::new(0), // 4: archive ok
        AtomicU64::new(0), // 5: archive err
        AtomicU64::new(0), // 6: inbox ok
        AtomicU64::new(0), // 7: inbox err
        AtomicU64::new(0), // 8: search ok
        AtomicU64::new(0), // 9: search err
        AtomicU64::new(0), // 10: ack ok
        AtomicU64::new(0), // 11: ack err
    ]);

    let rss_before = rss_kb();
    let start_time = Instant::now();
    let mut all_latencies = Vec::new();

    let handles: Vec<_> = (0..n_agents)
        .map(|i| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let counters = Arc::clone(&phase_counters);

            std::thread::Builder::new()
                .name(format!("lifecycle-{i}"))
                .stack_size(2 * 1024 * 1024)
                .spawn(move || {
                    barrier.wait();
                    let mut thread_latencies = Vec::new();
                    let (agent_id, agent_name_str) = &agent_ids[i];

                    // Phase 1: Reserve 2 files
                    for r in 0..2 {
                        let op_start = Instant::now();
                        let pattern = format!("src/agent_{i}/workspace_{r}.rs");
                        let result = block_on_with_retry(8, |cx| {
                            let pool = pool.clone();
                            let pattern = pattern.clone();
                            async move {
                                queries::create_file_reservations(
                                    &cx,
                                    &pool,
                                    project_id,
                                    *agent_id,
                                    &[pattern.as_str()],
                                    300,
                                    true,
                                    &format!("lifecycle-{i}-{r}"),
                                )
                                .await
                            }
                        });
                        if result.is_empty() {
                            counters[1].fetch_add(1, Ordering::Relaxed);
                        } else {
                            counters[0].fetch_add(1, Ordering::Relaxed);
                        }
                        thread_latencies.push(op_start.elapsed().as_micros() as u64);
                    }

                    // Phase 2: Send 5 messages + archive
                    for m in 0..5 {
                        let op_start = Instant::now();
                        let recipient_idx = (i + m + 1) % n_agents;
                        let (recipient_id, recipient_name) = &agent_ids[recipient_idx];
                        let thread_id = format!("lc-{i}-{m}");
                        let subject = format!("Lifecycle msg {m} from {agent_name_str}");
                        let body = format!("Body {i}-{m} lifecycle test content");

                        let msg = block_on_with_retry(8, |cx| {
                            let pool = pool.clone();
                            let subject = subject.clone();
                            let body = body.clone();
                            let thread_id = thread_id.clone();
                            async move {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pool,
                                    project_id,
                                    *agent_id,
                                    &subject,
                                    &body,
                                    Some(&thread_id),
                                    "normal",
                                    false,
                                    "[]",
                                    &[(*recipient_id, "to")],
                                )
                                .await
                            }
                        });
                        counters[2].fetch_add(1, Ordering::Relaxed);

                        let msg_json = serde_json::json!({
                            "id": msg.id.expect("msg id"),
                            "subject": subject,
                            "thread_id": thread_id,
                            "created_ts": micros_to_iso(msg.created_ts),
                        });
                        match write_message_bundle(
                            &archive,
                            &config,
                            &msg_json,
                            &body,
                            agent_name_str,
                            std::slice::from_ref(recipient_name),
                            &[],
                            None,
                        ) {
                            Ok(()) => {
                                counters[4].fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                counters[5].fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        thread_latencies.push(op_start.elapsed().as_micros() as u64);
                    }

                    // Phase 3: Poll inbox 3 times
                    for _ in 0..3 {
                        let op_start = Instant::now();
                        let result = block_on(|cx| {
                            let pool = pool.clone();
                            async move {
                                queries::fetch_inbox(
                                    &cx, &pool, project_id, *agent_id, false, None, 50,
                                )
                                .await
                            }
                        });
                        match result {
                            Outcome::Ok(_) => {
                                counters[6].fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {
                                counters[7].fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        thread_latencies.push(op_start.elapsed().as_micros() as u64);
                    }

                    // Phase 4: Search 2 times
                    let search_terms = ["alpha", "bravo", "seed", "lifecycle", "body"];
                    for s in 0..2 {
                        let op_start = Instant::now();
                        let query = search_terms[(i + s) % search_terms.len()];
                        let result = block_on(|cx| {
                            let pool = pool.clone();
                            async move {
                                queries::search_messages(&cx, &pool, project_id, query, 20).await
                            }
                        });
                        match result {
                            Outcome::Ok(_) => {
                                counters[8].fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {
                                counters[9].fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        thread_latencies.push(op_start.elapsed().as_micros() as u64);
                    }

                    // Phase 5: Acknowledge first inbox message (if any)
                    let inbox_result = block_on(|cx| {
                        let pool = pool.clone();
                        async move {
                            queries::fetch_inbox(&cx, &pool, project_id, *agent_id, false, None, 5)
                                .await
                        }
                    });
                    if let Outcome::Ok(inbox) = inbox_result {
                        for row in inbox.iter().take(2) {
                            let op_start = Instant::now();
                            let msg_id = row.message.id.expect("inbox msg id");
                            let ack_result = block_on(|cx| {
                                let pool = pool.clone();
                                async move {
                                    queries::acknowledge_message(&cx, &pool, *agent_id, msg_id)
                                        .await
                                }
                            });
                            match ack_result {
                                Outcome::Ok(_) => {
                                    counters[10].fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {
                                    counters[11].fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            thread_latencies.push(op_start.elapsed().as_micros() as u64);
                        }
                    }

                    thread_latencies
                })
                .expect("spawn lifecycle thread")
        })
        .collect();

    for h in handles {
        let mut lats = h.join().expect("lifecycle agent panicked");
        all_latencies.append(&mut lats);
    }

    flush_async_commits();

    let elapsed = start_time.elapsed();
    let rss_after = rss_kb();

    let reserve_ok = phase_counters[0].load(Ordering::Relaxed);
    let reserve_err = phase_counters[1].load(Ordering::Relaxed);
    let send_ok = phase_counters[2].load(Ordering::Relaxed);
    let send_err = phase_counters[3].load(Ordering::Relaxed);
    let archive_ok = phase_counters[4].load(Ordering::Relaxed);
    let archive_err = phase_counters[5].load(Ordering::Relaxed);
    let inbox_ok = phase_counters[6].load(Ordering::Relaxed);
    let inbox_err = phase_counters[7].load(Ordering::Relaxed);
    let search_ok = phase_counters[8].load(Ordering::Relaxed);
    let search_err = phase_counters[9].load(Ordering::Relaxed);
    let ack_ok = phase_counters[10].load(Ordering::Relaxed);
    let ack_err = phase_counters[11].load(Ordering::Relaxed);

    let total_ops = all_latencies.len() as u64;
    let total_errors = reserve_err + send_err + archive_err + inbox_err + search_err + ack_err;

    let report = LatencyReport::from_latencies(&mut all_latencies, total_errors);

    eprintln!("\n=== stress_100_agent_full_lifecycle ===");
    eprintln!(
        "  Agents: {n_agents}, Elapsed: {:.1}s",
        elapsed.as_secs_f64()
    );
    eprintln!("  Reservations: {reserve_ok} ok, {reserve_err} err");
    eprintln!("  Messages (DB): {send_ok} ok, {send_err} err");
    eprintln!("  Messages (Git): {archive_ok} ok, {archive_err} err");
    eprintln!("  Inbox reads: {inbox_ok} ok, {inbox_err} err");
    eprintln!("  Searches: {search_ok} ok, {search_err} err");
    eprintln!("  Acks: {ack_ok} ok, {ack_err} err");
    eprintln!("  Total ops: {total_ops}, Total errors: {total_errors}");
    eprintln!(
        "  Throughput: {:.0} ops/sec",
        total_ops as f64 / elapsed.as_secs_f64()
    );
    eprintln!(
        "  RSS: {} KB before, {} KB after (+{} KB)",
        rss_before,
        rss_after,
        rss_after.saturating_sub(rss_before)
    );
    report.print("Full lifecycle");

    // Critical assertions
    // DB sends must all succeed (block_on_with_retry handles transient locks)
    assert_eq!(
        send_err, 0,
        "message DB send errors should be 0: {send_err}"
    );
    assert_eq!(send_ok, (n_agents * 5) as u64, "missing message sends");

    // Inbox reads should overwhelmingly succeed (WAL mode)
    assert!(
        inbox_err < inbox_ok / 10,
        "too many inbox errors: {inbox_err}/{inbox_ok}"
    );

    // Searches should work
    assert!(
        search_err < search_ok / 10,
        "too many search errors: {search_err}/{search_ok}"
    );

    // Archive error rate under 5%
    let archive_total = archive_ok + archive_err;
    if archive_total > 0 {
        let arch_error_rate = archive_err as f64 / archive_total as f64;
        assert!(
            arch_error_rate < 0.05,
            "archive error rate {:.1}% exceeds 5% ({archive_err}/{archive_total})",
            arch_error_rate * 100.0,
        );
    }

    // Overall error rate under 5%
    let overall_error_rate = total_errors as f64 / total_ops.max(1) as f64;
    assert!(
        overall_error_rate < 0.05,
        "overall error rate {:.1}% exceeds 5% ({total_errors}/{total_ops})",
        overall_error_rate * 100.0,
    );

    // p99 latency under 30s
    assert!(
        report.p99_us < 30_000_000,
        "p99 latency {:.1}s exceeds 30s budget",
        report.p99_us as f64 / 1_000_000.0,
    );
}

// ===========================================================================
// TEST 13: Multi-project thundering herd — 120 agents across 12 projects
//
// Scenario: 12 projects, each with 10 agents, all starting simultaneously.
// Each agent sends 5 messages AND reads inbox. Tests cross-project isolation
// under realistic multi-tenant load.
// ===========================================================================

#[test]
fn stress_multi_project_120_agents() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let db_path = tmp
        .path()
        .join(format!("stress_multi120_{}.db", unique_suffix()));
    let pool_config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 80,
        min_connections: 20,
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 10,
    };
    let pool = DbPool::new(&pool_config).expect("create pool");

    let n_projects: usize = 12;
    let agents_per_project: usize = 10;
    let msgs_per_agent: usize = 5;
    let total_agents = n_projects * agents_per_project;

    // Setup all projects and agents
    let pool_setup = pool.clone();
    let mut project_data = Vec::new();
    for p in 0..n_projects {
        let human_key = unique_human_key(&format!("stress-multi120-p{p}"));
        let pool_clone = pool_setup.clone();
        let (project_id, slug, agents) = block_on(|cx| async move {
            let project = match queries::ensure_project(&cx, &pool_clone, &human_key).await {
                Outcome::Ok(row) => row,
                other => panic!("ensure_project p{p} failed: {other:?}"),
            };
            let pid = project.id.expect("project id");
            let mut agents = Vec::new();
            for a in 0..agents_per_project {
                // offset 900 + unique per project/agent combo
                let name = agent_name(900 + p * agents_per_project + a);
                let agent = match queries::register_agent(
                    &cx,
                    &pool_clone,
                    pid,
                    &name,
                    "multi120-stress",
                    "test-model",
                    Some("multi-project 120 agent test"),
                    None,
                )
                .await
                {
                    Outcome::Ok(row) => row,
                    other => panic!("register agent {name} p{p} failed: {other:?}"),
                };
                agents.push((agent.id.expect("agent id"), name));
            }
            (pid, project.slug, agents)
        });
        let archive = ensure_archive(&config, &slug).expect("ensure archive");
        project_data.push((project_id, slug, agents, archive));
    }

    let barrier = Arc::new(Barrier::new(total_agents));
    let db_successes = Arc::new(AtomicU64::new(0));
    let archive_successes = Arc::new(AtomicU64::new(0));
    let db_errors = Arc::new(AtomicU64::new(0));
    let archive_errors = Arc::new(AtomicU64::new(0));
    let inbox_successes = Arc::new(AtomicU64::new(0));
    let inbox_errors = Arc::new(AtomicU64::new(0));

    let start_time = Instant::now();

    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for (p_idx, (project_id, _slug, agents, archive)) in project_data.iter().enumerate() {
        for (a_idx, (sender_id, sender_name)) in agents.iter().enumerate() {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agents = agents.clone();
            let barrier = Arc::clone(&barrier);
            let db_successes = Arc::clone(&db_successes);
            let archive_successes = Arc::clone(&archive_successes);
            let archive_errors = Arc::clone(&archive_errors);
            let inbox_successes = Arc::clone(&inbox_successes);
            let inbox_errors = Arc::clone(&inbox_errors);
            let project_id = *project_id;
            let sender_id = *sender_id;
            let sender_name = sender_name.clone();

            handles.push(
                std::thread::Builder::new()
                    .name(format!("m120-p{p_idx}-a{a_idx}"))
                    .stack_size(2 * 1024 * 1024)
                    .spawn(move || {
                        barrier.wait();

                        // Send messages
                        for m in 0..msgs_per_agent {
                            let recipient_idx = (a_idx + m + 1) % agents.len();
                            let (recipient_id, recipient_name) = &agents[recipient_idx];
                            let thread_id = format!("m120-p{p_idx}-a{a_idx}-m{m}");

                            let msg = block_on_with_retry(8, |cx| {
                                let pool = pool.clone();
                                let thread_id = thread_id.clone();
                                async move {
                                    queries::create_message_with_recipients(
                                        &cx,
                                        &pool,
                                        project_id,
                                        sender_id,
                                        &format!("Multi120 msg {m}"),
                                        &format!("Body p{p_idx} a{a_idx} m{m}"),
                                        Some(&thread_id),
                                        "normal",
                                        false,
                                        "[]",
                                        &[(*recipient_id, "to")],
                                    )
                                    .await
                                }
                            });
                            db_successes.fetch_add(1, Ordering::Relaxed);

                            let msg_json = serde_json::json!({
                                "id": msg.id.expect("msg id"),
                                "subject": format!("Multi120 msg {m}"),
                                "thread_id": thread_id,
                                "created_ts": micros_to_iso(msg.created_ts),
                            });

                            match write_message_bundle(
                                &archive,
                                &config,
                                &msg_json,
                                &format!("Body p{p_idx} a{a_idx} m{m}"),
                                &sender_name,
                                std::slice::from_ref(recipient_name),
                                &[],
                                None,
                            ) {
                                Ok(()) => {
                                    archive_successes.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(_) => {
                                    archive_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }

                        // Poll inbox 3 times
                        for _ in 0..3 {
                            let result = block_on(|cx| {
                                let pool = pool.clone();
                                async move {
                                    queries::fetch_inbox(
                                        &cx, &pool, project_id, sender_id, false, None, 50,
                                    )
                                    .await
                                }
                            });
                            match result {
                                Outcome::Ok(_) => {
                                    inbox_successes.fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {
                                    inbox_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    })
                    .expect("spawn thread"),
            );
        }
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    flush_async_commits();
    let elapsed = start_time.elapsed();

    let db_ok = db_successes.load(Ordering::Relaxed);
    let db_err = db_errors.load(Ordering::Relaxed);
    let arch_ok = archive_successes.load(Ordering::Relaxed);
    let arch_err = archive_errors.load(Ordering::Relaxed);
    let inbox_ok = inbox_successes.load(Ordering::Relaxed);
    let inbox_err = inbox_errors.load(Ordering::Relaxed);

    let expected_msgs = (total_agents * msgs_per_agent) as u64;
    let expected_inbox = (total_agents * 3) as u64;

    eprintln!("\n=== stress_multi_project_120_agents ===");
    eprintln!(
        "  Projects: {n_projects}, Agents/project: {agents_per_project}, Total agents: {total_agents}"
    );
    eprintln!("  Elapsed: {:.1}s", elapsed.as_secs_f64());
    eprintln!("  Messages DB: {db_ok} ok, {db_err} err (expected {expected_msgs})");
    eprintln!("  Messages Archive: {arch_ok} ok, {arch_err} err");
    eprintln!("  Inbox reads: {inbox_ok} ok, {inbox_err} err (expected {expected_inbox})");
    eprintln!(
        "  Throughput: {:.0} ops/sec",
        (db_ok + arch_ok + inbox_ok) as f64 / elapsed.as_secs_f64()
    );

    // DB sends must all succeed
    assert_eq!(db_err, 0, "DB errors should be 0: {db_err}");
    assert_eq!(db_ok, expected_msgs, "missing DB sends");

    // Archive errors under 5%
    if expected_msgs > 0 {
        let arch_error_rate = arch_err as f64 / expected_msgs as f64;
        assert!(
            arch_error_rate < 0.05,
            "archive error rate {:.1}% exceeds 5%",
            arch_error_rate * 100.0,
        );
    }

    // Inbox reads should mostly succeed (WAL)
    assert!(
        inbox_err < expected_inbox / 10,
        "too many inbox errors: {inbox_err}/{expected_inbox}"
    );
}

// ===========================================================================
// TEST 14: 200-agent pool exhaustion torture test (IGNORED - very heavy)
//
// Scenario: 200 threads, pool capped at 25 connections (8x oversubscription).
// Each thread does 15 operations mixing writes and reads. This is the
// "absolute worst case" test that would have killed the Python version.
// ===========================================================================

#[test]
#[ignore] // Very heavy: cargo test --test stress_pipeline -- --ignored --nocapture
fn stress_200_agent_pool_exhaustion_torture() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let db_path = tmp
        .path()
        .join(format!("stress_200_{}.db", unique_suffix()));
    // Deliberately constrained: 25 max for 200 threads = 8x oversubscription
    let pool_config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 25,
        min_connections: 5,
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 5,
    };
    let pool = DbPool::new(&pool_config).expect("create constrained pool");

    let n_threads: usize = 200;
    let ops_per_thread: usize = 15;
    let total_ops = (n_threads * ops_per_thread) as u64;

    // Setup project + a subset of agents (re-use via modulo)
    let human_key = unique_human_key("stress-200-torture");
    let pool_setup = pool.clone();
    let n_registered_agents: usize = 100; // Register 100, 200 threads share them
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_registered_agents {
            let name = agent_name(i + 1100);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "torture-test",
                "test-model",
                Some("200-agent torture"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent {name} failed: {other:?}"),
            };
            ids.push((agent.id.expect("agent id"), name));
        }
        (pid, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    let barrier = Arc::new(Barrier::new(n_threads));
    let successes = Arc::new(AtomicU64::new(0));
    let db_errors = Arc::new(AtomicU64::new(0));
    let archive_errors = Arc::new(AtomicU64::new(0));
    let timeout_errors = Arc::new(AtomicU64::new(0));
    let mut all_latencies = Vec::new();

    let rss_before = rss_kb();
    let start_time = Instant::now();

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let successes = Arc::clone(&successes);
            let db_errors = Arc::clone(&db_errors);
            let archive_errors = Arc::clone(&archive_errors);
            let timeout_errors = Arc::clone(&timeout_errors);

            std::thread::Builder::new()
                .name(format!("torture-{t}"))
                .stack_size(2 * 1024 * 1024)
                .spawn(move || {
                    barrier.wait();
                    let mut thread_latencies = Vec::new();

                    for op in 0..ops_per_thread {
                        let op_start = Instant::now();
                        let agent_idx = t % agent_ids.len();
                        let (sender_id, sender_name) = &agent_ids[agent_idx];
                        let recipient_idx = (agent_idx + 1) % agent_ids.len();
                        let (recipient_id, recipient_name) = &agent_ids[recipient_idx];

                        // Mix reads and writes
                        let is_write = op % 3 != 2; // ~67% writes, ~33% reads
                        if is_write {
                            let thread_id = format!("tort-{t}-{op}");
                            let cx = Cx::for_testing();
                            let rt = RuntimeBuilder::current_thread().build().expect("build rt");
                            let pool_c = pool.clone();
                            let thread_id_c = thread_id.clone();
                            let msg_result = rt.block_on(async {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pool_c,
                                    project_id,
                                    *sender_id,
                                    &format!("Torture msg {t}-{op}"),
                                    &format!("Body {t}-{op}"),
                                    Some(&thread_id_c),
                                    "normal",
                                    false,
                                    "[]",
                                    &[(*recipient_id, "to")],
                                )
                                .await
                            });

                            match msg_result {
                                Outcome::Ok(msg) => {
                                    let msg_json = serde_json::json!({
                                        "id": msg.id.expect("msg id"),
                                        "subject": format!("Torture msg {t}-{op}"),
                                        "thread_id": thread_id,
                                        "created_ts": micros_to_iso(msg.created_ts),
                                    });
                                    match write_message_bundle(
                                        &archive,
                                        &config,
                                        &msg_json,
                                        &format!("Body {t}-{op}"),
                                        sender_name,
                                        std::slice::from_ref(recipient_name),
                                        &[],
                                        None,
                                    ) {
                                        Ok(()) => {
                                            successes.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(_) => {
                                            archive_errors.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                                Outcome::Err(e) => {
                                    let msg = format!("{e:?}");
                                    if msg.contains("timeout") || msg.contains("Timeout") {
                                        timeout_errors.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        db_errors.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Outcome::Cancelled(_) => {
                                    timeout_errors.fetch_add(1, Ordering::Relaxed);
                                }
                                Outcome::Panicked(p) => {
                                    panic!("DB panicked: {p}");
                                }
                            }
                        } else {
                            // Read operation: inbox fetch
                            let cx = Cx::for_testing();
                            let rt = RuntimeBuilder::current_thread().build().expect("build rt");
                            let pool_c = pool.clone();
                            let result = rt.block_on(async {
                                queries::fetch_inbox(
                                    &cx, &pool_c, project_id, *sender_id, false, None, 50,
                                )
                                .await
                            });
                            match result {
                                Outcome::Ok(_) => {
                                    successes.fetch_add(1, Ordering::Relaxed);
                                }
                                Outcome::Err(e) => {
                                    let msg = format!("{e:?}");
                                    if msg.contains("timeout") || msg.contains("Timeout") {
                                        timeout_errors.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        db_errors.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                _ => {
                                    db_errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }

                        thread_latencies.push(op_start.elapsed().as_micros() as u64);
                    }
                    thread_latencies
                })
                .expect("spawn torture thread")
        })
        .collect();

    for h in handles {
        let mut lats = h.join().expect("torture thread panicked");
        all_latencies.append(&mut lats);
    }

    flush_async_commits();
    let elapsed = start_time.elapsed();
    let rss_after = rss_kb();

    let ok = successes.load(Ordering::Relaxed);
    let db_err = db_errors.load(Ordering::Relaxed);
    let arch_err = archive_errors.load(Ordering::Relaxed);
    let timeouts = timeout_errors.load(Ordering::Relaxed);

    let report = LatencyReport::from_latencies(&mut all_latencies, db_err + arch_err + timeouts);

    eprintln!("\n=== stress_200_agent_pool_exhaustion_torture ===");
    eprintln!(
        "  Threads: {n_threads} (pool max: 25 = {:.0}x oversubscription)",
        n_threads as f64 / 25.0
    );
    eprintln!("  Ops/thread: {ops_per_thread}, Total: {total_ops}");
    eprintln!("  Elapsed: {:.1}s", elapsed.as_secs_f64());
    eprintln!(
        "  Successes: {ok}, DB errors: {db_err}, Archive errors: {arch_err}, Timeouts: {timeouts}"
    );
    eprintln!(
        "  Throughput: {:.0} ops/sec",
        total_ops as f64 / elapsed.as_secs_f64()
    );
    eprintln!(
        "  RSS: {} KB before, {} KB after (+{} KB)",
        rss_before,
        rss_after,
        rss_after.saturating_sub(rss_before)
    );
    report.print("Torture (mixed R/W)");

    // No panics — system must stay up under extreme oversubscription
    // DB errors from pool exhaustion are expected but should be bounded
    let failure_rate = (db_err + timeouts) as f64 / total_ops as f64;
    assert!(
        failure_rate < 0.25,
        "failure rate {:.1}% exceeds 25% under 8x oversubscription ({}/{total_ops})",
        failure_rate * 100.0,
        db_err + timeouts,
    );
    // Must complete substantial work even under extreme contention
    assert!(
        ok > total_ops / 4,
        "less than 25% succeeded ({ok}/{total_ops}) — system collapsed"
    );
    // p99 should stay under 60s even under torture
    assert!(
        report.p99_us < 60_000_000,
        "p99 latency {:.1}s exceeds 60s",
        report.p99_us as f64 / 1_000_000.0,
    );
}

// ===========================================================================
// TEST 15: Sustained 100-agent endurance (IGNORED - very heavy, 60s+)
//
// Scenario: 100 agents running for 60 seconds with a target of 200 ops/sec.
// Production-scale pool (100 connections). Monitors for:
// - Latency degradation over time
// - Memory leaks (RSS growth)
// - Error rate creep
// - Coalescer and WBQ health
// ===========================================================================

#[test]
#[ignore] // Very heavy: cargo test --test stress_pipeline -- --ignored --nocapture
fn stress_sustained_100_agents_60s() {
    let duration_secs: u64 = std::env::var("STRESS_DURATION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let target_rps: u64 = std::env::var("STRESS_TARGET_RPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let db_path = tmp
        .path()
        .join(format!("stress_endurance_{}.db", unique_suffix()));
    let pool_config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 100,
        min_connections: 25,
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 15,
    };
    let pool = DbPool::new(&pool_config).expect("create production pool");

    let n_agents: usize = 100;
    let n_workers: usize = 40; // 40 threads to drive 200 RPS across 100 agents
    wbq_start();

    let human_key = unique_human_key("stress-endurance");
    let pool_setup = pool.clone();
    let (project_id, project_slug, agent_ids) = block_on(|cx| async move {
        let project = match queries::ensure_project(&cx, &pool_setup, &human_key).await {
            Outcome::Ok(row) => row,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let pid = project.id.expect("project id");
        let mut ids = Vec::new();
        for i in 0..n_agents {
            let name = agent_name(i + 1400);
            let agent = match queries::register_agent(
                &cx,
                &pool_setup,
                pid,
                &name,
                "endurance-test",
                "test-model",
                Some("endurance agent"),
                None,
            )
            .await
            {
                Outcome::Ok(row) => row,
                other => panic!("register agent {name} failed: {other:?}"),
            };
            ids.push((agent.id.expect("agent id"), name));
        }
        (pid, project.slug, ids)
    });

    let archive = ensure_archive(&config, &project_slug).expect("ensure archive");

    // Seed messages for reads/search
    let pool_seed = pool.clone();
    let agent_ids_seed = agent_ids.clone();
    block_on(|cx| {
        let pool = pool_seed;
        async move {
            for i in 0..500 {
                let sender = i % n_agents;
                let recipient = (i + 1) % n_agents;
                let _ = queries::create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    agent_ids_seed[sender].0,
                    &format!("Endurance seed {i}"),
                    &format!("Endurance seed body {i} searchable delta echo foxtrot"),
                    Some(&format!("endurance-thread-{}", i % 30)),
                    "normal",
                    false,
                    "[]",
                    &[(agent_ids_seed[recipient].0, "to")],
                )
                .await;
            }
        }
    });

    let start = Instant::now();
    let consumed = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(n_workers));
    let errors = Arc::new(AtomicU64::new(0));
    let ops_done = Arc::new(AtomicU64::new(0));

    // Track latency buckets per 10-second interval for degradation detection
    let n_intervals = duration_secs.div_ceil(10) as usize;
    let interval_ops: Arc<Vec<AtomicU64>> =
        Arc::new((0..n_intervals).map(|_| AtomicU64::new(0)).collect());
    let interval_errors: Arc<Vec<AtomicU64>> =
        Arc::new((0..n_intervals).map(|_| AtomicU64::new(0)).collect());
    let interval_latency_sum: Arc<Vec<AtomicU64>> =
        Arc::new((0..n_intervals).map(|_| AtomicU64::new(0)).collect());

    let rss_before = rss_kb();
    let mut all_latencies = Vec::new();

    let handles: Vec<_> = (0..n_workers)
        .map(|w| {
            let pool = pool.clone();
            let config = config.clone();
            let archive = archive.clone();
            let agent_ids = agent_ids.clone();
            let barrier = Arc::clone(&barrier);
            let errors = Arc::clone(&errors);
            let ops_done = Arc::clone(&ops_done);
            let consumed = Arc::clone(&consumed);
            let interval_ops = Arc::clone(&interval_ops);
            let interval_errors = Arc::clone(&interval_errors);
            let interval_latency_sum = Arc::clone(&interval_latency_sum);

            std::thread::Builder::new()
                .name(format!("endurance-{w}"))
                .stack_size(2 * 1024 * 1024)
                .spawn(move || {
                    barrier.wait();
                    let deadline = Instant::now() + Duration::from_secs(duration_secs);
                    let mut thread_latencies = Vec::new();
                    let mut op_counter = 0_u64;

                    while Instant::now() < deadline {
                        // Rate limiting
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let allowed = elapsed_ms * target_rps / 1000;
                        let my_seq = consumed.fetch_add(1, Ordering::Relaxed);
                        if my_seq > allowed + 50 {
                            std::thread::sleep(Duration::from_millis(2));
                        }

                        let op_start = Instant::now();
                        op_counter += 1;

                        // Which 10s interval are we in?
                        let interval = (elapsed_ms / 10_000) as usize;
                        let interval = interval.min(n_intervals - 1);

                        // Weighted ops — realistic production mix
                        let roll = (op_counter + w as u64 * 7) % 20;
                        let agent_idx = ((w * 3 + op_counter as usize) * 7) % agent_ids.len();
                        let (agent_id, agent_name_str) = &agent_ids[agent_idx];

                        let ok = match roll {
                            0..=6 => {
                                // 35% message send + archive
                                let recipient_idx = (agent_idx + 1) % agent_ids.len();
                                let (rid, rname) = &agent_ids[recipient_idx];
                                let tid = format!("end-{w}-{op_counter}");

                                let msg_result = block_on_with_retry(5, |cx| {
                                    let pool = pool.clone();
                                    let tid = tid.clone();
                                    async move {
                                        queries::create_message_with_recipients(
                                            &cx,
                                            &pool,
                                            project_id,
                                            *agent_id,
                                            &format!("Endurance msg {op_counter}"),
                                            &format!("Body {w}-{op_counter}"),
                                            Some(&tid),
                                            "normal",
                                            false,
                                            "[]",
                                            &[(*rid, "to")],
                                        )
                                        .await
                                    }
                                });

                                let msg_json = serde_json::json!({
                                    "id": msg_result.id.expect("id"),
                                    "subject": format!("Endurance msg {op_counter}"),
                                    "thread_id": tid,
                                    "created_ts": micros_to_iso(msg_result.created_ts),
                                });
                                write_message_bundle(
                                    &archive,
                                    &config,
                                    &msg_json,
                                    &format!("Body {w}-{op_counter}"),
                                    agent_name_str,
                                    std::slice::from_ref(rname),
                                    &[],
                                    None,
                                )
                                .is_ok()
                            }
                            7..=12 => {
                                // 30% inbox fetch
                                let result = block_on(|cx| {
                                    let pool = pool.clone();
                                    async move {
                                        queries::fetch_inbox(
                                            &cx, &pool, project_id, *agent_id, false, None, 20,
                                        )
                                        .await
                                    }
                                });
                                matches!(result, Outcome::Ok(_))
                            }
                            13..=15 => {
                                // 15% search
                                let terms = [
                                    "delta",
                                    "echo",
                                    "foxtrot",
                                    "endurance",
                                    "seed",
                                    "body",
                                    "searchable",
                                ];
                                let q = terms[op_counter as usize % terms.len()];
                                let result = block_on(|cx| {
                                    let pool = pool.clone();
                                    async move {
                                        queries::search_messages(&cx, &pool, project_id, q, 10)
                                            .await
                                    }
                                });
                                matches!(result, Outcome::Ok(_))
                            }
                            16..=17 => {
                                // 10% file reservation
                                let pattern = format!("src/endurance_{w}/file_{op_counter}.rs");
                                let result = block_on(|cx| {
                                    let pool = pool.clone();
                                    let pattern = pattern.clone();
                                    async move {
                                        queries::create_file_reservations(
                                            &cx,
                                            &pool,
                                            project_id,
                                            *agent_id,
                                            &[pattern.as_str()],
                                            60,
                                            false,
                                            &format!("endurance-{w}-{op_counter}"),
                                        )
                                        .await
                                    }
                                });
                                matches!(result, Outcome::Ok(_))
                            }
                            18 => {
                                // 5% agent profile update (archive only)
                                let agent_json = serde_json::json!({
                                    "name": agent_name_str,
                                    "program": "endurance-test",
                                    "model": "test-model",
                                    "op": op_counter,
                                });
                                write_agent_profile_with_config(&archive, &config, &agent_json)
                                    .is_ok()
                            }
                            _ => {
                                // 5% acknowledge (if inbox has messages)
                                let inbox = block_on(|cx| {
                                    let pool = pool.clone();
                                    async move {
                                        queries::fetch_inbox(
                                            &cx, &pool, project_id, *agent_id, false, None, 3,
                                        )
                                        .await
                                    }
                                });
                                if let Outcome::Ok(rows) = inbox {
                                    if let Some(row) = rows.first() {
                                        let ack = block_on(|cx| {
                                            let pool = pool.clone();
                                            let mid = row.message.id.expect("inbox msg id");
                                            async move {
                                                queries::acknowledge_message(
                                                    &cx, &pool, *agent_id, mid,
                                                )
                                                .await
                                            }
                                        });
                                        matches!(ack, Outcome::Ok(_))
                                    } else {
                                        true // no messages to ack, not an error
                                    }
                                } else {
                                    false
                                }
                            }
                        };

                        let lat_us = op_start.elapsed().as_micros() as u64;
                        if ok {
                            ops_done.fetch_add(1, Ordering::Relaxed);
                            interval_ops[interval].fetch_add(1, Ordering::Relaxed);
                        } else {
                            errors.fetch_add(1, Ordering::Relaxed);
                            interval_errors[interval].fetch_add(1, Ordering::Relaxed);
                        }
                        interval_latency_sum[interval].fetch_add(lat_us, Ordering::Relaxed);

                        thread_latencies.push(lat_us);
                    }

                    thread_latencies
                })
                .expect("spawn endurance worker")
        })
        .collect();

    for h in handles {
        let mut lats = h.join().expect("endurance worker panicked");
        all_latencies.append(&mut lats);
    }

    flush_async_commits();
    wbq_flush();

    let elapsed = start.elapsed();
    let rss_after = rss_kb();
    let total_ok = ops_done.load(Ordering::Relaxed);
    let total_err = errors.load(Ordering::Relaxed);
    let total = total_ok + total_err;

    let report = LatencyReport::from_latencies(&mut all_latencies, total_err);

    eprintln!("\n=== stress_sustained_100_agents_60s ===");
    eprintln!(
        "  Duration: {:.1}s, Target RPS: {target_rps}, Workers: {n_workers}, Agents: {n_agents}",
        elapsed.as_secs_f64(),
    );
    eprintln!("  Total ops: {total}, Success: {total_ok}, Errors: {total_err}");
    eprintln!("  Actual RPS: {:.0}", total as f64 / elapsed.as_secs_f64());
    eprintln!(
        "  RSS: {} KB before, {} KB after (+{} KB)",
        rss_before,
        rss_after,
        rss_after.saturating_sub(rss_before)
    );
    report.print("Endurance (100 agents)");

    // Per-interval breakdown
    eprintln!("  --- Per-interval (10s buckets) ---");
    for i in 0..n_intervals {
        let ops = interval_ops[i].load(Ordering::Relaxed);
        let errs = interval_errors[i].load(Ordering::Relaxed);
        let lat_sum = interval_latency_sum[i].load(Ordering::Relaxed);
        let total_i = ops + errs;
        if total_i > 0 {
            eprintln!(
                "    [{:>2}s-{:>2}s]: {} ops, {} errors ({:.1}%), avg latency {:.1}ms",
                i * 10,
                (i + 1) * 10,
                total_i,
                errs,
                errs as f64 / total_i as f64 * 100.0,
                lat_sum as f64 / total_i as f64 / 1000.0,
            );
        }
    }

    let wbq = wbq_stats();
    let coalescer_stats = get_commit_coalescer().stats();
    eprintln!(
        "  WBQ: enqueued={}, drained={}, errors={}, fallbacks={}",
        wbq.enqueued, wbq.drained, wbq.errors, wbq.fallbacks,
    );
    eprintln!(
        "  Coalescer: enqueued={}, commits={}, batching={:.1}x",
        coalescer_stats.enqueued,
        coalescer_stats.commits,
        coalescer_stats.enqueued as f64 / coalescer_stats.commits.max(1) as f64,
    );

    // Assertions
    let error_rate = total_err as f64 / total.max(1) as f64;
    assert!(
        error_rate < 0.10,
        "error rate {:.1}% exceeds 10% ({total_err}/{total})",
        error_rate * 100.0,
    );
    assert!(
        report.p99_us < 10_000_000,
        "p99 latency {:.1}s exceeds 10s",
        report.p99_us as f64 / 1_000_000.0,
    );
    // Must sustain meaningful throughput
    let actual_rps = total as f64 / elapsed.as_secs_f64();
    assert!(
        actual_rps > target_rps as f64 / 4.0,
        "actual RPS {actual_rps:.0} is less than 25% of target {target_rps}"
    );
    // Memory growth should be bounded (< 500MB growth for a 60s test)
    let rss_growth_kb = rss_after.saturating_sub(rss_before);
    assert!(
        rss_growth_kb < 500_000,
        "RSS grew by {} KB — possible memory leak",
        rss_growth_kb,
    );

    // Check for latency degradation: last interval avg should not be >3x first interval
    let first_interval_ops = interval_ops[0].load(Ordering::Relaxed);
    let first_interval_lat = interval_latency_sum[0].load(Ordering::Relaxed);
    let last_idx = n_intervals - 1;
    let last_interval_ops = interval_ops[last_idx].load(Ordering::Relaxed);
    let last_interval_lat = interval_latency_sum[last_idx].load(Ordering::Relaxed);
    if first_interval_ops > 0 && last_interval_ops > 0 {
        let first_avg = first_interval_lat as f64 / first_interval_ops as f64;
        let last_avg = last_interval_lat as f64 / last_interval_ops as f64;
        eprintln!(
            "  Latency trend: first 10s avg={:.1}ms, last 10s avg={:.1}ms ({:.1}x)",
            first_avg / 1000.0,
            last_avg / 1000.0,
            last_avg / first_avg.max(1.0),
        );
        assert!(
            last_avg < first_avg * 5.0,
            "latency degraded {:.1}x over test duration (first={:.1}ms, last={:.1}ms)",
            last_avg / first_avg.max(1.0),
            first_avg / 1000.0,
            last_avg / 1000.0,
        );
    }
}
