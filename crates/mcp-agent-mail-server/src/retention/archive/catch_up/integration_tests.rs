//! Exercise the turn executor with the real live-DB/archive reconciler.
//! The injected clocks remove machine-speed dependence where needed; these are
//! correctness tests, not a throughput benchmark or release-smoke swarm verdict.

use super::super::schedule::{Lane, RepairSchedule};
use super::*;
use asupersync::Outcome;
use fastmcp_core::block_on;
use mcp_agent_mail_db::sqlmodel_core::Value as DbValue;
use mcp_agent_mail_db::{DbPoolConfig, create_pool, now_micros, queries};
use mcp_agent_mail_storage::recovery::agent_reconcile::AgentReconcileCursor;
use mcp_agent_mail_storage::recovery::reservation_reconcile::ReservationReconcileCursor;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::Ordering;

struct Fixture {
    cx: Cx,
    pool: DbPool,
    config: Config,
    // Keep the source and archive alive until after the pool is dropped.
    _temp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("catch-up.sqlite3");
    let storage_root = temp.path().join("archive");
    std::fs::create_dir_all(&storage_root).unwrap();
    let pool_config = DbPoolConfig {
        database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&db_path),
        storage_root: Some(storage_root.clone()),
        min_connections: 1,
        max_connections: 1,
        ..DbPoolConfig::default()
    };
    let pool = create_pool(&pool_config).unwrap();
    let cx = Cx::for_testing();
    let project_root = temp.path().join("project");
    std::fs::create_dir(&project_root).unwrap();
    let human_key = project_root.to_string_lossy().into_owned();
    let project = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
        &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1")],
        || match block_on(queries::ensure_project(&cx, &pool, &human_key)) {
            Outcome::Ok(project) => project,
            other => panic!("ensure_project failed: {other:?}"), // ubs:ignore — cfg(test)-only fixture must fail if real project setup fails
        },
    );
    let project_id = project.id.unwrap();
    let mut agents = Vec::new();
    for name in ["BlueLake", "GreenStone"] {
        let agent = match block_on(queries::register_agent(
            &cx, &pool, project_id, name, "test", "test", None, None, None,
        )) {
            Outcome::Ok(agent) => agent,
            other => panic!("register_agent failed: {other:?}"), // ubs:ignore — cfg(test)-only fixture must fail if real agent registration fails
        };
        agents.push(agent.id.unwrap());
    }
    let conn = match block_on(pool.acquire(&cx)) {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => panic!("acquire failed: {error}"), // ubs:ignore — cfg(test)-only fixture acquisition failure must fail the test
        _ => panic!("acquire was cancelled or panicked"), // ubs:ignore — cfg(test)-only fixture must reject cancellation/panic
    };
    // These rows model mail accepted just before a crash: inside the ordinary
    // 30-second grace, but before the restarted worker's settled-write anchor.
    let created_ts = now_micros().saturating_sub(1_000_000);
    for id in 9001_i64..=9009 {
        conn.execute_raw(&format!(
            "INSERT INTO messages \
             (id, project_id, sender_id, thread_id, subject, body_md, importance, \
              ack_required, created_ts, attachments, archive_metadata_json) \
             VALUES ({id}, {project_id}, {}, 'burst-thread', 'subject-{id}', \
                     'body-{id}', 'normal', 1, {created_ts}, '[]', '{{}}')",
            agents[0]
        ))
        .unwrap();
        conn.execute_raw(&format!(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
             VALUES ({id}, {}, 'to', NULL, NULL)",
            agents[1]
        ))
        .unwrap();
    }
    drop(conn);
    Fixture {
        cx,
        pool,
        config: Config {
            database_url: pool_config.database_url,
            storage_root,
            ..Config::default()
        },
        _temp: temp,
    }
}

fn assert_source_untouched(fixture: &Fixture) {
    let conn = match block_on(fixture.pool.acquire(&fixture.cx)) {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => panic!("acquire failed: {error}"), // ubs:ignore — cfg(test)-only fixture acquisition failure must fail the test
        _ => panic!("acquire was cancelled or panicked"), // ubs:ignore — cfg(test)-only fixture must reject cancellation/panic
    };
    let rows = conn
        .query_sync("SELECT id, subject, body_md FROM messages ORDER BY id", &[])
        .unwrap();
    assert_eq!(
        rows.len(),
        9,
        "repair must neither delete nor redeliver mail"
    );
    for (row, id) in rows.iter().zip(9001_i64..=9009) {
        assert_eq!(row.get_named::<i64>("id").unwrap(), id);
        assert_eq!(
            row.get_named::<String>("subject").unwrap(),
            format!("subject-{id}")
        );
        assert_eq!(
            row.get_named::<String>("body_md").unwrap(),
            format!("body-{id}")
        );
    }
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS n FROM message_recipients \
             WHERE read_ts IS NULL AND ack_ts IS NULL",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 9);
}

fn assert_committed_bundles(fixture: &Fixture, expected_messages: usize) {
    assert_committed_bundles_in_range(fixture, 9001..=9009, expected_messages);
}

fn assert_committed_bundles_in_range(
    fixture: &Fixture,
    ids: std::ops::RangeInclusive<i64>,
    expected_messages: usize,
) {
    let mut copies = BTreeMap::<i64, usize>::new();
    for entry in walkdir::WalkDir::new(&fixture.config.storage_root).follow_links(false) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "md")
        {
            continue;
        }
        let bytes = std::fs::read(entry.path()).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let Some((frontmatter, body)) = text
            .strip_prefix("---json\n")
            .and_then(|text| text.split_once("\n---\n\n"))
        else {
            continue;
        };
        let message: Value = serde_json::from_str(frontmatter).unwrap();
        let Some(id) = message["id"].as_i64() else {
            continue;
        };
        assert!(ids.contains(&id));
        assert_eq!(message["subject"], json!(format!("subject-{id}")));
        assert_eq!(body, format!("body-{id}"));
        assert_eq!(message["thread_id"], json!("burst-thread"));
        assert_eq!(message["to"], json!(["GreenStone"]));
        assert_eq!(message["bcc"], json!([]));
        assert_eq!(message["ack_required"], json!(true));
        assert!(message.get("reply_to").is_none());
        // cat-file reads HEAD's raw blob, not the index or working tree. Using
        // the file's directory also covers either supported archive Git layout.
        let output = Command::new("git")
            .arg("-C")
            .arg(entry.path().parent().unwrap())
            .args(["cat-file", "blob"])
            .arg(format!("HEAD:./{}", entry.file_name().to_string_lossy()))
            .output()
            .expect("Git is required for archive integration tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout, bytes,
            "disk and committed archive must agree"
        );
        *copies.entry(id).or_default() += 1;
    }
    assert_eq!(copies.len(), expected_messages);
    assert!(
        copies.values().all(|copies| *copies == 3),
        "canonical, outbox and inbox copies are required"
    );
}

#[test]
fn real_crash_orphans_converge_across_checkpointed_batches_without_client_reads() {
    let fixture = fixture();
    let stop = AtomicBool::new(false);
    let mut cursor = ReconcileCursor::settled_before(now_micros());
    let mut reports = Vec::new();
    let backlog = run_with(
        || {
            reconcile_message_batch(
                &fixture.cx,
                &fixture.pool,
                &fixture.config,
                &mut cursor,
                &stop,
            )
        },
        |report| reports.push((report.repaired, report.deferred, report.checkpoint_saved)),
        || Duration::ZERO,
    )
    .unwrap();
    assert!(!backlog);
    assert!(
        reports.len() >= 3,
        "nine repairs require multiple four-repair batches"
    );
    assert_eq!(reports.iter().map(|report| report.0).sum::<usize>(), 9);
    assert!(
        reports
            .iter()
            .all(|report| report.0 <= 4 && report.1 == 0 && report.2 == Some(true))
    );
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 9);
}

fn insert_tail_messages(fixture: &Fixture, ids: impl Iterator<Item = i64>) {
    let conn = match block_on(fixture.pool.acquire(&fixture.cx)) {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => panic!("acquire failed: {error}"), // ubs:ignore — cfg(test)-only fixture acquisition failure must fail the test
        _ => panic!("acquire was cancelled or panicked"), // ubs:ignore — cfg(test)-only fixture must reject cancellation/panic
    };
    for id in ids {
        // Reuse only the fixture's project/agent identities. Old timestamps
        // make these rows eligible without sleeps or changing the grace policy.
        conn.execute_raw(&format!(
            "INSERT INTO messages \
             (id, project_id, sender_id, thread_id, subject, body_md, importance, \
              ack_required, created_ts, attachments, archive_metadata_json) \
             SELECT {id}, project_id, sender_id, 'burst-thread', 'subject-{id}', \
                    'body-{id}', 'normal', 1, 1000000, '[]', '{{}}' \
             FROM messages WHERE id = 9001"
        ))
        .unwrap();
        conn.execute_raw(&format!(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
             SELECT {id}, agent_id, kind, NULL, NULL \
             FROM message_recipients WHERE message_id = 9001"
        ))
        .unwrap();
    }
}

fn source_snapshot(fixture: &Fixture) -> [Vec<Vec<DbValue>>; 2] {
    let conn = match block_on(fixture.pool.acquire(&fixture.cx)) {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => panic!("acquire failed: {error}"), // ubs:ignore — cfg(test)-only fixture acquisition failure must fail the test
        _ => panic!("acquire was cancelled or panicked"), // ubs:ignore — cfg(test)-only fixture must reject cancellation/panic
    };
    [
        "SELECT * FROM messages ORDER BY id",
        "SELECT * FROM message_recipients ORDER BY message_id, agent_id",
    ]
    .map(|sql| {
        conn.query_sync(sql, &[])
            .unwrap()
            .iter()
            .map(|row| row.values().cloned().collect())
            .collect()
    })
}

#[test]
#[allow(clippy::too_many_lines)] // Keep the restart, scheduling and committed-byte witnesses together.
fn restored_forward_tail_reaches_orphan_after_history_ends_without_client_reads() {
    // Preserve the reconciler's real reason if this fixture defers work.
    // RUST_LOG alone has no effect in a libtest process without a subscriber.
    #[derive(Default)]
    struct RepairWarnings(std::sync::atomic::AtomicU64);
    impl tracing::Subscriber for RepairWarnings {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == "maintenance" && *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(self.0.fetch_add(1, Ordering::Relaxed) + 1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            event.record(
                &mut |field: &tracing::field::Field, value: &dyn std::fmt::Debug| {
                    eprintln!("{}={value:?}", field.name());
                },
            );
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }
    let _warnings = tracing::subscriber::set_default(RepairWarnings::default());
    let fixture = fixture();
    let stop = AtomicBool::new(false);
    let mut before_restart = ReconcileCursor::settled_before(now_micros());
    let mut repaired = 0;
    for _ in 0..9 {
        let report = reconcile_message_batch(
            &fixture.cx,
            &fixture.pool,
            &fixture.config,
            &mut before_restart,
            &stop,
        )
        .unwrap();
        assert_eq!(report.deferred, 0);
        assert_eq!(report.checkpoint_saved, Some(true));
        repaired += report.repaired;
        if repaired == 9 {
            break;
        }
    }
    assert_eq!(repaired, 9);
    assert_committed_bundles(&fixture, 9);
    drop(before_restart);

    // Inspect the checkpoint actually published by the production entrypoint.
    // The next history query will be empty, while the forward cursor is still
    // at the last message accepted before the simulated restart.
    let checkpoint_paths = std::fs::read_dir(
        std::path::Path::new(fixture.pool.sqlite_path())
            .parent()
            .unwrap(),
    )
    .unwrap()
    .map(|entry| entry.unwrap().path())
    .filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(".am-message-reconcile-")
                    && std::path::Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext == "json")
            })
    })
    .collect::<Vec<_>>();
    assert_eq!(checkpoint_paths.len(), 1);
    let checkpoint_bytes = std::fs::read(&checkpoint_paths[0]).unwrap();
    let checkpoint: Value = serde_json::from_slice(&checkpoint_bytes).unwrap();
    assert_eq!(checkpoint["tail_after"], json!(9009));
    assert_eq!(checkpoint["backfill_ceiling"], json!(9000));

    // Build a healthy forward backlog without touching that durable cursor.
    // The ordinary, non-checkpointed public entrypoint supplies all archive/Git
    // work; this fixture never fabricates archive bytes or a checkpoint schema.
    insert_tail_messages(&fixture, (9010..=9074).filter(|id| *id != 9042));
    let mut preparation = ReconcileCursor::default();
    let mut prepared = 0;
    for _ in 0..32 {
        let report = reconcile_message_batch(
            &fixture.cx,
            &fixture.pool,
            &fixture.config,
            &mut preparation,
            &stop,
        )
        .unwrap();
        assert_eq!(report.deferred, 0);
        assert_eq!(report.checkpoint_saved, None);
        prepared += report.repaired;
        if prepared == 64 {
            break;
        }
    }
    assert_eq!(prepared, 64);
    assert_eq!(
        std::fs::read(&checkpoint_paths[0]).unwrap(),
        checkpoint_bytes
    );
    assert_committed_bundles_in_range(&fixture, 9001..=9074, 73);
    insert_tail_messages(&fixture, std::iter::once(9042));
    let conn = match block_on(fixture.pool.acquire(&fixture.cx)) {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => panic!("acquire failed: {error}"), // ubs:ignore — cfg(test)-only fixture acquisition failure must fail the test
        _ => panic!("acquire was cancelled or panicked"), // ubs:ignore — cfg(test)-only fixture must reject cancellation/panic
    };
    conn.execute_raw(
        "UPDATE message_recipients SET read_ts = 2000000, ack_ts = 2500000 \
         WHERE message_id % 2 = 0",
    )
    .unwrap();
    drop(conn);
    let source_before = source_snapshot(&fixture);
    assert_eq!(source_before[0].len(), 74);
    assert_eq!(source_before[1].len(), 74);

    let mut restarted = ReconcileCursor::settled_before(now_micros());
    let mut reports = Vec::new();
    let backlog = run_with(
        || {
            reconcile_message_batch(
                &fixture.cx,
                &fixture.pool,
                &fixture.config,
                &mut restarted,
                &stop,
            )
        },
        |report| {
            assert!(!report.interrupted);
            assert_eq!(report.deferred, 0);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert!(report.scanned <= 32);
            assert!(report.repaired <= 4);
            assert!(report.payload_bytes <= 16 * 1024 * 1024);
            reports.push((
                report.unchanged,
                report.repaired,
                report.budget_exhausted,
                report.checkpoint_restored,
            ));
        },
        || Duration::ZERO,
    )
    .unwrap();
    assert!(reports[0].3, "the real durable cursor must be restored");
    // Startup priority checks sixteen newest settled rows alongside the
    // original sixteen forward rows; neither page contains orphan 9042.
    assert_eq!(reports[0].0, 32);
    assert_eq!(reports[0].1, 0);
    assert!(
        reports[0].2,
        "ending history must not hide the remaining eligible forward tail"
    );
    assert!(reports.len() >= 3);
    assert_eq!(reports[1].1, 0, "a second healthy page precedes the orphan");
    assert_eq!(reports.iter().map(|report| report.1).sum::<usize>(), 1);
    assert!(
        !backlog,
        "the finite healthy tail must eventually permit idle"
    );
    assert_eq!(source_snapshot(&fixture), source_before);
    assert_committed_bundles_in_range(&fixture, 9001..=9074, 74);

    // A subsequent ordinary rotation is healthy, not a permanent startup loop.
    let mut idle_batches = 0;
    assert!(
        !run_with(
            || {
                reconcile_message_batch(
                    &fixture.cx,
                    &fixture.pool,
                    &fixture.config,
                    &mut restarted,
                    &stop,
                )
            },
            |report| {
                idle_batches += 1;
                assert_eq!(report.repaired, 0);
                assert_eq!(report.deferred, 0);
                assert!(!report.budget_exhausted);
            },
            || Duration::ZERO,
        )
        .unwrap()
    );
    assert_eq!(idle_batches, 1);
    assert_eq!(source_snapshot(&fixture), source_before);
}

#[test]
fn shutdown_between_real_batches_leaves_remaining_source_mail_untouched() {
    let fixture = fixture();
    let stop = AtomicBool::new(false);
    let mut cursor = ReconcileCursor::settled_before(now_micros());
    let mut observed = Vec::new();
    let backlog = run_with(
        || {
            reconcile_message_batch(
                &fixture.cx,
                &fixture.pool,
                &fixture.config,
                &mut cursor,
                &stop,
            )
        },
        |report| {
            observed.push((report.repaired, report.interrupted));
            stop.store(true, Ordering::Release);
        },
        || Duration::ZERO,
    )
    .unwrap();
    assert!(!backlog, "shutdown must not request catch-up polling");
    assert_eq!(observed, [(4, false), (0, true)]);
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 4);
}

#[test]
#[allow(clippy::too_many_lines)] // Keep the lock/pool/committed-byte witness in one test.
fn real_checkpoint_lock_contention_yields_then_resumes_the_same_source_and_cursor() {
    let fixture = fixture();
    let stop = AtomicBool::new(false);
    let mut cursor = ReconcileCursor::settled_before(now_micros());
    let first = reconcile_message_batch(
        &fixture.cx,
        &fixture.pool,
        &fixture.config,
        &mut cursor,
        &stop,
    )
    .unwrap();
    assert_eq!(first.repaired, 4);
    assert_eq!(first.checkpoint_saved, Some(true));
    assert_committed_bundles(&fixture, 4);

    // Use the checkpoint actually created by the production storage pass,
    // rather than duplicating its source/root binding or filename algorithm.
    let database = std::path::Path::new(fixture.pool.sqlite_path());
    let lock_paths = std::fs::read_dir(database.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(".am-message-reconcile-") && name.ends_with(".json.lock")
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(lock_paths.len(), 1, "one source-bound checkpoint lock");
    let lock_path = &lock_paths[0];
    let checkpoint_path = lock_path.with_extension("");
    let before_checkpoint = std::fs::read(&checkpoint_path).unwrap();
    let owner = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&owner).unwrap();

    // The real OS lock must become a short follow-up, not a source failure.
    // No storage pass completed, so neither a report nor checkpoint progress
    // may be invented. The same live pool must remain usable afterwards.
    let mut reports = 0;
    let backlog = super::run(
        &fixture.cx,
        &fixture.pool,
        &fixture.config,
        &mut cursor,
        &stop,
        &MaintenanceTurn::new(Instant::now()),
        |_| reports += 1,
    )
    .expect("checkpoint contention must not retire the live source pool");
    assert_eq!(backlog, WorkHint::Contended { progressed: false });
    assert_eq!(reports, 0);
    assert_eq!(std::fs::read(&checkpoint_path).unwrap(), before_checkpoint);
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 4);

    // GH#354: repeated REAL checkpoint refusals must wake only the message
    // lane, not multiply healthy agent/reservation scans by sixty. Invoke
    // the actual storage-backed peer turns on the same one-connection pool.
    // Only the outer clock is virtual; no SQL, archive or lock is mocked.
    let mut agent_cursor = AgentReconcileCursor::default();
    let mut reservation_cursor = ReservationReconcileCursor::default();
    let start = Instant::now();
    let mut schedule = RepairSchedule::new(Duration::from_mins(1), true, true, true);
    let mut peer_turns = [0usize; 2];
    let mut peer_settled_at = [None; 2];
    let mut message_turns = 0usize;
    for second in 0..60 {
        let now = start + Duration::from_secs(second);
        let result = schedule.run_if_due(
            Lane::Messages,
            || now,
            || {
                message_turns += 1;
                Some(super::run(
                    &fixture.cx,
                    &fixture.pool,
                    &fixture.config,
                    &mut cursor,
                    &stop,
                    &MaintenanceTurn::new(Instant::now()),
                    |_| reports += 1,
                ))
            },
        );
        if [0, 1, 3, 7, 15, 31].contains(&second) {
            assert_eq!(result, Some(Ok(WorkHint::Contended { progressed: false })));
        } else {
            assert_eq!(result, None);
        }
        let agent = schedule.run_if_due(
            Lane::Agents,
            || now,
            || {
                peer_turns[0] += 1;
                Some(super::super::agent_catch_up::run(
                    &fixture.cx,
                    &fixture.pool,
                    &fixture.config,
                    &mut agent_cursor,
                    &stop,
                    &MaintenanceTurn::new(Instant::now()),
                    |report| {
                        assert!(!report.interrupted);
                        assert_eq!(report.deferred, 0);
                    },
                ))
            },
        );
        let reservation = schedule.run_if_due(
            Lane::Reservations,
            || now,
            || {
                peer_turns[1] += 1;
                Some(super::super::reservation_catch_up::run(
                    &fixture.cx,
                    &fixture.pool,
                    &fixture.config,
                    &mut reservation_cursor,
                    &stop,
                    &MaintenanceTurn::new(Instant::now()),
                    |report| {
                        assert!(!report.interrupted);
                        assert_eq!(report.deferred, 0);
                    },
                ))
            },
        );
        for (index, result) in [agent, reservation].into_iter().enumerate() {
            match result {
                Some(Ok(WorkHint::Idle)) => {
                    assert!(
                        peer_settled_at[index].replace(second).is_none(),
                        "healthy peer ran again before its minute deadline"
                    );
                }
                Some(Ok(hint)) if hint.needs_follow_up() => {
                    // A slow first profile publication may legitimately
                    // consume its turn budget. Let that real backlog drain;
                    // never assume the fixture's Git writes fit in 250 ms.
                    assert!(peer_settled_at[index].is_none());
                }
                None => assert!(peer_settled_at[index].is_some()),
                Some(Err(error)) => panic!("peer source failed: {error}"),
                Some(Ok(other)) => panic!("unexpected healthy-peer scheduling result: {other:?}"),
            }
        }
        assert!(
            (Duration::from_secs(1)..=Duration::from_secs(60)).contains(&schedule.next_delay(now))
        );
    }
    assert_eq!(
        message_turns, 6,
        "persistent admission contention must back off"
    );
    for (turns, settled_at) in peer_turns.into_iter().zip(peer_settled_at) {
        let settled_at = settled_at.expect("finite peer fixture must become healthy");
        assert_eq!(turns, usize::try_from(settled_at).unwrap() + 1);
        assert!(turns < 60, "healthy peer never got an idle interval");
    }
    assert_eq!(reports, 0, "a refused source produced a fake report");
    assert_eq!(std::fs::read(&checkpoint_path).unwrap(), before_checkpoint);
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 4);
    drop(owner);

    // Use the production entry point again, retaining this pool and cursor.
    // A slow machine may yield after each batch; the finite turn count is a
    // correctness bound for nine rows, not an elapsed-time performance gate.
    let mut finished = false;
    let mut repaired = 0;
    for second in 60..92 {
        let now = start + Duration::from_secs(second);
        let outcome = schedule.run_if_due(
            Lane::Messages,
            || now,
            || {
                Some(super::run(
                    &fixture.cx,
                    &fixture.pool,
                    &fixture.config,
                    &mut cursor,
                    &stop,
                    &MaintenanceTurn::new(Instant::now()),
                    |report| {
                        assert!(!report.interrupted);
                        assert_eq!(report.deferred, 0);
                        assert_eq!(report.checkpoint_saved, Some(true));
                        assert!(report.repaired <= 4);
                        repaired += report.repaired;
                    },
                ))
            },
        );
        let Some(outcome) = outcome else { continue };
        if !outcome.unwrap().needs_follow_up() {
            finished = true;
            break;
        }
    }
    assert!(
        finished,
        "released checkpoint ownership must not pin catch-up"
    );
    assert_eq!(repaired, 5, "do not replay the four completed repairs");
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 9);
}

/// Isolate CPU accounting from other tests' engine threads. A receipt written
/// only by the completed child prevents an incorrect --exact filter from
/// reporting success after running zero tests. The parent bounds even a stuck
/// query/close and always reaps its own child on failure.
#[cfg(target_os = "linux")]
fn run_idle_cpu_child(receipt: &std::path::Path) {
    struct OwnedChild(Option<std::process::Child>);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let (_, module) = module_path!().split_once("::").expect("nested test module");
    let name = format!("{module}::gh354_idle_database_workers_park_and_serve_after_idle");
    let mut child = OwnedChild(Some(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env("AM_GH354_IDLE_CPU_CHILD_RECEIPT", receipt)
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.as_mut().unwrap().try_wait().unwrap() {
            // try_wait has already reaped it. Do not signal a reaped PID.
            drop(child.0.take());
            assert!(status.success(), "idle CPU child failed: {status}");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_mins(2),
            "idle CPU child did not complete within its bounded lifetime"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let receipt: Value = serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
    assert_eq!(receipt["recovered_messages"], json!(9));
}

#[cfg(target_os = "linux")]
fn process_cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    // comm can contain spaces and parentheses; fields after its LAST ')' start
    // at field 3 (state). utime/stime are fields 14/15, indices 11/12 here.
    let (_, fields) = stat.rsplit_once(')').unwrap();
    let fields: Vec<_> = fields.split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

#[cfg(target_os = "linux")]
fn assert_engine_worker_present() {
    let present = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            std::fs::read_to_string(entry.path().join("comm"))
                .is_ok_and(|name| name.trim() == "fsqlite-worker")
        });
    assert!(present, "idle CPU test did not retain a real engine worker");
}

/// This is a real pooled-engine idle/response canary, not an HTTP-server or
/// populated-production-store performance certificate. The separate release
/// probe covers whole-daemon idle CPU with its actual background workers.
#[cfg(target_os = "linux")]
#[test]
fn gh354_idle_database_workers_park_and_serve_after_idle() {
    let Some(receipt) = std::env::var_os("AM_GH354_IDLE_CPU_CHILD_RECEIPT") else {
        let temp = tempfile::tempdir().unwrap();
        run_idle_cpu_child(&temp.path().join("idle-cpu.json"));
        return;
    };
    let fixture = fixture();
    assert_engine_worker_present();
    let output = Command::new("getconf").arg("CLK_TCK").output().unwrap();
    assert!(output.status.success(), "getconf CLK_TCK failed");
    let ticks_per_second: u64 = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(ticks_per_second > 0);

    // Exclude initial schema/statement work. Send no SQL during the measured
    // interval. Whole-process counters include CPU in the fsqlite workers,
    // unlike measuring only this sleeping caller thread.
    std::thread::sleep(Duration::from_secs(2));
    let started = Instant::now();
    let before = process_cpu_ticks();
    std::thread::sleep(Duration::from_secs(4));
    let after = process_cpu_ticks();
    let wall_ms = started.elapsed().as_millis();
    assert!(after >= before, "process CPU counters regressed");
    let cpu_ms = u128::from(after - before) * 1_000 / u128::from(ticks_per_second);
    assert_engine_worker_present();
    assert!(
        cpu_ms * 4 <= wall_ms,
        "idle pooled engine exceeded 25% of one core: {cpu_ms} CPU ms / {wall_ms} wall ms"
    );

    let read_started = Instant::now();
    assert_source_untouched(&fixture);
    let first_read_ms = read_started.elapsed().as_millis();
    assert!(
        first_read_ms < 5_000,
        "post-idle reads took {first_read_ms} ms"
    );

    // Idle throttling must not achieve low CPU by abandoning accepted mail.
    // Exercise actual checkpointed catch-up after the quiet interval and
    // verify all canonical/inbox/outbox bytes in committed Git history.
    let stop = AtomicBool::new(false);
    let mut cursor = ReconcileCursor::settled_before(now_micros());
    let mut repaired = 0;
    let mut finished = false;
    for _ in 0..32 {
        let backlog = super::run(
            &fixture.cx,
            &fixture.pool,
            &fixture.config,
            &mut cursor,
            &stop,
            &MaintenanceTurn::new(Instant::now()),
            |report| repaired += report.repaired,
        )
        .unwrap();
        if !backlog.needs_follow_up() {
            finished = true;
            break;
        }
    }
    assert!(finished, "post-idle archive recovery did not finish");
    assert_eq!(repaired, 9);
    assert_source_untouched(&fixture);
    assert_committed_bundles(&fixture, 9);
    std::fs::write(
        receipt,
        serde_json::to_vec(&json!({
            "cpu_ms": cpu_ms,
            "wall_ms": wall_ms,
            "first_read_ms": first_read_ms,
            "recovered_messages": repaired,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Keep the real worker's Linux counters separate from parallel libtest cases.
/// The child receipt also proves that the exact filter actually ran this test.
#[cfg(target_os = "linux")]
fn run_populated_maintenance_child(receipt: &std::path::Path) {
    struct OwnedChild(Option<std::process::Child>);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let (_, module) = module_path!().split_once("::").expect("nested test module");
    let name = format!("{module}::gh383_populated_maintenance_paces_cpu_and_reaches_old_orphan");
    let mut child = OwnedChild(Some(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env("AM_GH383_MAINTENANCE_CPU_RECEIPT", receipt)
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.as_mut().unwrap().try_wait().unwrap() {
            drop(child.0.take());
            assert!(
                status.success(),
                "populated maintenance child failed: {status}"
            );
            break;
        }
        assert!(
            started.elapsed() < Duration::from_mins(5),
            "populated maintenance child exceeded its bounded lifetime"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let receipt: Value = serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
    assert_eq!(receipt["source_messages"], json!(256));
    assert_eq!(receipt["committed_messages"], json!(256));
    assert!(receipt["index_bytes"].as_u64().unwrap() > 30 * 1024 * 1024);
    println!("GH383 populated maintenance receipt: {receipt}");
}

#[cfg(target_os = "linux")]
fn maintenance_thread_stat() -> std::path::PathBuf {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            std::fs::read_to_string(path.join("comm"))
                .is_ok_and(|name| name.trim() == "retention-quota")
        })
        .expect("the production maintenance worker must exist")
        .join("stat")
}

#[cfg(target_os = "linux")]
fn thread_cpu_ticks(stat_path: &std::path::Path) -> u64 {
    let stat = std::fs::read_to_string(stat_path).unwrap();
    let (_, fields) = stat.rsplit_once(')').unwrap();
    let fields: Vec<_> = fields.split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

/// Make a native, committed v2 index of approximately the reported size. These
/// are synthetic Git entries sharing one blob, not 154,845 fabricated messages.
/// Real messages and their three committed copies remain a separate oracle.
#[cfg(target_os = "linux")]
fn add_large_native_index(fixture: &Fixture) -> u64 {
    let conn = match block_on(fixture.pool.acquire(&fixture.cx)) {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => panic!("acquire for index fixture failed: {error}"), // ubs:ignore — cfg(test)-only fixture must reject failed acquisition
        _ => panic!("index fixture acquisition was cancelled or panicked"), // ubs:ignore — cfg(test)-only fixture must reject cancellation/panic
    };
    let rows = conn.query_sync("SELECT slug FROM projects", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let slug = rows[0].get_named::<String>("slug").unwrap();
    drop(conn);
    let archive = mcp_agent_mail_storage::ensure_archive(&fixture.config, &slug).unwrap();
    let repo = git2::Repository::open(&archive.repo_root).unwrap();
    let blob = repo.blob(b"synthetic index-size witness\n").unwrap();
    let mut index = repo.index().unwrap();
    index.set_version(2).unwrap();
    for id in 0..154_845 {
        let mut path = format!("maintenance-fixture/{id:06}-").into_bytes();
        path.resize(144, b'x');
        index
            .add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100_644,
                uid: 0,
                gid: 0,
                file_size: 29,
                id: blob,
                flags: 0,
                flags_extended: 0,
                path,
            })
            .unwrap();
    }
    let tree_id = index.write_tree().unwrap();
    index.write().unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let signature =
        git2::Signature::now("Maintenance test", "maintenance@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "Seed synthetic index-size witness",
        &repo.find_tree(tree_id).unwrap(),
        &[&parent],
    )
    .unwrap();
    std::fs::metadata(repo.path().join("index")).unwrap().len()
}

/// Measure the production retention-quota thread with reporting/quota disabled,
/// a populated archive, a reporter-sized native index and real forward scan
/// debt. Low CPU is insufficient: an old crash orphan behind healthy pages must
/// acquire canonical, inbox and outbox bytes in committed Git without a client
/// read or changes to accepted messages/receipts. This is a component workload,
/// not a whole-daemon or deployment-specific CPU certificate.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::too_many_lines)] // Keep the workload, CPU window and convergence oracles together.
fn gh383_populated_maintenance_paces_cpu_and_reaches_old_orphan() {
    let Some(receipt) = std::env::var_os("AM_GH383_MAINTENANCE_CPU_RECEIPT") else {
        let temp = tempfile::tempdir().unwrap();
        run_populated_maintenance_child(&temp.path().join("maintenance-cpu.json"));
        return;
    };
    let mut fixture = fixture();
    fixture.config.retention_report_enabled = false;
    fixture.config.quota_enabled = false;
    fixture.config.messages_retention_days = 0;
    let stop = AtomicBool::new(false);
    let mut cursor = ReconcileCursor::settled_before(now_micros());
    let mut repaired = 0;
    for _ in 0..16 {
        let report = reconcile_message_batch(
            &fixture.cx,
            &fixture.pool,
            &fixture.config,
            &mut cursor,
            &stop,
        )
        .unwrap();
        assert_eq!(report.deferred, 0);
        assert_eq!(report.checkpoint_saved, Some(true));
        repaired += report.repaired;
        if repaired == 9 {
            break;
        }
    }
    assert_eq!(repaired, 9);
    // Leave the durable forward cursor at 9009. Prepare healthy later archive
    // rows with the ordinary non-checkpointed entrypoint, then accept the hole.
    insert_tail_messages(&fixture, (9010..=9256).filter(|id| *id != 9128));
    let mut preparation = ReconcileCursor::default();
    let mut prepared = 0;
    for _ in 0..256 {
        let report = reconcile_message_batch(
            &fixture.cx,
            &fixture.pool,
            &fixture.config,
            &mut preparation,
            &stop,
        )
        .unwrap();
        assert_eq!(report.deferred, 0);
        assert_eq!(report.checkpoint_saved, None);
        prepared += report.repaired;
        if prepared == 246 {
            break;
        }
    }
    assert_eq!(prepared, 246);
    insert_tail_messages(&fixture, std::iter::once(9128));
    let source_before = source_snapshot(&fixture);
    assert_eq!(source_before[0].len(), 256);
    let index_bytes = add_large_native_index(&fixture);
    assert!(index_bytes > 30 * 1024 * 1024);

    let output = Command::new("getconf").arg("CLK_TCK").output().unwrap();
    assert!(output.status.success());
    let ticks_per_second: u64 = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(ticks_per_second > 0);
    let (cpu_ms, wall_ms, convergence_ms) =
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("AM_MESSAGE_ARCHIVE_RECONCILE_ENABLED", "true"),
                ("AM_AGENT_ARCHIVE_RECONCILE_ENABLED", "true"),
                ("AM_RESERVATION_ARCHIVE_RECONCILE_ENABLED", "true"),
            ],
            || {
                struct Worker;
                impl Drop for Worker {
                    fn drop(&mut self) {
                        super::super::shutdown();
                    }
                }
                super::super::start(&fixture.config);
                let worker = Worker;
                let stat = maintenance_thread_stat();
                // Start just before the worker's ten-second startup admission;
                // the measured interval includes actual catch-up and its rest.
                std::thread::sleep(Duration::from_secs(9));
                let started = Instant::now();
                let before = thread_cpu_ticks(&stat);
                std::thread::sleep(Duration::from_secs(30));
                let after = thread_cpu_ticks(&stat);
                let wall_ms = started.elapsed().as_millis();
                assert!(after >= before);
                let cpu_ms = u128::from(after - before) * 1_000 / u128::from(ticks_per_second);
                // The scheduler targets 20% active wall time. Allow a boundary
                // batch overrun and timing noise; the counters are thread CPU,
                // not the caller's sleep or the process's independent DB work.
                assert!(
                    cpu_ms * 100 <= wall_ms * 35,
                    "populated retention thread exceeded 35% of one core: {cpu_ms} / {wall_ms} ms"
                );
                loop {
                    let copies = walkdir::WalkDir::new(&fixture.config.storage_root)
                        .follow_links(false)
                        .into_iter()
                        .map(Result::unwrap)
                        .filter(|entry| entry.file_type().is_file())
                        .filter(|entry| entry.file_name().to_string_lossy().ends_with("__9128.md"))
                        .count();
                    if copies == 3 {
                        break;
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(120),
                        "paced worker did not reach the old orphan behind healthy pages"
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
                let convergence_ms = started.elapsed().as_millis();
                drop(worker);
                (cpu_ms, wall_ms, convergence_ms)
            },
        );
    assert_eq!(source_snapshot(&fixture), source_before);
    assert_committed_bundles_in_range(&fixture, 9001..=9256, 256);
    std::fs::write(
        receipt,
        serde_json::to_vec(&json!({
            "thread_cpu_ms": cpu_ms,
            "wall_ms": wall_ms,
            "convergence_ms": convergence_ms,
            "index_bytes": index_bytes,
            "source_messages": 256,
            "committed_messages": 256,
            "reporting_enabled": false,
            "quota_enabled": false,
        }))
        .unwrap(),
    )
    .unwrap();
}
