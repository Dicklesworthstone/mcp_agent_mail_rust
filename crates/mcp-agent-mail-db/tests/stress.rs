//! Stress tests proving correctness under concurrent load.
//!
//! These tests verify that the DB layer handles concurrent operations correctly:
//! - No lost writes under contention
//! - No deadlocks with multiple concurrent agents
//! - Cache coherency under concurrent read/write
//! - Deferred touch batching correctness
//! - No data corruption with overlapping transactions

#![allow(
    clippy::needless_collect,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::manual_let_else
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("stress_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 20,
        min_connections: 4,
        acquire_timeout_ms: 60_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

/// Run an async closure in its own runtime on the current thread.
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

/// Retry an async operation up to `max_retries` times on transient `SQLite` lock errors.
/// Pool init can race under extreme contention (multiple connections
/// simultaneously running `PRAGMA` + `CREATE TABLE`), producing `SQLITE_BUSY`
/// before `busy_timeout` is established on the new connection.
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
                    std::thread::sleep(std::time::Duration::from_millis(10 * (attempt as u64 + 1)));
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

// =============================================================================
// Test: Concurrent ensure_project (idempotent under contention)
// =============================================================================

#[test]
fn stress_concurrent_ensure_project() {
    let (pool, _dir) = make_pool();
    let n_threads = 8;
    let barrier = Arc::new(Barrier::new(n_threads));
    let human_key = format!("/data/stress/proj_{}", unique_suffix());

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let key = human_key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let row = block_on_with_retry(3, |cx| {
                    let p = pool.clone();
                    let k = key.clone();
                    async move { queries::ensure_project(&cx, &p, &k).await }
                });
                assert!(!row.slug.is_empty());
                row.id.expect("project should have an id")
            })
        })
        .collect();

    let ids: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All threads should get the same project ID (idempotent creation)
    let first = ids[0];
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            *id, first,
            "thread {i} got different project id {id} vs {first}"
        );
    }
}

// =============================================================================
// Test: Concurrent register_agent (no duplicate, last writer wins)
// =============================================================================

#[test]
fn stress_concurrent_register_agent() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/agents_{suffix}");

    let project_id = {
        let p = pool.clone();
        block_on(|cx| async move {
            match queries::ensure_project(&cx, &p, &human_key).await {
                Outcome::Ok(row) => row.id.unwrap(),
                _ => panic!("ensure_project failed"),
            }
        })
    };

    let n_threads = 8;
    let barrier = Arc::new(Barrier::new(n_threads));

    let handles: Vec<_> = (0..n_threads)
        .map(|i| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                block_on(|cx| async move {
                    match queries::register_agent(
                        &cx,
                        &pool,
                        project_id,
                        "BoldCastle",
                        "test-prog",
                        &format!("model-{i}"),
                        Some(&format!("task from thread {i}")),
                        None,
                    )
                    .await
                    {
                        Outcome::Ok(row) => row,
                        Outcome::Err(e) => {
                            panic!("register_agent thread {i} failed: {e:?}")
                        }
                        _ => panic!("unexpected outcome"),
                    }
                })
            })
        })
        .collect();

    let agents: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All should return the same agent ID (idempotent register)
    let first_id = agents[0].id;
    for (i, agent) in agents.iter().enumerate() {
        assert_eq!(
            agent.id, first_id,
            "thread {i} got different agent id {:?} vs {:?}",
            agent.id, first_id
        );
        assert_eq!(agent.name, "BoldCastle");
    }

    // Verify only one agent row in DB
    let all_agents = {
        let p = pool;
        block_on(|cx| async move {
            match queries::list_agents(&cx, &p, project_id).await {
                Outcome::Ok(rows) => rows,
                _ => panic!("list_agents failed"),
            }
        })
    };
    assert_eq!(
        all_agents.len(),
        1,
        "should have exactly 1 agent, got {}",
        all_agents.len()
    );
}

// =============================================================================
// Test: Concurrent message sending (no lost writes)
// =============================================================================

#[test]
fn stress_concurrent_message_sending() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/msgs_{suffix}");

    let (project_id, sender_id) = {
        let p = pool.clone();
        block_on(|cx| async move {
            let proj = match queries::ensure_project(&cx, &p, &human_key).await {
                Outcome::Ok(r) => r,
                _ => panic!("ensure_project failed"),
            };
            let pid = proj.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &p,
                pid,
                "SwiftFalcon",
                "test",
                "test",
                None,
                None,
            )
            .await
            {
                Outcome::Ok(r) => r,
                _ => panic!("register sender failed"),
            };

            (pid, sender.id.unwrap())
        })
    };

    let n_threads = 8;
    let msgs_per_thread = 10;
    let barrier = Arc::new(Barrier::new(n_threads));

    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let mut created_ids = Vec::new();
                for m in 0..msgs_per_thread {
                    let msg_id = {
                        let p = pool.clone();
                        block_on(|cx| async move {
                            match queries::create_message(
                                &cx,
                                &p,
                                project_id,
                                sender_id,
                                &format!("Msg from t{t} #{m}"),
                                &format!("Body from thread {t}, message {m}"),
                                None,
                                "normal",
                                false,
                                "",
                            )
                            .await
                            {
                                Outcome::Ok(row) => row.id.unwrap(),
                                Outcome::Err(e) => {
                                    panic!("create_message t{t} m{m} failed: {e:?}")
                                }
                                _ => panic!("unexpected outcome"),
                            }
                        })
                    };
                    created_ids.push(msg_id);
                }
                created_ids
            })
        })
        .collect();

    let all_ids: Vec<i64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();

    // All IDs should be unique (no overwrites)
    let expected = n_threads * msgs_per_thread;
    assert_eq!(
        all_ids.len(),
        expected,
        "expected {expected} messages, got {}",
        all_ids.len()
    );

    let mut sorted = all_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        expected,
        "duplicate message IDs detected: {} unique out of {expected}",
        sorted.len()
    );

    // Verify all messages are retrievable
    for &id in &all_ids {
        let p = pool.clone();
        block_on(|cx| async move {
            match queries::get_message(&cx, &p, id).await {
                Outcome::Ok(msg) => {
                    assert_eq!(msg.project_id, project_id);
                    assert_eq!(msg.sender_id, sender_id);
                }
                Outcome::Err(e) => panic!("get_message({id}) failed: {e:?}"),
                _ => panic!("unexpected outcome"),
            }
        });
    }
}

// =============================================================================
// Test: Concurrent file reservation creation (both succeed, advisory)
// =============================================================================

#[test]
fn stress_concurrent_file_reservations() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/locks_{suffix}");

    let (project_id, agent1_id, agent2_id) = {
        let p = pool.clone();
        block_on(|cx| async move {
            let proj = match queries::ensure_project(&cx, &p, &human_key).await {
                Outcome::Ok(r) => r,
                _ => panic!("ensure_project failed"),
            };
            let pid = proj.id.unwrap();

            let a1 = match queries::register_agent(
                &cx,
                &p,
                pid,
                "GreenPeak",
                "test",
                "test",
                None,
                None,
            )
            .await
            {
                Outcome::Ok(r) => r,
                _ => panic!("register agent1 failed"),
            };

            let a2 =
                match queries::register_agent(&cx, &p, pid, "BluePond", "test", "test", None, None)
                    .await
                {
                    Outcome::Ok(r) => r,
                    _ => panic!("register agent2 failed"),
                };

            (pid, a1.id.unwrap(), a2.id.unwrap())
        })
    };

    let barrier = Arc::new(Barrier::new(2));

    let pool1 = pool.clone();
    let barrier1 = Arc::clone(&barrier);
    let h1 = std::thread::spawn(move || {
        barrier1.wait();
        block_on(|cx| async move {
            queries::create_file_reservations(
                &cx,
                &pool1,
                project_id,
                agent1_id,
                &["src/main.rs"],
                3600,
                true,
                "agent1 edit",
            )
            .await
        })
    });

    let pool2 = pool;
    let barrier2 = Arc::clone(&barrier);
    let h2 = std::thread::spawn(move || {
        barrier2.wait();
        block_on(|cx| async move {
            queries::create_file_reservations(
                &cx,
                &pool2,
                project_id,
                agent2_id,
                &["src/main.rs"],
                3600,
                true,
                "agent2 edit",
            )
            .await
        })
    });

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    match (r1, r2) {
        (Outcome::Ok(res1), Outcome::Ok(res2)) => {
            let total = res1.len() + res2.len();
            assert!(total >= 2, "both agents should get reservation records");
        }
        (Outcome::Err(e), _) => panic!("agent1 reservation failed: {e:?}"),
        (_, Outcome::Err(e)) => panic!("agent2 reservation failed: {e:?}"),
        _ => panic!("unexpected outcome"),
    }
}

// =============================================================================
// Test: Deferred touch batching under concurrent load
// =============================================================================

#[test]
fn stress_deferred_touch_batch_correctness() {
    let cache = mcp_agent_mail_db::cache::read_cache();
    let n_threads: usize = 16;
    let touches_per_thread: usize = 100;
    let barrier = Arc::new(Barrier::new(n_threads));
    let n_agents: i64 = 4;
    let base_id: i64 = 900_000 + unique_suffix() as i64 * 100;

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let cache = mcp_agent_mail_db::cache::read_cache();
                for i in 0..touches_per_thread {
                    let agent_id = base_id + (i % n_agents as usize) as i64;
                    let ts = (i * 1000 + 500) as i64;
                    cache.enqueue_touch(agent_id, ts);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let drained = cache.drain_touches();

    let our_entries: std::collections::HashMap<i64, i64> = drained
        .into_iter()
        .filter(|(k, _)| *k >= base_id && *k < base_id + n_agents)
        .collect();

    assert_eq!(
        our_entries.len(),
        n_agents as usize,
        "expected {n_agents} agent entries after coalescing, got {}",
        our_entries.len()
    );

    for agent_id in base_id..base_id + n_agents {
        assert!(
            our_entries.contains_key(&agent_id),
            "missing agent_id {agent_id}"
        );
        let ts = our_entries[&agent_id];
        assert!(ts > 0, "timestamp for agent {agent_id} should be positive");
    }
}

// =============================================================================
// Test: Cache coherency under mixed read/write workload
// =============================================================================

#[test]
fn stress_cache_coherency_mixed_workload() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/cache_{suffix}");

    let project_id = {
        let p = pool.clone();
        block_on(|cx| async move {
            match queries::ensure_project(&cx, &p, &human_key).await {
                Outcome::Ok(row) => row.id.unwrap(),
                _ => panic!("ensure_project failed"),
            }
        })
    };

    let agent_names: Vec<&str> = vec![
        "BoldCastle",
        "CalmRiver",
        "DarkForest",
        "AmberPeak",
        "FrostyLake",
        "GoldCreek",
        "MistyCave",
        "CopperRidge",
        "JadeMountain",
        "TealHawk",
    ];
    let n_agents = agent_names.len();

    for name in &agent_names {
        let p = pool.clone();
        block_on(|cx| async move {
            match queries::register_agent(
                &cx,
                &p,
                project_id,
                name,
                "test",
                "test",
                Some("initial"),
                None,
            )
            .await
            {
                Outcome::Ok(_) => {}
                Outcome::Err(e) => panic!("register {name} failed: {e:?}"),
                _ => panic!("unexpected outcome"),
            }
        });
    }

    let n_readers: usize = 8;
    let n_writers: usize = 4;
    let iterations: usize = 20;
    let barrier = Arc::new(Barrier::new(n_readers + n_writers));

    let writer_handles: Vec<_> = (0..n_writers)
        .map(|w| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let names: Vec<String> = agent_names
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..iterations {
                    let name = names[i % n_agents].clone();
                    let p = pool.clone();
                    block_on(|cx| async move {
                        let _ = queries::register_agent(
                            &cx,
                            &p,
                            project_id,
                            &name,
                            "test",
                            &format!("model-w{w}-i{i}"),
                            Some(&format!("task from writer {w} iter {i}")),
                            None,
                        )
                        .await;
                    });
                }
            })
        })
        .collect();

    let reader_handles: Vec<_> = (0..n_readers)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let names: Vec<String> = agent_names
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            std::thread::spawn(move || {
                barrier.wait();
                let mut reads = 0u64;
                for i in 0..iterations {
                    let name = names[i % n_agents].clone();
                    let p = pool.clone();
                    block_on(|cx| async move {
                        match queries::get_agent(&cx, &p, project_id, &name).await {
                            Outcome::Ok(agent) => {
                                assert_eq!(agent.name, name);
                            }
                            Outcome::Err(e) => {
                                panic!("get_agent({name}) failed: {e:?}");
                            }
                            _ => panic!("unexpected outcome"),
                        }
                    });
                    reads += 1;
                }
                reads
            })
        })
        .collect();

    for h in writer_handles {
        h.join().unwrap();
    }
    let total_reads: u64 = reader_handles.into_iter().map(|h| h.join().unwrap()).sum();

    assert_eq!(
        total_reads,
        (n_readers as u64) * (iterations as u64),
        "all reads should succeed"
    );
}

// =============================================================================
// Test: Concurrent inbox fetch + message creation (read-write overlap)
// =============================================================================

#[test]
fn stress_concurrent_inbox_and_send() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/inbox_{suffix}");

    let (project_id, sender_id, receiver_id) = {
        let p = pool.clone();
        block_on(|cx| async move {
            let proj = match queries::ensure_project(&cx, &p, &human_key).await {
                Outcome::Ok(r) => r,
                _ => panic!("ensure_project failed"),
            };
            let pid = proj.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &p,
                pid,
                "SwiftEagle",
                "test",
                "test",
                None,
                None,
            )
            .await
            {
                Outcome::Ok(r) => r,
                _ => panic!("register sender failed"),
            };

            let receiver =
                match queries::register_agent(&cx, &p, pid, "DarkBay", "test", "test", None, None)
                    .await
                {
                    Outcome::Ok(r) => r,
                    _ => panic!("register receiver failed"),
                };

            (pid, sender.id.unwrap(), receiver.id.unwrap())
        })
    };

    let n_senders: usize = 4;
    let n_readers: usize = 4;
    let msgs_per_sender: usize = 10;
    let barrier = Arc::new(Barrier::new(n_senders + n_readers));

    let send_handles: Vec<_> = (0..n_senders)
        .map(|s| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for m in 0..msgs_per_sender {
                    let p = pool.clone();
                    block_on(|cx| async move {
                        let msg = match queries::create_message(
                            &cx,
                            &p,
                            project_id,
                            sender_id,
                            &format!("From sender {s} msg {m}"),
                            &format!("Body {s}-{m}"),
                            None,
                            "normal",
                            false,
                            "",
                        )
                        .await
                        {
                            Outcome::Ok(r) => r,
                            Outcome::Err(e) => {
                                panic!("create_message failed: {e:?}")
                            }
                            _ => panic!("unexpected"),
                        };

                        let _ = queries::add_recipients(
                            &cx,
                            &p,
                            msg.id.unwrap(),
                            &[(receiver_id, "to")],
                        )
                        .await;
                    });
                }
            })
        })
        .collect();

    let reader_handles: Vec<_> = (0..n_readers)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..msgs_per_sender {
                    let p = pool.clone();
                    block_on(|cx| async move {
                        match queries::fetch_inbox(
                            &cx,
                            &p,
                            project_id,
                            receiver_id,
                            false,
                            None,
                            50,
                        )
                        .await
                        {
                            Outcome::Ok(_) => {}
                            Outcome::Err(e) => {
                                panic!("fetch_inbox failed: {e:?}")
                            }
                            _ => panic!("unexpected"),
                        }
                    });
                }
            })
        })
        .collect();

    for h in send_handles {
        h.join().unwrap();
    }
    for h in reader_handles {
        h.join().unwrap();
    }

    // Final count
    let final_count = {
        let p = pool;
        block_on(|cx| async move {
            match queries::fetch_inbox(&cx, &p, project_id, receiver_id, false, None, 200).await {
                Outcome::Ok(msgs) => msgs.len(),
                _ => panic!("final fetch_inbox failed"),
            }
        })
    };

    let expected = n_senders * msgs_per_sender;
    assert_eq!(
        final_count, expected,
        "expected {expected} messages in inbox, got {final_count}"
    );
}

// =============================================================================
// Test: Concurrent mark_read + acknowledge (idempotent, no race corruption)
// =============================================================================

#[test]
fn stress_concurrent_read_ack() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/ack_{suffix}");

    let (project_id, receiver_id, msg_id) = {
        let p = pool.clone();
        block_on(|cx| async move {
            let proj = match queries::ensure_project(&cx, &p, &human_key).await {
                Outcome::Ok(r) => r,
                _ => panic!("ensure_project failed"),
            };
            let pid = proj.id.unwrap();

            let sender =
                match queries::register_agent(&cx, &p, pid, "BoldFox", "test", "test", None, None)
                    .await
                {
                    Outcome::Ok(r) => r,
                    _ => panic!("register sender failed"),
                };

            let receiver =
                match queries::register_agent(&cx, &p, pid, "QuietOwl", "test", "test", None, None)
                    .await
                {
                    Outcome::Ok(r) => r,
                    _ => panic!("register receiver failed"),
                };

            let msg = match queries::create_message(
                &cx,
                &p,
                pid,
                sender.id.unwrap(),
                "Test ack race",
                "Body",
                None,
                "normal",
                true,
                "",
            )
            .await
            {
                Outcome::Ok(r) => r,
                _ => panic!("create_message failed"),
            };

            let _ =
                queries::add_recipients(&cx, &p, msg.id.unwrap(), &[(receiver.id.unwrap(), "to")])
                    .await;

            (pid, receiver.id.unwrap(), msg.id.unwrap())
        })
    };

    let n_threads = 8;
    let barrier = Arc::new(Barrier::new(n_threads));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                block_on(|cx| async move {
                    let _ = queries::mark_message_read(&cx, &pool, receiver_id, msg_id).await;
                    let _ = queries::acknowledge_message(&cx, &pool, receiver_id, msg_id).await;
                });
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Verify the message was read and acknowledged (idempotent)
    let p = pool;
    block_on(|cx| async move {
        match queries::fetch_inbox(&cx, &p, project_id, receiver_id, false, None, 50).await {
            Outcome::Ok(msgs) => {
                assert_eq!(msgs.len(), 1, "should have exactly 1 message in inbox");
                let m = &msgs[0];
                assert!(m.ack_ts.is_some(), "message should be acknowledged");
            }
            Outcome::Err(e) => panic!("fetch_inbox failed: {e:?}"),
            _ => panic!("unexpected"),
        }
    });
}

// =============================================================================
// Test: Pool exhaustion recovery (all connections busy → wait → succeed)
// =============================================================================

#[test]
fn stress_pool_exhaustion_recovery() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("pool_exhaust_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 3,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
    };
    let pool = DbPool::new(&config).expect("create pool");
    std::mem::forget(dir);

    let human_key = format!("/data/stress/pool_exhaust_{}", unique_suffix());
    {
        let p = pool.clone();
        let key = human_key.clone();
        block_on(|cx| async move {
            let _ = queries::ensure_project(&cx, &p, &key).await;
        });
    }

    let n_threads: usize = 12;
    let barrier = Arc::new(Barrier::new(n_threads));
    let success_count = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let success = Arc::clone(&success_count);
            let key = human_key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                block_on(|cx| async move {
                    match queries::ensure_project(&cx, &pool, &key).await {
                        Outcome::Ok(_) => {
                            success.fetch_add(1, Ordering::Relaxed);
                        }
                        Outcome::Err(e) => {
                            panic!(
                                "pool exhaustion should not cause failure with 30s timeout: {e:?}"
                            );
                        }
                        _ => {}
                    }
                });
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        success_count.load(Ordering::Relaxed),
        n_threads as u64,
        "all threads should succeed despite pool contention"
    );
}

// ---------------------------------------------------------------------------
// Integration test: fetch_unacked_for_agent
// ---------------------------------------------------------------------------

/// Verifies that `fetch_unacked_for_agent` returns ack-required messages
/// that have not been acknowledged, and excludes them once acknowledged.
#[test]
fn fetch_unacked_returns_pending_and_excludes_acknowledged() {
    let (pool, _dir) = make_pool();

    block_on(|cx| async move {
        // Setup: project + two agents
        let proj = match queries::ensure_project(&cx, &pool, "/data/test-unacked").await {
            Outcome::Ok(p) => p,
            _ => panic!("ensure_project failed"),
        };
        let pid = proj.id.unwrap();

        let sender =
            match queries::register_agent(&cx, &pool, pid, "GreenElk", "test", "test", None, None)
                .await
            {
                Outcome::Ok(a) => a,
                Outcome::Err(e) => panic!("register sender failed: {e:?}"),
                _ => panic!("register sender: unexpected outcome"),
            };
        let receiver =
            match queries::register_agent(&cx, &pool, pid, "BlueDeer", "test", "test", None, None)
                .await
            {
                Outcome::Ok(a) => a,
                Outcome::Err(e) => panic!("register receiver failed: {e:?}"),
                _ => panic!("register receiver: unexpected outcome"),
            };

        let sender_id = sender.id.unwrap();
        let receiver_id = receiver.id.unwrap();

        // Create an ack-required message with recipient
        let msg = match queries::create_message_with_recipients(
            &cx,
            &pool,
            pid,
            sender_id,
            "Please ack this",
            "Test body",
            None,
            "normal",
            true, // ack_required
            "",
            &[(receiver_id, "to")],
        )
        .await
        {
            Outcome::Ok(m) => m,
            _ => panic!("create_message_with_recipients failed"),
        };
        let msg_id = msg.id.unwrap();

        // Also create a non-ack message (should NOT appear)
        match queries::create_message_with_recipients(
            &cx,
            &pool,
            pid,
            sender_id,
            "No ack needed",
            "Body 2",
            None,
            "normal",
            false, // ack_required = false
            "",
            &[(receiver_id, "to")],
        )
        .await
        {
            Outcome::Ok(_) => {}
            _ => panic!("create non-ack message failed"),
        }

        // Fetch unacked: should return exactly the ack-required message
        let unacked = match queries::fetch_unacked_for_agent(&cx, &pool, pid, receiver_id, 50).await
        {
            Outcome::Ok(rows) => rows,
            _ => panic!("fetch_unacked_for_agent failed"),
        };

        assert_eq!(unacked.len(), 1, "expected 1 unacked message");
        assert_eq!(unacked[0].message.id, Some(msg_id));
        assert_eq!(unacked[0].sender_name, "GreenElk");
        assert_eq!(unacked[0].kind, "to");
        assert!(unacked[0].read_ts.is_none());

        // Acknowledge the message
        match queries::acknowledge_message(&cx, &pool, receiver_id, msg_id).await {
            Outcome::Ok(_) => {}
            _ => panic!("acknowledge_message failed"),
        }

        // Fetch unacked again: should now be empty
        let unacked_after =
            match queries::fetch_unacked_for_agent(&cx, &pool, pid, receiver_id, 50).await {
                Outcome::Ok(rows) => rows,
                _ => panic!("fetch_unacked_for_agent after ack failed"),
            };

        assert!(
            unacked_after.is_empty(),
            "expected 0 unacked messages after acknowledgement, got {}",
            unacked_after.len()
        );
    });
}
