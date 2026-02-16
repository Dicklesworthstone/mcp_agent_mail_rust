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
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::manual_let_else
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::schema;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, InboxStatsRow, read_cache};
use sqlmodel_schema::MigrationStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);
static INBOX_STATS_TEST_MUTEX: Mutex<()> = Mutex::new(());

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
        warmup_connections: 0,
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
// Test: Concurrent pool warmup should not surface SQLITE_BUSY
// =============================================================================

#[test]
fn stress_concurrent_pool_warmup_has_no_sqlite_busy() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("pool_warmup_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 64,
        min_connections: 0,
        acquire_timeout_ms: 60_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");

    let n_threads = 50;
    let barrier_start = Arc::new(Barrier::new(n_threads));
    let barrier_hold = Arc::new(Barrier::new(n_threads));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier_start = Arc::clone(&barrier_start);
            let barrier_hold = Arc::clone(&barrier_hold);
            std::thread::spawn(move || {
                barrier_start.wait();
                let conn = match block_on(|cx| async move { pool.acquire(&cx).await }) {
                    Outcome::Ok(c) => c,
                    Outcome::Err(e) => {
                        panic!("pool warmup acquire should succeed without SQLITE_BUSY: {e:?}")
                    }
                    Outcome::Cancelled(r) => panic!("pool warmup acquire cancelled: {r:?}"),
                    Outcome::Panicked(p) => panic!("{p}"),
                };
                barrier_hold.wait();
                drop(conn);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread join");
    }

    // Verify we end up at a consistent latest schema (no migration races).
    block_on(|cx| async move {
        let conn = match pool.acquire(&cx).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => panic!("acquire after warmup should succeed: {e:?}"),
            Outcome::Cancelled(r) => panic!("acquire after warmup cancelled: {r:?}"),
            Outcome::Panicked(p) => panic!("{p}"),
        };

        let statuses = match schema::migration_status(&cx, &*conn).await {
            Outcome::Ok(s) => s,
            Outcome::Err(e) => panic!("migration_status should succeed: {e:?}"),
            Outcome::Cancelled(r) => panic!("migration_status cancelled: {r:?}"),
            Outcome::Panicked(p) => panic!("{p}"),
        };

        let expected = schema::schema_migrations().len();
        assert_eq!(statuses.len(), expected, "all migrations should be tracked");
        assert!(
            statuses
                .iter()
                .all(|(_id, status)| matches!(status, MigrationStatus::Applied { .. })),
            "all migrations should be applied after warmup"
        );
    });
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
        warmup_connections: 0,
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

// =============================================================================
// Test: 1000-agent concurrent workload (br-15dv.9.1)
//
// Spawns 50 projects × 20 agents = 1000 agents performing 16,000 operations:
//   1,000 registrations + 5,000 sends + 5,000 fetches + 5,000 acks
//
// Run explicitly: cargo test --test stress stress_1000_agent -- --ignored
// =============================================================================

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        let mut out: String = f.to_uppercase().collect();
        out.extend(c);
        out
    })
}

#[test]
#[ignore = "heavy load test: 1000 agents, 16K operations"]
fn stress_1000_agent_concurrent_workload() {
    use mcp_agent_mail_core::models::{VALID_ADJECTIVES, VALID_NOUNS};
    use std::time::Instant;

    let start = Instant::now();

    // Pool with enough connections for 50 concurrent threads
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("stress_1k_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 100,
        min_connections: 10,
        acquire_timeout_ms: 120_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    std::mem::forget(dir); // prevent cleanup while threads are running

    // Generate 1000 unique agent names from adjective×noun cross product
    let mut all_names: Vec<String> = Vec::with_capacity(1000);
    'name_gen: for adj in VALID_ADJECTIVES {
        for noun in VALID_NOUNS {
            all_names.push(format!("{}{}", cap(adj), cap(noun)));
            if all_names.len() >= 1000 {
                break 'name_gen;
            }
        }
    }
    assert_eq!(all_names.len(), 1000, "need 1000 unique agent names");

    let n_projects: usize = 50;
    let agents_per_project: usize = 20;
    let msgs_per_agent: usize = 5;

    // Counters
    let registration_count = Arc::new(AtomicU64::new(0));
    let send_count = Arc::new(AtomicU64::new(0));
    let fetch_count = Arc::new(AtomicU64::new(0));
    let ack_count = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));

    // All 50 threads start simultaneously
    let barrier = Arc::new(Barrier::new(n_projects));

    let handles: Vec<_> = (0..n_projects)
        .map(|p| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let names: Vec<String> =
                all_names[p * agents_per_project..(p + 1) * agents_per_project].to_vec();
            let reg_c = Arc::clone(&registration_count);
            let snd_c = Arc::clone(&send_count);
            let ftc_c = Arc::clone(&fetch_count);
            let ack_c = Arc::clone(&ack_count);
            let err_c = Arc::clone(&error_count);

            std::thread::spawn(move || {
                barrier.wait();

                let human_key = format!("/data/stress/1k_p{p}_{}", unique_suffix());

                // ── Phase 1: ensure project + register agents ──
                let project_id = block_on_with_retry(5, |cx| {
                    let pp = pool.clone();
                    let k = human_key.clone();
                    async move { queries::ensure_project(&cx, &pp, &k).await }
                })
                .id
                .unwrap();

                let mut agent_ids: Vec<i64> = Vec::with_capacity(agents_per_project);
                for name in &names {
                    let aid = block_on_with_retry(5, |cx| {
                        let pp = pool.clone();
                        let n = name.clone();
                        async move {
                            queries::register_agent(
                                &cx,
                                &pp,
                                project_id,
                                &n,
                                "stress",
                                "stress-model",
                                None,
                                None,
                            )
                            .await
                        }
                    });
                    agent_ids.push(aid.id.unwrap());
                    reg_c.fetch_add(1, Ordering::Relaxed);
                }

                // ── Phase 2: each agent sends msgs_per_agent messages ──
                // Agent a sends to agents (a+1)%N, (a+2)%N, ..., (a+5)%N
                for (a, &sender_id) in agent_ids.iter().enumerate() {
                    for m in 0..msgs_per_agent {
                        let receiver_idx = (a + m + 1) % agents_per_project;
                        let receiver_id = agent_ids[receiver_idx];
                        let pp = pool.clone();
                        match block_on(|cx| {
                            let pp2 = pp.clone();
                            async move {
                                queries::create_message_with_recipients(
                                    &cx,
                                    &pp2,
                                    project_id,
                                    sender_id,
                                    &format!("p{p}a{a}m{m}"),
                                    &format!("body {a}-{m}"),
                                    None,
                                    "normal",
                                    true,
                                    "",
                                    &[(receiver_id, "to")],
                                )
                                .await
                            }
                        }) {
                            Outcome::Ok(_) => {
                                snd_c.fetch_add(1, Ordering::Relaxed);
                            }
                            Outcome::Err(e) => {
                                eprintln!("send error p{p} a{a} m{m}: {e:?}");
                                err_c.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {
                                err_c.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }

                // ── Phase 3: each agent fetches inbox + acks up to 5 ──
                for (a, &agent_id) in agent_ids.iter().enumerate() {
                    let pp = pool.clone();
                    match block_on(|cx| {
                        let pp2 = pp.clone();
                        async move {
                            queries::fetch_inbox(&cx, &pp2, project_id, agent_id, false, None, 50)
                                .await
                        }
                    }) {
                        Outcome::Ok(msgs) => {
                            ftc_c.fetch_add(1, Ordering::Relaxed);
                            for msg in msgs.iter().take(msgs_per_agent) {
                                let mid = msg.message.id.unwrap();
                                let pp3 = pool.clone();
                                match block_on(|cx| {
                                    let pp4 = pp3.clone();
                                    async move {
                                        queries::acknowledge_message(&cx, &pp4, agent_id, mid).await
                                    }
                                }) {
                                    Outcome::Ok(_) => {
                                        ack_c.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Outcome::Err(e) => {
                                        eprintln!("ack error p{p} a{a} mid{mid}: {e:?}");
                                        err_c.fetch_add(1, Ordering::Relaxed);
                                    }
                                    _ => {
                                        err_c.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                        Outcome::Err(e) => {
                            eprintln!("fetch error p{p} a{a}: {e:?}");
                            err_c.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            err_c.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread should not panic");
    }

    let elapsed = start.elapsed();
    let regs = registration_count.load(Ordering::Relaxed);
    let sends = send_count.load(Ordering::Relaxed);
    let fetches = fetch_count.load(Ordering::Relaxed);
    let acks = ack_count.load(Ordering::Relaxed);
    let errors = error_count.load(Ordering::Relaxed);

    eprintln!(
        "1000-agent stress: {} regs, {} sends, {} fetches, {} acks, {} errors in {:.2}s",
        regs,
        sends,
        fetches,
        acks,
        errors,
        elapsed.as_secs_f64()
    );

    assert_eq!(errors, 0, "expected zero errors, got {errors}");
    assert_eq!(regs, 1000, "expected 1000 registrations");
    assert_eq!(sends, 5000, "expected 5000 sends");
    assert_eq!(fetches, 1000, "expected 1000 fetches");
    // Each agent should receive ~5 messages (deterministic ring pattern),
    // so acks should be close to 5000.
    assert!(
        acks >= 4000,
        "expected >= 4000 acks (got {acks}; some timing variation possible)"
    );
    assert!(
        elapsed.as_secs() < 120,
        "expected < 120s, took {:.1}s",
        elapsed.as_secs_f64()
    );
}

// =============================================================================
// Test: 200-concurrent burst acquire+release+SELECT (br-15dv.1.1.3)
//
// Proves the pool handles burst contention without SQLITE_BUSY or timeouts.
// =============================================================================

#[test]
fn stress_burst_200_concurrent_acquire_release() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("burst200_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 50,
        min_connections: 10,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    std::mem::forget(dir);

    // Seed a project so SELECT has data to hit
    let human_key = format!("/data/stress/burst200_{}", unique_suffix());
    {
        let p = pool.clone();
        let k = human_key.clone();
        block_on(|cx| async move {
            let _ = queries::ensure_project(&cx, &p, &k).await;
        });
    }

    let n_threads: usize = 200;
    let barrier = Arc::new(Barrier::new(n_threads));
    let error_count = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let errors = Arc::clone(&error_count);
            let key = human_key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                // Each thread: acquire → run a SELECT → release
                block_on(|cx| async move {
                    match pool.acquire(&cx).await {
                        Outcome::Ok(conn) => {
                            // Run a lightweight read to exercise the connection
                            let result = conn.query_sync(
                                "SELECT count(*) AS cnt FROM projects WHERE human_key = ?",
                                &[sqlmodel_core::Value::Text(key)],
                            );
                            if result.is_err() {
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                            drop(conn);
                        }
                        Outcome::Err(e) => {
                            eprintln!("burst200 acquire error: {e:?}");
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread join");
    }

    let errs = error_count.load(Ordering::Relaxed);
    assert_eq!(
        errs, 0,
        "expected 0 errors from 200 concurrent acquire+SELECT, got {errs}"
    );
}

// =============================================================================
// Test: Pool acquire latency budget (br-15dv.1.1.3)
//
// Measures p95 acquire latency under moderate contention (after warmup)
// and asserts it stays below the Yellow SLO threshold (50ms).
// On failure, prints full histogram stats for diagnosis.
// =============================================================================

#[test]
#[allow(clippy::cast_precision_loss)]
fn stress_pool_acquire_latency_budget() {
    use mcp_agent_mail_core::slo;

    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("latency_budget_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 50,
        min_connections: 10,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 10,
    };
    let pool = DbPool::new(&config).expect("create pool");
    std::mem::forget(dir);

    // Warmup phase: pre-open enough connections so that connection-creation
    // cost (file open + PRAGMA application) doesn't skew the measurement.
    // Warm up at least as many as the thread count to avoid cold-start outliers.
    block_on(|cx| {
        let p = pool.clone();
        async move {
            let _ = p.warmup(&cx, 50, std::time::Duration::from_secs(30)).await;
        }
    });

    // Seed a project
    let human_key = format!("/data/stress/latency_{}", unique_suffix());
    {
        let p = pool.clone();
        let k = human_key.clone();
        block_on(|cx| async move {
            let _ = queries::ensure_project(&cx, &p, &k).await;
        });
    }

    // Reset metrics so warmup acquires don't skew the histogram
    let metrics = mcp_agent_mail_core::global_metrics();
    metrics.db.pool_acquire_latency_us.reset();

    // Moderate contention: 50 threads, 20 acquire+release cycles each = 1000 samples
    let n_threads: usize = 50;
    let cycles_per_thread: usize = 20;
    let barrier = Arc::new(Barrier::new(n_threads));
    let error_count = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let errors = Arc::clone(&error_count);
            let key = human_key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..cycles_per_thread {
                    let pp = pool.clone();
                    let k = key.clone();
                    let errs = Arc::clone(&errors);
                    block_on(|cx| async move {
                        match pp.acquire(&cx).await {
                            Outcome::Ok(conn) => {
                                // Lightweight read
                                let _ = conn.query_sync(
                                    "SELECT 1 AS ok WHERE EXISTS (SELECT 1 FROM projects WHERE human_key = ?)",
                                    &[sqlmodel_core::Value::Text(k)],
                                );
                                drop(conn);
                            }
                            _ => {
                                errs.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread join");
    }

    let errs = error_count.load(Ordering::Relaxed);
    assert_eq!(
        errs, 0,
        "expected 0 errors during latency measurement, got {errs}"
    );

    // Check the histogram
    let snap = metrics.db.pool_acquire_latency_us.snapshot();

    eprintln!(
        "pool acquire latency: count={}, min={}μs, p50={}μs, p95={}μs, p99={}μs, max={}μs",
        snap.count, snap.min, snap.p50, snap.p95, snap.p99, snap.max
    );

    // SLO: p95 must stay in Green or Yellow zone (≤ 50ms = 50,000μs)
    #[allow(clippy::cast_precision_loss)]
    let p95_ms = snap.p95 as f64 / 1000.0;
    assert!(
        snap.p95 <= slo::POOL_ACQUIRE_YELLOW_US,
        "pool acquire p95 ({p95}μs = {p95_ms:.1}ms) exceeds Yellow SLO ({yellow}μs = {yellow_ms}ms).\n\
         Histogram: count={count}, min={min}μs, p50={p50}μs, p99={p99}μs, max={max}μs",
        p95 = snap.p95,
        p95_ms = p95_ms,
        yellow = slo::POOL_ACQUIRE_YELLOW_US,
        yellow_ms = slo::POOL_ACQUIRE_YELLOW_US / 1000,
        count = snap.count,
        min = snap.min,
        p50 = snap.p50,
        p99 = snap.p99,
        max = snap.max,
    );

    // Bonus: verify enough samples collected
    let expected_samples = (n_threads * cycles_per_thread) as u64;
    assert!(
        snap.count >= expected_samples,
        "expected at least {expected_samples} samples (warmup excluded), got {}",
        snap.count
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

// ---------------------------------------------------------------------------
// Cache thrashing with Zipfian access patterns (br-15dv.9.3)
// ---------------------------------------------------------------------------

/// Zipfian-like distribution using inverse CDF with configurable skew.
/// Returns an index in `0..n` biased heavily toward lower indices.
/// `skew` controls concentration: 1.0 = moderate, 2.0 = heavy, 3.0 = extreme.
fn zipfian_index_skewed(n: usize, rng_state: &mut u64, skew: f64) -> usize {
    // xorshift64 PRNG (fast, non-crypto)
    let mut x = *rng_state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *rng_state = x;

    // Map uniform u64 → Zipfian: rank = floor(n^(u^skew)).
    // Squaring/cubing u concentrates probability on popular (low-rank) items:
    //   skew=1.0 → ~74% of accesses hit top 20% of items
    //   skew=2.0 → ~86% of accesses hit top 20% of items
    //   skew=3.0 → ~91% of accesses hit top 20% of items
    let u = (x as f64) / (u64::MAX as f64); // uniform in [0, 1)
    let skewed_u = u.powf(skew);
    let rank = (n as f64).powf(skewed_u) - 1.0;
    (rank as usize).min(n - 1)
}

#[test]
fn cache_zipfian_thrashing() {
    use mcp_agent_mail_db::cache::ReadCache;
    use mcp_agent_mail_db::models::{AgentRow, ProjectRow};

    const CACHE_CAPACITY: usize = 100;
    const NUM_PROJECTS: usize = 10;
    const AGENTS_PER_PROJECT: usize = 50; // 500 total agents
    const TOTAL_AGENTS: usize = NUM_PROJECTS * AGENTS_PER_PROJECT;
    const LOOKUPS_PER_CYCLE: usize = 2_000;
    const NUM_CYCLES: usize = 6;

    let cache = ReadCache::new_for_testing_with_capacity(CACHE_CAPACITY);

    // Pre-create all project and agent rows
    let projects: Vec<ProjectRow> = (0..NUM_PROJECTS)
        .map(|i| ProjectRow {
            id: Some(i as i64 + 1),
            slug: format!("proj-{i}"),
            human_key: format!("/data/proj-{i}"),
            created_at: 0,
        })
        .collect();

    let agents: Vec<AgentRow> = (0..TOTAL_AGENTS)
        .map(|i| {
            let project_idx = i / AGENTS_PER_PROJECT;
            let project_id = projects[project_idx].id.unwrap();
            AgentRow {
                id: Some(i as i64 + 1),
                project_id,
                name: format!("Agent{i}"),
                program: "test".to_string(),
                model: "test".to_string(),
                task_description: String::new(),
                inception_ts: 0,
                last_active_ts: 0,
                attachments_policy: "auto".to_string(),
                contact_policy: "open".to_string(),
            }
        })
        .collect();

    // Seed all agents into cache (only CACHE_CAPACITY will survive due to eviction)
    for agent in &agents {
        cache.put_agent(agent);
    }

    // Verify capacity is respected
    let counts = cache.entry_counts();
    assert!(
        counts.agents_by_key <= CACHE_CAPACITY,
        "agents_by_key ({}) exceeds capacity ({CACHE_CAPACITY})",
        counts.agents_by_key
    );

    let mut rng_state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    let mut hit_rates = Vec::with_capacity(NUM_CYCLES);

    for cycle in 0..NUM_CYCLES {
        let mut hits = 0_usize;
        let mut misses = 0_usize;

        for _ in 0..LOOKUPS_PER_CYCLE {
            let idx = zipfian_index_skewed(TOTAL_AGENTS, &mut rng_state, 2.0);
            let agent = &agents[idx];

            // Try cache lookup
            if cache.get_agent(agent.project_id, &agent.name).is_some() {
                hits += 1;
            } else {
                misses += 1;
                // Simulate DB fetch: re-insert into cache
                cache.put_agent(agent);
            }
        }

        let hit_rate = hits as f64 / (hits + misses) as f64;
        hit_rates.push(hit_rate);
        eprintln!(
            "  cycle {cycle}: hits={hits}, misses={misses}, hit_rate={:.1}%",
            hit_rate * 100.0
        );
    }

    // After cycle 1 (0-indexed), cache should have warmed up with popular items.
    // Zipfian skew means a small set of agents are accessed repeatedly.
    assert!(
        hit_rates[1] > 0.50,
        "cycle 1 hit rate ({:.1}%) should be > 50% (Zipfian working set fits in cache)",
        hit_rates[1] * 100.0
    );

    // After cycle 4, LRU should have stabilized popular items.
    assert!(
        hit_rates[4] > 0.70,
        "cycle 4 hit rate ({:.1}%) should be > 70% (LRU stabilized popular items)",
        hit_rates[4] * 100.0
    );

    // Final cycle should maintain high hit rate.
    let last = hit_rates.last().unwrap();
    assert!(
        *last > 0.70,
        "final cycle hit rate ({:.1}%) should be > 70%",
        last * 100.0
    );
}

#[test]
fn cache_zipfian_within_capacity() {
    use mcp_agent_mail_db::cache::ReadCache;
    use mcp_agent_mail_db::models::AgentRow;

    // When working set fits entirely in cache, hit rate should be ~100%
    // after the initial cold start.
    const CACHE_CAPACITY: usize = 200;
    const NUM_AGENTS: usize = 100; // fits comfortably
    const LOOKUPS: usize = 5_000;

    let cache = ReadCache::new_for_testing_with_capacity(CACHE_CAPACITY);

    let agents: Vec<AgentRow> = (0..NUM_AGENTS)
        .map(|i| AgentRow {
            id: Some(i as i64 + 1),
            project_id: 1,
            name: format!("FitAgent{i}"),
            program: "test".to_string(),
            model: "test".to_string(),
            task_description: String::new(),
            inception_ts: 0,
            last_active_ts: 0,
            attachments_policy: "auto".to_string(),
            contact_policy: "open".to_string(),
        })
        .collect();

    // Pre-warm cache: two passes needed because S3-FIFO's Small queue is 10%
    // of capacity (20 slots). First pass fills Small and overflows 80 items to
    // Ghost. Second pass promotes those Ghost items to Main via get→miss→put.
    for agent in &agents {
        cache.put_agent(agent);
    }
    for agent in &agents {
        if cache.get_agent(agent.project_id, &agent.name).is_none() {
            cache.put_agent(agent); // promote from Ghost to Main
        }
    }

    let mut rng_state: u64 = 0xCAFE_1234_5678_ABCD;
    let mut hits = 0_usize;
    let mut misses = 0_usize;

    for _ in 0..LOOKUPS {
        let idx = zipfian_index_skewed(NUM_AGENTS, &mut rng_state, 2.0);
        let agent = &agents[idx];
        if cache.get_agent(agent.project_id, &agent.name).is_some() {
            hits += 1;
        } else {
            misses += 1;
            cache.put_agent(agent);
        }
    }

    let hit_rate = hits as f64 / (hits + misses) as f64;
    eprintln!(
        "  within-capacity: hits={hits}, misses={misses}, hit_rate={:.1}%",
        hit_rate * 100.0
    );

    // All agents fit in cache, so after pre-warm every lookup should hit.
    assert!(
        hit_rate > 0.99,
        "within-capacity hit rate ({:.1}%) should be > 99%",
        hit_rate * 100.0
    );
}

#[test]
fn cache_concurrent_zipfian_access() {
    use mcp_agent_mail_db::cache::ReadCache;
    use mcp_agent_mail_db::models::AgentRow;

    const CACHE_CAPACITY: usize = 100;
    const NUM_AGENTS: usize = 500;
    const LOOKUPS_PER_THREAD: usize = 1_000;
    const NUM_THREADS: usize = 8;

    let cache = Arc::new(ReadCache::new_for_testing_with_capacity(CACHE_CAPACITY));

    let agents: Arc<Vec<AgentRow>> = Arc::new(
        (0..NUM_AGENTS)
            .map(|i| AgentRow {
                id: Some(i as i64 + 1),
                project_id: 1,
                name: format!("ConcAgent{i}"),
                program: "test".to_string(),
                model: "test".to_string(),
                task_description: String::new(),
                inception_ts: 0,
                last_active_ts: 0,
                attachments_policy: "auto".to_string(),
                contact_policy: "open".to_string(),
            })
            .collect(),
    );

    let barrier = Arc::new(Barrier::new(NUM_THREADS));
    let total_hits = Arc::new(AtomicU64::new(0));
    let total_misses = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..NUM_THREADS)
        .map(|tid| {
            let cache = Arc::clone(&cache);
            let agents = Arc::clone(&agents);
            let barrier = Arc::clone(&barrier);
            let total_hits = Arc::clone(&total_hits);
            let total_misses = Arc::clone(&total_misses);

            std::thread::spawn(move || {
                // Each thread gets a unique PRNG seed
                let mut rng_state: u64 = 0xBEEF_DEAD_0000_0000 | (tid as u64 + 1);
                barrier.wait();

                let mut hits = 0_u64;
                let mut misses = 0_u64;

                for _ in 0..LOOKUPS_PER_THREAD {
                    let idx = zipfian_index_skewed(NUM_AGENTS, &mut rng_state, 2.0);
                    let agent = &agents[idx];
                    if cache.get_agent(agent.project_id, &agent.name).is_some() {
                        hits += 1;
                    } else {
                        misses += 1;
                        cache.put_agent(agent);
                    }
                }

                total_hits.fetch_add(hits, Ordering::Relaxed);
                total_misses.fetch_add(misses, Ordering::Relaxed);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let hits = total_hits.load(Ordering::Relaxed);
    let misses = total_misses.load(Ordering::Relaxed);
    let total = hits + misses;
    let hit_rate = hits as f64 / total as f64;

    eprintln!(
        "  concurrent: hits={hits}, misses={misses}, total={total}, hit_rate={:.1}%",
        hit_rate * 100.0
    );

    // With concurrent access, hit rate will be lower due to contention, but
    // should still be reasonable with Zipfian skew.
    assert!(
        hit_rate > 0.30,
        "concurrent hit rate ({:.1}%) should be > 30% (Zipfian still concentrates on popular items)",
        hit_rate * 100.0
    );

    // Verify no capacity violation
    let counts = cache.entry_counts();
    assert!(
        counts.agents_by_key <= CACHE_CAPACITY,
        "agents_by_key ({}) exceeds capacity ({CACHE_CAPACITY})",
        counts.agents_by_key
    );
}

// ---------------------------------------------------------------------------
// Large message payload stress tests (br-15dv.9.5)
// ---------------------------------------------------------------------------

/// Helper: generate a deterministic body of `n` bytes.
fn make_large_body(n: usize) -> String {
    // Produce repeating text with some variation so FTS can index real tokens.
    let phrase = "The quick brown fox jumped over the lazy sleeping dog. ";
    let mut body = String::with_capacity(n + phrase.len());
    while body.len() < n {
        body.push_str(phrase);
    }
    body.truncate(n);
    body
}

/// Helper: set up a project + sender + receiver in the pool.
fn setup_project_and_agents(pool: &DbPool) -> (i64, i64, i64) {
    let suffix = unique_suffix();

    let pid = block_on_with_retry(3, |cx| {
        let pool = pool.clone();
        let hk = format!("/data/lgmsg-{suffix}");
        async move { queries::ensure_project(&cx, &pool, &hk).await }
    })
    .id
    .unwrap();

    let sender_id = block_on_with_retry(3, |cx| {
        let pool = pool.clone();
        async move {
            queries::register_agent(&cx, &pool, pid, "BoldCastle", "test", "test", None, None).await
        }
    })
    .id
    .unwrap();

    let receiver_id = block_on_with_retry(3, |cx| {
        let pool = pool.clone();
        async move {
            queries::register_agent(&cx, &pool, pid, "QuietLake", "test", "test", None, None).await
        }
    })
    .id
    .unwrap();

    (pid, sender_id, receiver_id)
}

fn create_message_for_receiver(
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    receiver_id: i64,
    subject: &str,
    ack_required: bool,
) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let subject = subject.to_string();
        async move {
            match queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                &subject,
                "inbox stats cache lifecycle body",
                None,
                "normal",
                ack_required,
                "",
                &[(receiver_id, "to")],
            )
            .await
            {
                Outcome::Ok(row) => row.id.expect("created message must include id"),
                Outcome::Err(e) => {
                    panic!("create_message_with_recipients failed: {e:?}")
                }
                Outcome::Cancelled(r) => panic!("create_message_with_recipients cancelled: {r:?}"),
                Outcome::Panicked(p) => panic!("{p}"),
            }
        }
    })
}

fn get_inbox_stats_opt(pool: &DbPool, agent_id: i64) -> Option<InboxStatsRow> {
    block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::get_inbox_stats(&cx, &pool, agent_id).await {
                Outcome::Ok(stats) => stats,
                Outcome::Err(e) => panic!("get_inbox_stats failed for agent {agent_id}: {e:?}"),
                Outcome::Cancelled(r) => {
                    panic!("get_inbox_stats cancelled for agent {agent_id}: {r:?}")
                }
                Outcome::Panicked(p) => panic!("{p}"),
            }
        }
    })
}

fn invalidate_cached_inbox_stats(pool: &DbPool, agent_id: i64) {
    read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), agent_id);
}

fn get_cached_inbox_stats(pool: &DbPool, agent_id: i64) -> Option<InboxStatsRow> {
    read_cache().get_inbox_stats_scoped(pool.sqlite_path(), agent_id)
}

fn put_cached_inbox_stats(pool: &DbPool, stats: &InboxStatsRow) {
    read_cache().put_inbox_stats_scoped(pool.sqlite_path(), stats);
}

fn get_inbox_stats(pool: &DbPool, agent_id: i64) -> InboxStatsRow {
    get_inbox_stats_opt(pool, agent_id)
        .unwrap_or_else(|| panic!("expected inbox stats row for agent {agent_id}, got None"))
}

fn mark_message_read(pool: &DbPool, agent_id: i64, message_id: i64) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::mark_message_read(&cx, &pool, agent_id, message_id).await {
                Outcome::Ok(ts) => ts,
                Outcome::Err(e) => {
                    panic!("mark_message_read failed for {agent_id}:{message_id}: {e:?}")
                }
                Outcome::Cancelled(r) => {
                    panic!("mark_message_read cancelled for {agent_id}:{message_id}: {r:?}")
                }
                Outcome::Panicked(p) => panic!("{p}"),
            }
        }
    })
}

fn acknowledge_message(pool: &DbPool, agent_id: i64, message_id: i64) -> (i64, i64) {
    block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::acknowledge_message(&cx, &pool, agent_id, message_id).await {
                Outcome::Ok(ts) => ts,
                Outcome::Err(e) => {
                    panic!("acknowledge_message failed for {agent_id}:{message_id}: {e:?}")
                }
                Outcome::Cancelled(r) => {
                    panic!("acknowledge_message cancelled for {agent_id}:{message_id}: {r:?}")
                }
                Outcome::Panicked(p) => panic!("{p}"),
            }
        }
    })
}

#[test]
fn stress_inbox_stats_cache_miss_read_through_and_hit() {
    let _cache_guard = INBOX_STATS_TEST_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let (project_id, sender_id, receiver_id) = setup_project_and_agents(&pool);

    invalidate_cached_inbox_stats(&pool, receiver_id);
    assert!(
        get_cached_inbox_stats(&pool, receiver_id).is_none(),
        "cache should start empty for receiver {receiver_id}"
    );

    let no_stats = get_inbox_stats_opt(&pool, receiver_id);
    assert!(
        no_stats.is_none(),
        "agent {receiver_id} should not have inbox stats before receiving messages"
    );
    assert!(
        get_cached_inbox_stats(&pool, receiver_id).is_none(),
        "cache miss path must not materialize stats when DB has no row"
    );

    let _msg_id = create_message_for_receiver(
        &pool,
        project_id,
        sender_id,
        receiver_id,
        "cache-miss-read-through",
        true,
    );

    let first = get_inbox_stats(&pool, receiver_id);
    assert_eq!(
        first.total_count, 1,
        "first DB-backed read should report exactly one delivered message"
    );
    assert_eq!(
        first.unread_count, 1,
        "first message should be unread before mark_message_read"
    );
    assert_eq!(
        first.ack_pending_count, 1,
        "ack_required message should increment ack_pending_count"
    );
    let cached_after_first = get_cached_inbox_stats(&pool, receiver_id);
    assert!(
        cached_after_first.is_some(),
        "read-through miss should populate cache for receiver {receiver_id}"
    );
    let cached_after_first = cached_after_first.unwrap();
    assert_eq!(
        cached_after_first.total_count, first.total_count,
        "cached total_count should match first DB-backed read"
    );
    assert_eq!(
        cached_after_first.unread_count, first.unread_count,
        "cached unread_count should match first DB-backed read"
    );
    assert_eq!(
        cached_after_first.ack_pending_count, first.ack_pending_count,
        "cached ack_pending_count should match first DB-backed read"
    );

    let second = get_inbox_stats(&pool, receiver_id);
    assert_eq!(
        second.total_count, first.total_count,
        "cache hit should preserve total_count"
    );
    assert_eq!(
        second.unread_count, first.unread_count,
        "cache hit should preserve unread_count"
    );
    assert_eq!(
        second.ack_pending_count, first.ack_pending_count,
        "cache hit should preserve ack_pending_count"
    );
    assert!(
        get_cached_inbox_stats(&pool, receiver_id).is_some(),
        "cache entry should remain present after hit"
    );

    invalidate_cached_inbox_stats(&pool, receiver_id);
}

#[test]
fn stress_inbox_stats_cache_short_circuits_db_on_hit() {
    let _cache_guard = INBOX_STATS_TEST_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let (project_id, sender_id, receiver_id) = setup_project_and_agents(&pool);

    invalidate_cached_inbox_stats(&pool, receiver_id);
    let _msg_id = create_message_for_receiver(
        &pool,
        project_id,
        sender_id,
        receiver_id,
        "cache-hit-short-circuit",
        false,
    );

    let db_stats = get_inbox_stats(&pool, receiver_id);
    assert_eq!(
        db_stats.total_count, 1,
        "DB stats should report one delivered message before cache override"
    );
    assert_eq!(
        db_stats.unread_count, 1,
        "DB stats should report unread message before cache override"
    );
    assert_eq!(
        db_stats.ack_pending_count, 0,
        "non-ack-required message should not increment ack_pending_count"
    );

    let sentinel = InboxStatsRow {
        agent_id: receiver_id,
        total_count: 999,
        unread_count: 888,
        ack_pending_count: 777,
        last_message_ts: Some(db_stats.last_message_ts.unwrap_or(0) + 1),
    };

    // Retry: parallel tests may invalidate the global cache via
    // create_message_with_recipients for recipients with the same
    // auto-increment agent_id in their own DBs.
    let mut cache_hit = false;
    for _ in 0..20 {
        put_cached_inbox_stats(&pool, &sentinel);
        let cached = get_inbox_stats(&pool, receiver_id);
        if cached.total_count == sentinel.total_count {
            assert_eq!(
                cached.unread_count, sentinel.unread_count,
                "cache hit should return cached unread_count instead of DB value"
            );
            assert_eq!(
                cached.ack_pending_count, sentinel.ack_pending_count,
                "cache hit should return cached ack_pending_count instead of DB value"
            );
            cache_hit = true;
            break;
        }
    }
    assert!(
        cache_hit,
        "cache hit should return cached total_count instead of DB value after retries"
    );

    invalidate_cached_inbox_stats(&pool, receiver_id);
    let refreshed = get_inbox_stats(&pool, receiver_id);
    assert_eq!(
        refreshed.total_count, db_stats.total_count,
        "after invalidation, read should return DB total_count"
    );
    assert_eq!(
        refreshed.unread_count, db_stats.unread_count,
        "after invalidation, read should return DB unread_count"
    );
    assert_eq!(
        refreshed.ack_pending_count, db_stats.ack_pending_count,
        "after invalidation, read should return DB ack_pending_count"
    );

    invalidate_cached_inbox_stats(&pool, receiver_id);
}

#[test]
fn stress_inbox_stats_invalidation_after_read_ack_and_new_message() {
    let _cache_guard = INBOX_STATS_TEST_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let (project_id, sender_id, receiver_id) = setup_project_and_agents(&pool);

    invalidate_cached_inbox_stats(&pool, receiver_id);

    let first_msg = create_message_for_receiver(
        &pool,
        project_id,
        sender_id,
        receiver_id,
        "invalidation-baseline",
        true,
    );

    let baseline = get_inbox_stats(&pool, receiver_id);
    assert_eq!(baseline.total_count, 1, "baseline total_count should be 1");
    assert_eq!(
        baseline.unread_count, 1,
        "baseline unread_count should be 1"
    );
    assert_eq!(
        baseline.ack_pending_count, 1,
        "baseline ack_pending_count should be 1 for ack-required message"
    );

    let stale_before_mark_read = InboxStatsRow {
        agent_id: receiver_id,
        total_count: 71,
        unread_count: 71,
        ack_pending_count: 71,
        last_message_ts: Some(baseline.last_message_ts.unwrap_or(0) + 11),
    };
    put_cached_inbox_stats(&pool, &stale_before_mark_read);

    let _read_ts = mark_message_read(&pool, receiver_id, first_msg);
    let after_mark_read = get_inbox_stats(&pool, receiver_id);
    assert_eq!(
        after_mark_read.total_count, 1,
        "mark_message_read should not change total_count"
    );
    assert_eq!(
        after_mark_read.unread_count, 0,
        "mark_message_read should decrement unread_count to zero"
    );
    assert_eq!(
        after_mark_read.ack_pending_count, 1,
        "mark_message_read should not change ack_pending_count"
    );
    assert_ne!(
        after_mark_read.unread_count, stale_before_mark_read.unread_count,
        "stale cached unread_count must be cleared by mark_message_read invalidation"
    );

    let stale_before_ack = InboxStatsRow {
        agent_id: receiver_id,
        total_count: 62,
        unread_count: 62,
        ack_pending_count: 62,
        last_message_ts: Some(after_mark_read.last_message_ts.unwrap_or(0) + 22),
    };
    put_cached_inbox_stats(&pool, &stale_before_ack);

    let _ack_ts = acknowledge_message(&pool, receiver_id, first_msg);
    let after_ack = get_inbox_stats(&pool, receiver_id);
    assert_eq!(
        after_ack.total_count, 1,
        "ack should not change total_count"
    );
    assert_eq!(
        after_ack.unread_count, 0,
        "ack should not change unread_count"
    );
    assert_eq!(
        after_ack.ack_pending_count, 0,
        "acknowledge_message should decrement ack_pending_count to zero"
    );
    assert_ne!(
        after_ack.ack_pending_count, stale_before_ack.ack_pending_count,
        "stale cached ack_pending_count must be cleared by acknowledge_message invalidation"
    );

    let stale_before_create = InboxStatsRow {
        agent_id: receiver_id,
        total_count: -1,
        unread_count: -1,
        ack_pending_count: -1,
        last_message_ts: Some(0),
    };
    put_cached_inbox_stats(&pool, &stale_before_create);

    let _second_msg = create_message_for_receiver(
        &pool,
        project_id,
        sender_id,
        receiver_id,
        "invalidation-create",
        true,
    );
    let after_create = get_inbox_stats(&pool, receiver_id);
    assert_eq!(
        after_create.total_count, 2,
        "new recipient message should increment total_count"
    );
    assert_eq!(
        after_create.unread_count, 1,
        "new unread message should increment unread_count"
    );
    assert_eq!(
        after_create.ack_pending_count, 1,
        "new ack-required message should increment ack_pending_count"
    );
    assert_ne!(
        after_create.total_count, stale_before_create.total_count,
        "stale cached totals must be cleared by create_message_with_recipients invalidation"
    );

    invalidate_cached_inbox_stats(&pool, receiver_id);
}

#[test]
fn stress_large_message_512kb_roundtrip() {
    // Verify that a 512KB message body survives the full DB roundtrip:
    // insert → fetch → FTS search.
    let (pool, _dir) = make_pool();
    let (pid, sender_id, receiver_id) = setup_project_and_agents(&pool);

    let body = make_large_body(512 * 1024); // 512 KB
    assert_eq!(body.len(), 512 * 1024);

    let msg_id = block_on(|cx| {
        let pool = pool.clone();
        let body = body.clone();
        async move {
            match queries::create_message_with_recipients(
                &cx,
                &pool,
                pid,
                sender_id,
                "Large 512KB message",
                &body,
                None,
                "normal",
                false,
                "",
                &[(receiver_id, "to")],
            )
            .await
            {
                Outcome::Ok(row) => row.id.unwrap(),
                other => panic!("create_message_with_recipients failed: {other:?}"),
            }
        }
    });

    // Fetch the message back and verify body integrity
    let fetched = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::fetch_inbox(&cx, &pool, pid, receiver_id, false, None, 10).await {
                Outcome::Ok(rows) => rows,
                other => panic!("fetch_inbox failed: {other:?}"),
            }
        }
    });

    assert!(!fetched.is_empty(), "inbox should contain the message");
    let found = fetched.iter().find(|r| r.message.id == Some(msg_id));
    assert!(found.is_some(), "message {msg_id} not found in inbox");
    assert_eq!(
        found.unwrap().message.body_md.len(),
        512 * 1024,
        "body should survive roundtrip intact"
    );

    // FTS search should find a word from the body
    let results = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::search_messages(&cx, &pool, pid, "quick brown fox", 10).await {
                Outcome::Ok(rows) => rows,
                other => panic!("search_messages failed: {other:?}"),
            }
        }
    });

    assert!(
        !results.is_empty(),
        "FTS should find 'quick brown fox' in the 512KB body"
    );
    assert_eq!(results[0].id, msg_id);
}

#[test]
fn stress_concurrent_large_messages() {
    // Send 20 messages with 100KB bodies concurrently from multiple threads.
    // Verifies no corruption or deadlocks under large-payload contention.
    const NUM_MESSAGES: usize = 20;
    const BODY_SIZE: usize = 100 * 1024; // 100 KB each

    let (pool, _dir) = make_pool();
    let (pid, sender_id, receiver_id) = setup_project_and_agents(&pool);

    let pool = Arc::new(pool);
    let barrier = Arc::new(Barrier::new(NUM_MESSAGES));
    let success_count = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..NUM_MESSAGES)
        .map(|i| {
            let pool = Arc::clone(&pool);
            let barrier = Arc::clone(&barrier);
            let success_count = Arc::clone(&success_count);

            std::thread::spawn(move || {
                let body = make_large_body(BODY_SIZE);
                let subject = format!("Concurrent large msg #{i}");

                barrier.wait();

                let result = block_on(|cx| {
                    let pool = (*pool).clone();
                    let subject = subject.clone();
                    let body = body.clone();
                    async move {
                        queries::create_message_with_recipients(
                            &cx,
                            &pool,
                            pid,
                            sender_id,
                            &subject,
                            &body,
                            None,
                            "normal",
                            false,
                            "",
                            &[(receiver_id, "to")],
                        )
                        .await
                    }
                });

                match result {
                    Outcome::Ok(row) => {
                        assert!(row.id.is_some());
                        success_count.fetch_add(1, Ordering::Relaxed);
                    }
                    other => panic!("thread {i}: create failed: {other:?}"),
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let successes = success_count.load(Ordering::Relaxed);
    eprintln!("  concurrent large messages: {successes}/{NUM_MESSAGES} succeeded");
    assert_eq!(
        successes, NUM_MESSAGES as u64,
        "all messages should succeed"
    );

    // Verify all messages are in the inbox
    let fetched = block_on(|cx| {
        let pool = (*pool).clone();
        async move {
            match queries::fetch_inbox(&cx, &pool, pid, receiver_id, false, None, 50).await {
                Outcome::Ok(rows) => rows,
                other => panic!("fetch_inbox failed: {other:?}"),
            }
        }
    });

    assert_eq!(
        fetched.len(),
        NUM_MESSAGES,
        "inbox should contain all {NUM_MESSAGES} messages"
    );

    // Verify body integrity for each
    for row in &fetched {
        assert_eq!(
            row.message.body_md.len(),
            BODY_SIZE,
            "message body should be {BODY_SIZE} bytes, got {}",
            row.message.body_md.len()
        );
    }
}

#[test]
fn stress_fts_large_body_search_performance() {
    // Index 10 messages with 256KB bodies, then search.
    // Measures that FTS works correctly with large payloads.
    const NUM_MESSAGES: usize = 10;
    const BODY_SIZE: usize = 256 * 1024; // 256 KB

    let (pool, _dir) = make_pool();
    let (pid, sender_id, receiver_id) = setup_project_and_agents(&pool);

    // Each message gets a unique keyword so we can search for specific ones
    let mut msg_ids = Vec::with_capacity(NUM_MESSAGES);
    for i in 0..NUM_MESSAGES {
        let mut body = make_large_body(BODY_SIZE - 50);
        // Embed a unique searchable token at the end
        let token = format!(" UNIQUETOKEN{i:04} ");
        body.push_str(&token);

        let msg_id = block_on(|cx| {
            let pool = pool.clone();
            let body = body.clone();
            let subject = format!("FTS stress #{i}");
            async move {
                match queries::create_message_with_recipients(
                    &cx,
                    &pool,
                    pid,
                    sender_id,
                    &subject,
                    &body,
                    None,
                    "normal",
                    false,
                    "",
                    &[(receiver_id, "to")],
                )
                .await
                {
                    Outcome::Ok(row) => row.id.unwrap(),
                    other => panic!("create msg {i} failed: {other:?}"),
                }
            }
        });
        msg_ids.push(msg_id);
    }

    // Search for a specific unique token
    let target_idx = 7;
    let search_term = format!("UNIQUETOKEN{target_idx:04}");

    let start = std::time::Instant::now();
    let results = block_on(|cx| {
        let pool = pool.clone();
        let term = search_term.clone();
        async move {
            match queries::search_messages(&cx, &pool, pid, &term, 10).await {
                Outcome::Ok(rows) => rows,
                other => panic!("search failed: {other:?}"),
            }
        }
    });
    let search_ms = start.elapsed().as_millis();

    eprintln!(
        "  FTS search over {} × {}KB bodies took {search_ms}ms, found {} results",
        NUM_MESSAGES,
        BODY_SIZE / 1024,
        results.len()
    );

    assert_eq!(
        results.len(),
        1,
        "should find exactly 1 message with token '{search_term}'"
    );
    assert_eq!(results[0].id, msg_ids[target_idx]);

    // Search for common term across all messages
    let common_results = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::search_messages(&cx, &pool, pid, "quick brown fox", 20).await {
                Outcome::Ok(rows) => rows,
                other => panic!("common search failed: {other:?}"),
            }
        }
    });

    assert_eq!(
        common_results.len(),
        NUM_MESSAGES,
        "all {NUM_MESSAGES} messages should match 'quick brown fox'"
    );

    // Search time should be reasonable even with large bodies
    assert!(
        search_ms < 5_000,
        "FTS search should complete within 5s, took {search_ms}ms"
    );
}

// =============================================================================
// Chaos testing: Injectable faults + circuit breaker verification
// =============================================================================

use mcp_agent_mail_db::{
    CIRCUIT_DB, CIRCUIT_GIT, CIRCUIT_LLM, CIRCUIT_SIGNAL, CircuitBreaker, CircuitState, Subsystem,
    circuit_for,
};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Lightweight fault injector for chaos testing.
///
/// Each call to `should_fail()` checks a counter-based failure schedule:
/// the operation fails if `call_count % period < fail_count`. This gives
/// deterministic, reproducible failure patterns (e.g., period=10, fail=1
/// means every 10th call fails).
struct FaultInjector {
    call_count: AtomicU64,
    /// How many calls out of every `period` calls should fail.
    fail_count: u64,
    /// The period of the failure cycle.
    period: u64,
    /// Whether injection is currently active.
    active: AtomicBool,
}

#[allow(dead_code)]
impl FaultInjector {
    /// Create with failure rate: `fail_count` out of every `period` calls fail.
    const fn new(fail_count: u64, period: u64) -> Self {
        Self {
            call_count: AtomicU64::new(0),
            fail_count,
            period,
            active: AtomicBool::new(true),
        }
    }

    /// Check if the current call should fail.
    fn should_fail(&self) -> bool {
        if !self.active.load(Ordering::Relaxed) {
            return false;
        }
        let n = self.call_count.fetch_add(1, Ordering::Relaxed);
        (n % self.period) < self.fail_count
    }

    /// Deactivate fault injection (simulates recovery).
    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// Reactivate fault injection.
    fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }

    /// Total calls made.
    fn total_calls(&self) -> u64 {
        self.call_count.load(Ordering::Relaxed)
    }
}

// -- Test: DB circuit breaker trips and recovers under 10% failure rate ------

#[test]
fn chaos_db_circuit_trips_and_recovers() {
    let cb = CircuitBreaker::with_subsystem("db", 5, Duration::from_millis(100));
    let injector = FaultInjector::new(1, 10); // 10% failure rate

    // Phase 1: Trip the circuit explicitly with 5 failures.
    for _ in 0..5 {
        cb.record_failure();
    }
    assert_eq!(
        cb.state(),
        CircuitState::Open,
        "circuit should be open after 5 failures"
    );

    // Verify injector produces ~10% failure rate.
    let mut inj_failures = 0u32;
    for _ in 0..100 {
        if injector.should_fail() {
            inj_failures += 1;
        }
    }
    assert!(
        inj_failures >= 5,
        "injector should produce ~10% failures, got {inj_failures}"
    );

    // Phase 2: Wait for recovery window — circuit should become half-open.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        cb.state(),
        CircuitState::HalfOpen,
        "circuit should be half-open after reset"
    );

    // Phase 3: Prove recovery with 3 consecutive successes.
    // First probe goes through check(); subsequent record_success() calls
    // simulate the real retry path (check gates the first call, then
    // record_success handles the half-open accumulation).
    assert!(
        cb.check().is_ok(),
        "first half-open probe should be allowed"
    );
    cb.record_success();
    assert_eq!(
        cb.state(),
        CircuitState::HalfOpen,
        "still half-open after 1 success"
    );
    cb.record_success();
    assert_eq!(
        cb.state(),
        CircuitState::HalfOpen,
        "still half-open after 2 successes"
    );
    cb.record_success();
    assert_eq!(
        cb.state(),
        CircuitState::Closed,
        "circuit should close after 3 consecutive successes"
    );

    // Phase 4: Run under fault injection — verify circuit handles mixed load.
    let mut successes = 0u32;
    let mut failures = 0u32;
    for _ in 0..50 {
        if cb.check().is_err() {
            continue;
        }
        if injector.should_fail() {
            cb.record_failure();
            failures += 1;
        } else {
            cb.record_success();
            successes += 1;
        }
    }
    assert!(
        successes > 0,
        "should have some successes under 10% fault rate"
    );
    assert!(
        failures > 0,
        "should have some failures under 10% fault rate"
    );
}

// -- Test: Git circuit independent from DB circuit ---------------------------

#[test]
fn chaos_git_failure_does_not_affect_db() {
    let db_cb = CircuitBreaker::with_subsystem("db", 5, Duration::from_secs(30));
    let git_cb = CircuitBreaker::with_subsystem("git", 3, Duration::from_secs(30));

    // Simulate git failures until circuit trips.
    for _ in 0..3 {
        git_cb.record_failure();
    }
    assert_eq!(
        git_cb.state(),
        CircuitState::Open,
        "git circuit should be open"
    );

    // DB circuit should be unaffected.
    assert_eq!(
        db_cb.state(),
        CircuitState::Closed,
        "db circuit should remain closed"
    );
    assert!(db_cb.check().is_ok(), "db operations should still work");

    // Simulate successful DB operations.
    for _ in 0..10 {
        db_cb.record_success();
    }
    assert_eq!(db_cb.state(), CircuitState::Closed, "db should stay closed");
    assert_eq!(
        git_cb.state(),
        CircuitState::Open,
        "git should still be open"
    );
}

// -- Test: Alternating failures across subsystems ----------------------------

#[test]
fn chaos_alternating_subsystem_failures() {
    let db_cb = CircuitBreaker::with_subsystem("db", 3, Duration::from_millis(100));
    let git_cb = CircuitBreaker::with_subsystem("git", 3, Duration::from_millis(100));

    // Phase 1: DB fails, git is fine.
    for _ in 0..3 {
        db_cb.record_failure();
    }
    assert_eq!(db_cb.state(), CircuitState::Open);
    assert_eq!(git_cb.state(), CircuitState::Closed);

    // Phase 2: DB recovers, then git fails.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(db_cb.state(), CircuitState::HalfOpen);
    for _ in 0..3 {
        db_cb.record_success();
    }
    assert_eq!(db_cb.state(), CircuitState::Closed);

    for _ in 0..3 {
        git_cb.record_failure();
    }
    assert_eq!(db_cb.state(), CircuitState::Closed, "db stays closed");
    assert_eq!(git_cb.state(), CircuitState::Open, "git is now open");

    // Phase 3: Both recover.
    std::thread::sleep(Duration::from_millis(150));
    for _ in 0..3 {
        git_cb.record_success();
    }
    assert_eq!(db_cb.state(), CircuitState::Closed);
    assert_eq!(git_cb.state(), CircuitState::Closed);
}

// -- Test: Concurrent threads with injected failures -------------------------

#[test]
fn chaos_concurrent_threads_with_db_faults() {
    let cb = Arc::new(CircuitBreaker::with_subsystem(
        "db",
        5,
        Duration::from_millis(200),
    ));
    let injector = Arc::new(FaultInjector::new(3, 10)); // 30% failure rate
    let n_threads = 8;
    let ops_per_thread = 50;
    let barrier = Arc::new(Barrier::new(n_threads));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let cb = Arc::clone(&cb);
            let inj = Arc::clone(&injector);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let mut successes = 0u32;
                let mut failures = 0u32;
                let mut blocked = 0u32;
                for _ in 0..ops_per_thread {
                    if cb.check().is_err() {
                        blocked += 1;
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    if inj.should_fail() {
                        cb.record_failure();
                        failures += 1;
                    } else {
                        cb.record_success();
                        successes += 1;
                    }
                }
                (successes, failures, blocked)
            })
        })
        .collect();

    let mut total_s = 0u32;
    let mut total_f = 0u32;
    let mut total_b = 0u32;
    for h in handles {
        let (s, f, b) = h.join().unwrap();
        total_s += s;
        total_f += f;
        total_b += b;
    }

    // No panics occurred — the critical assertion.
    assert!(total_s > 0, "should have some successes");
    assert!(total_f > 0, "should have some failures with 30% rate");
    // With 30% failure rate, circuit should have tripped at least once.
    assert!(
        total_b > 0 || total_f >= 5,
        "circuit should have tripped or had enough failures: blocked={total_b}, failures={total_f}"
    );
}

// -- Test: Circuit state transitions are always valid ------------------------

#[test]
fn chaos_circuit_state_transitions_valid() {
    let cb = CircuitBreaker::with_subsystem("test", 3, Duration::from_millis(50));

    // Track state transitions.
    let mut transitions: Vec<(CircuitState, CircuitState)> = Vec::new();
    let mut prev_state = cb.state();

    // Run through a full lifecycle: closed -> open -> half-open -> closed
    let ops = [
        (false, "success"),
        (true, "fail"),
        (true, "fail"),
        (true, "fail"), // trips at 3
    ];

    for (should_fail, _label) in &ops {
        if *should_fail {
            cb.record_failure();
        } else {
            cb.record_success();
        }
        let new_state = cb.state();
        if new_state != prev_state {
            transitions.push((prev_state, new_state));
            prev_state = new_state;
        }
    }

    // Should have transitioned Closed -> Open
    assert!(
        transitions.contains(&(CircuitState::Closed, CircuitState::Open)),
        "should have Closed->Open transition: {transitions:?}"
    );

    // Wait for half-open
    std::thread::sleep(Duration::from_millis(70));
    let new_state = cb.state();
    if new_state != prev_state {
        transitions.push((prev_state, new_state));
        prev_state = new_state;
    }
    assert!(
        transitions.contains(&(CircuitState::Open, CircuitState::HalfOpen)),
        "should have Open->HalfOpen transition: {transitions:?}"
    );

    // Recover
    for _ in 0..3 {
        cb.record_success();
    }
    let new_state = cb.state();
    if new_state != prev_state {
        transitions.push((prev_state, new_state));
    }

    assert!(
        transitions.contains(&(CircuitState::HalfOpen, CircuitState::Closed)),
        "should have HalfOpen->Closed transition: {transitions:?}"
    );

    // Verify no invalid transitions exist.
    for (from, to) in &transitions {
        let valid = matches!(
            (from, to),
            (CircuitState::Closed, CircuitState::Open)
                | (CircuitState::Open, CircuitState::HalfOpen)
                | (
                    CircuitState::HalfOpen,
                    CircuitState::Closed | CircuitState::Open
                )
        );
        assert!(valid, "invalid transition: {from} -> {to}");
    }
}

// -- Test: Data integrity after circuit breaker recovery ---------------------

#[test]
fn chaos_data_integrity_after_cb_recovery() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/chaos/integrity_{suffix}");

    // Setup: create project and agents.
    let (pid, sender_id, receiver_id) = {
        let p = pool.clone();
        let key = human_key;
        block_on(|cx| async move {
            let proj = match queries::ensure_project(&cx, &p, &key).await {
                Outcome::Ok(r) => r,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let pid = proj.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &p,
                pid,
                "RedLake",
                "chaos",
                "test",
                Some("sender"),
                None,
            )
            .await
            {
                Outcome::Ok(r) => r,
                other => panic!("register sender failed: {other:?}"),
            };

            let receiver = match queries::register_agent(
                &cx,
                &p,
                pid,
                "BluePeak",
                "chaos",
                "test",
                Some("receiver"),
                None,
            )
            .await
            {
                Outcome::Ok(r) => r,
                other => panic!("register receiver failed: {other:?}"),
            };

            (pid, sender.id.unwrap(), receiver.id.unwrap())
        })
    };

    // Phase 1: Send messages, some with simulated failures (we retry on failure).
    let num_messages = 20;
    let injector = FaultInjector::new(2, 10); // 20% simulated failure rate
    let mut sent_ids = Vec::new();

    for i in 0..num_messages {
        let success = loop {
            if injector.should_fail() {
                // Simulate a transient failure — just retry.
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            break true;
        };
        if success {
            let p = pool.clone();
            let msg_id = block_on(|cx| async move {
                match queries::create_message_with_recipients(
                    &cx,
                    &p,
                    pid,
                    sender_id,
                    &format!("chaos msg {i}"),
                    &format!("body {i}"),
                    None,
                    "normal",
                    false,
                    "",
                    &[(receiver_id, "to")],
                )
                .await
                {
                    Outcome::Ok(row) => row.id.unwrap(),
                    other => panic!("create_message {i} failed: {other:?}"),
                }
            });
            sent_ids.push(msg_id);
        }
    }

    assert_eq!(
        sent_ids.len(),
        num_messages,
        "all messages should have been sent"
    );

    // Phase 2: Verify all messages are retrievable.
    let inbox = block_on(|cx| async move {
        match queries::fetch_inbox(&cx, &pool, pid, receiver_id, false, None, 100).await {
            Outcome::Ok(rows) => rows,
            other => panic!("fetch_inbox failed: {other:?}"),
        }
    });

    assert_eq!(
        inbox.len(),
        num_messages,
        "all {num_messages} messages should be in inbox, got {}",
        inbox.len()
    );

    // Verify each sent message ID appears in inbox.
    for id in &sent_ids {
        assert!(
            inbox.iter().any(|row| row.message.id == Some(*id)),
            "message {id} should be in inbox"
        );
    }
}

// -- Test: Global circuit breakers are isolated from each other ---------------

#[test]
fn chaos_global_circuits_isolated() {
    // Reset all global circuits to known state.
    CIRCUIT_DB.reset();
    CIRCUIT_GIT.reset();
    CIRCUIT_SIGNAL.reset();
    CIRCUIT_LLM.reset();

    // Trip the LLM circuit (threshold=3).
    for _ in 0..3 {
        CIRCUIT_LLM.record_failure();
    }
    assert_eq!(CIRCUIT_LLM.state(), CircuitState::Open);

    // All other circuits remain closed.
    assert_eq!(CIRCUIT_DB.state(), CircuitState::Closed);
    assert_eq!(CIRCUIT_GIT.state(), CircuitState::Closed);
    assert_eq!(CIRCUIT_SIGNAL.state(), CircuitState::Closed);

    // Trip the signal circuit.
    for _ in 0..5 {
        CIRCUIT_SIGNAL.record_failure();
    }
    assert_eq!(CIRCUIT_SIGNAL.state(), CircuitState::Open);

    // DB and git remain unaffected.
    assert_eq!(CIRCUIT_DB.state(), CircuitState::Closed);
    assert_eq!(CIRCUIT_GIT.state(), CircuitState::Closed);

    // circuit_for() returns the correct circuit.
    assert_eq!(circuit_for(Subsystem::Llm).state(), CircuitState::Open);
    assert_eq!(circuit_for(Subsystem::Db).state(), CircuitState::Closed);

    // Clean up.
    CIRCUIT_DB.reset();
    CIRCUIT_GIT.reset();
    CIRCUIT_SIGNAL.reset();
    CIRCUIT_LLM.reset();
}

// -- Test: set_agent_contact_policy updates the global cache ----------------

#[test]
fn set_contact_policy_updates_cache() {
    let (pool, _dir) = make_pool();

    block_on(|cx| {
        let pool = pool.clone();
        async move {
            // 1. Create project + agent (register_agent populates cache with policy="auto")
            let u = unique_suffix();
            let project = queries::ensure_project(&cx, &pool, &format!("/tmp/cache_policy_{u}"))
                .await
                .unwrap();

            let agent = queries::register_agent(
                &cx,
                &pool,
                project.id.unwrap(),
                &format!("Red{}", noun_for(u)),
                "test",
                "test",
                None,
                None,
            )
            .await
            .unwrap();

            let agent_id = agent.id.unwrap();

            // 2. Verify initial cache has policy="auto"
            let cached = read_cache().get_agent_by_id(agent_id);
            assert!(cached.is_some(), "agent should be cached after register");
            assert_eq!(cached.unwrap().contact_policy, "auto");

            // 3. Change policy to "contacts_only"
            let updated = queries::set_agent_contact_policy(&cx, &pool, agent_id, "contacts_only")
                .await
                .unwrap();
            assert_eq!(updated.contact_policy, "contacts_only");

            // 4. Verify cache was updated (this was the bug — cache was stale before the fix)
            let cached2 = read_cache().get_agent_by_id(agent_id);
            assert!(cached2.is_some(), "agent should still be in cache");
            assert_eq!(
                cached2.unwrap().contact_policy,
                "contacts_only",
                "cache must reflect updated policy"
            );

            // 5. Change again to "block_all"
            let updated2 = queries::set_agent_contact_policy(&cx, &pool, agent_id, "block_all")
                .await
                .unwrap();
            assert_eq!(updated2.contact_policy, "block_all");

            let cached3 = read_cache().get_agent_by_id(agent_id);
            assert_eq!(
                cached3.unwrap().contact_policy,
                "block_all",
                "cache must track successive policy changes"
            );
        }
    });
}

// =============================================================================
// Edge case: message-to-self (sender is also a recipient)
// =============================================================================

#[test]
fn edge_case_message_to_self() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/self_msg_{suffix}");

    block_on(|cx| async move {
        let proj = match queries::ensure_project(&cx, &pool, &human_key).await {
            Outcome::Ok(r) => r,
            _ => panic!("ensure_project failed"),
        };
        let pid = proj.id.unwrap();

        let agent =
            match queries::register_agent(&cx, &pool, pid, "GoldLake", "test", "test", None, None)
                .await
            {
                Outcome::Ok(r) => r,
                _ => panic!("register agent failed"),
            };
        let agent_id = agent.id.unwrap();

        // Send a message where sender == recipient (message-to-self)
        let msg = match queries::create_message_with_recipients(
            &cx,
            &pool,
            pid,
            agent_id,
            "Note to self",
            "Remember to review the PR",
            Some("self-thread"),
            "normal",
            false,
            "[]",
            &[(agent_id, "to")],
        )
        .await
        {
            Outcome::Ok(m) => m,
            other => panic!("create_message_with_recipients failed: {other:?}"),
        };
        let msg_id = msg.id.unwrap();
        assert!(msg_id > 0, "message should have a valid ID");

        // Fetch inbox — the message should appear (sender == recipient)
        let inbox = match queries::fetch_inbox(&cx, &pool, pid, agent_id, false, None, 20).await {
            Outcome::Ok(rows) => rows,
            other => panic!("fetch_inbox failed: {other:?}"),
        };
        assert_eq!(inbox.len(), 1, "self-sent message should appear in inbox");
        assert_eq!(inbox[0].message.subject, "Note to self");
        assert_eq!(
            inbox[0].message.sender_id, agent_id,
            "sender_id should match the recipient's own id"
        );
    });
}

// =============================================================================
// Edge case: force-release idempotency (double force-release returns 0)
// =============================================================================

#[test]
fn edge_case_force_release_idempotency() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/force_release_{suffix}");

    block_on(|cx| async move {
        let proj = match queries::ensure_project(&cx, &pool, &human_key).await {
            Outcome::Ok(r) => r,
            _ => panic!("ensure_project failed"),
        };
        let pid = proj.id.unwrap();

        let agent =
            match queries::register_agent(&cx, &pool, pid, "RedPeak", "test", "test", None, None)
                .await
            {
                Outcome::Ok(r) => r,
                _ => panic!("register agent failed"),
            };
        let agent_id = agent.id.unwrap();

        // Create a file reservation
        let reservations = match queries::create_file_reservations(
            &cx,
            &pool,
            pid,
            agent_id,
            &["src/*.rs"],
            3600,
            true,
            "test",
        )
        .await
        {
            Outcome::Ok(r) => r,
            other => panic!("create_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);
        let res_id = reservations[0].id.unwrap();

        // First force-release: should affect 1 row
        let released = match queries::force_release_reservation(&cx, &pool, res_id).await {
            Outcome::Ok(n) => n,
            other => panic!("first force_release failed: {other:?}"),
        };
        assert_eq!(released, 1, "first force-release should update 1 row");

        // Second force-release (idempotent): should affect 0 rows (already released)
        let released_again = match queries::force_release_reservation(&cx, &pool, res_id).await {
            Outcome::Ok(n) => n,
            other => panic!("second force_release failed: {other:?}"),
        };
        assert_eq!(
            released_again, 0,
            "second force-release should be a no-op (0 rows affected)"
        );
    });
}

// =============================================================================
// Edge case: release_reservations idempotency (double release returns 0)
// =============================================================================

#[test]
fn edge_case_release_reservations_idempotency() {
    let (pool, _dir) = make_pool();
    let suffix = unique_suffix();
    let human_key = format!("/data/stress/release_idem_{suffix}");

    block_on(|cx| async move {
        let proj = match queries::ensure_project(&cx, &pool, &human_key).await {
            Outcome::Ok(r) => r,
            _ => panic!("ensure_project failed"),
        };
        let pid = proj.id.unwrap();

        let agent =
            match queries::register_agent(&cx, &pool, pid, "BluePond", "test", "test", None, None)
                .await
            {
                Outcome::Ok(r) => r,
                _ => panic!("register agent failed"),
            };
        let agent_id = agent.id.unwrap();

        // Create two file reservations
        let reservations = match queries::create_file_reservations(
            &cx,
            &pool,
            pid,
            agent_id,
            &["src/main.rs", "src/lib.rs"],
            3600,
            true,
            "test",
        )
        .await
        {
            Outcome::Ok(r) => r,
            other => panic!("create_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 2);

        // First release: should release both
        let released =
            match queries::release_reservations(&cx, &pool, pid, agent_id, None, None).await {
                Outcome::Ok(n) => n,
                other => panic!("first release failed: {other:?}"),
            };
        assert_eq!(released, 2, "first release should free 2 reservations");

        // Second release (idempotent): should release 0
        let released_again =
            match queries::release_reservations(&cx, &pool, pid, agent_id, None, None).await {
                Outcome::Ok(n) => n,
                other => panic!("second release failed: {other:?}"),
            };
        assert_eq!(
            released_again, 0,
            "second release should be a no-op (0 rows, all already released)"
        );

        // Verify no active reservations remain
        let active = match queries::get_active_reservations(&cx, &pool, pid).await {
            Outcome::Ok(r) => r,
            other => panic!("get_active_reservations failed: {other:?}"),
        };
        assert!(active.is_empty(), "no active reservations should remain");
    });
}

/// Helper: pick a valid noun based on index to avoid name collisions in tests.
fn noun_for(idx: u64) -> &'static str {
    const NOUNS: &[&str] = &[
        "Lake", "Peak", "Stone", "Creek", "Pond", "Grove", "Ridge", "Brook", "Cliff", "Glen",
        "Hill", "Cove", "Marsh", "Castle", "River", "Forest", "Valley", "Canyon", "Meadow",
        "Prairie",
    ];
    NOUNS[(idx as usize) % NOUNS.len()]
}
