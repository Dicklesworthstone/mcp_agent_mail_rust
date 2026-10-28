//! End-to-end live-mailbox recovery under actual promotion/archive contention.

use super::*;
use mcp_agent_mail_db::write_barrier::{
    DrainOutcome, acquire_promotion_barrier_draining, active_writer_count, begin_write_activity,
    try_acquire_promotion_barrier_if_idle,
};
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// Other storage libtests use the same process-wide fence and admission gate.
/// Keep their activity out of tests that deliberately control those owners.
pub(super) fn isolated() -> bool {
    const CHILD: &str = "AM_TEST_MESSAGE_BATCH_ADMISSION_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest thread");
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .output()
        .expect("run isolated message batch test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "isolated {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    true
}

fn fixture(test: impl FnOnce(&Cx, &DbPool, &Config)) {
    if isolated() {
        return;
    }
    mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().join("archive"),
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &temp.path().join("mail.sqlite3"),
            ),
            ..Config::default()
        };
        fs::create_dir_all(&config.storage_root).unwrap();
        let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
            database_url: config.database_url.clone(),
            storage_root: Some(config.storage_root.clone()),
            min_connections: 1,
            max_connections: 1,
            ..Default::default()
        })
        .unwrap();
        let cx = Cx::for_testing();
        let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
        for (project, slug, sender, recipient, bcc, id) in [
            (101, "project", 101, 102, 103, 901),
            (201, "other", 201, 202, 203, 902),
        ] {
            conn.execute_raw(&format!(
                "INSERT INTO projects(id, slug, human_key, created_at) VALUES({project}, '{slug}', '/{slug}', 1)"
            )).unwrap();
            conn.execute_raw(&format!(
                "INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) \
                 VALUES({sender}, {project}, 'BlueLake', 'test', 'test', 1, 1), \
                 ({recipient}, {project}, 'GreenStone', 'test', 'test', 1, 1), \
                 ({bcc}, {project}, 'RedFox', 'test', 'test', 1, 1)"
            )).unwrap();
            conn.execute_raw(&format!(
                "INSERT INTO messages(id, project_id, sender_id, thread_id, topic, subject, body_md, importance, \
                 ack_required, created_ts, recipients_json, attachments, archive_metadata_json) \
                 VALUES({id}, {project}, {sender}, 'thread', 'handoff', 'message-{id}', 'Keep λ and every byte.', \
                 'high', 1, 1000000, '{{\"to\":[\"GreenStone\"],\"cc\":[],\"bcc\":[\"RedFox\"]}}', '[]', '{{\"reply_to\":700}}')"
            )).unwrap();
            conn.execute_raw(&format!(
                "INSERT INTO message_recipients(message_id, agent_id, kind, read_ts, ack_ts) \
                 VALUES({id}, {recipient}, 'to', 7, 11), ({id}, {bcc}, 'bcc', NULL, NULL)"
            ))
            .unwrap();
        }
        drop(conn);
        test(&cx, &pool, &config);
        crate::flush_async_commits();
    });
}

pub(super) fn database_evidence(cx: &Cx, pool: &DbPool) -> Value {
    let conn = outcome(block_on(pool.acquire(cx))).unwrap();
    let messages = conn.query_sync(
        "SELECT id, project_id, sender_id, subject, body_md, thread_id, topic, importance, ack_required, \
         created_ts, recipients_json, attachments, archive_metadata_json FROM messages ORDER BY id",
        &[],
    ).unwrap();
    let messages = messages.iter().map(|row| json!({
        "id": row.get_named::<i64>("id").unwrap(),
        "project_id": row.get_named::<i64>("project_id").unwrap(),
        "sender_id": row.get_named::<i64>("sender_id").unwrap(),
        "subject": row.get_named::<String>("subject").unwrap(),
        "body_md": row.get_named::<String>("body_md").unwrap(),
        "thread_id": row.get_named::<Option<String>>("thread_id").unwrap(),
        "topic": row.get_named::<Option<String>>("topic").unwrap(),
        "importance": row.get_named::<String>("importance").unwrap(),
        "ack_required": row.get_named::<i64>("ack_required").unwrap(),
        "created_ts": row.get_named::<i64>("created_ts").unwrap(),
        "recipients_json": row.get_named::<String>("recipients_json").unwrap(),
        "attachments": row.get_named::<String>("attachments").unwrap(),
        "archive_metadata_json": row.get_named::<Option<String>>("archive_metadata_json").unwrap(),
    })).collect::<Vec<_>>();
    let receipts = conn.query_sync(
        "SELECT message_id, agent_id, kind, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id",
        &[],
    ).unwrap();
    let receipts = receipts
        .iter()
        .map(|row| {
            json!({
                "message_id": row.get_named::<i64>("message_id").unwrap(),
                "agent_id": row.get_named::<i64>("agent_id").unwrap(),
                "kind": row.get_named::<String>("kind").unwrap(),
                "read_ts": row.get_named::<Option<i64>>("read_ts").unwrap(),
                "ack_ts": row.get_named::<Option<i64>>("ack_ts").unwrap(),
            })
        })
        .collect::<Vec<_>>();
    json!({"messages": messages, "receipts": receipts})
}

/// Run with an actual drain blocked at the archive fence. Backdate only the
/// enqueue timestamp so the test exercises minutes of lag without sleeping
/// for minutes; queue accounting, drain, lag sampling and reconciliation are
/// the production paths. `fixture` isolates their process-global state.
fn with_queued_archive_lag(config: &Config, age: Duration, test: impl FnOnce()) {
    crate::wbq_start();
    // Publication admission is reentrant on its owning thread. Keep the owner
    // separate from both this caller and the WBQ so real repair attempts must
    // observe contention instead of inheriting the fixture's authority.
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let owner = std::thread::spawn(move || {
        let _fence = crate::ArchiveMutationGuard::begin();
        ready_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(30))
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let wbq = crate::WBQ.get().unwrap();
    let sender = wbq.sender.lock().unwrap().as_ref().unwrap().clone();
    let op = crate::WriteOp::ClearSignal {
        config: config.clone(),
        project_slug: "project".into(),
        agent_name: "GreenStone".into(),
    };
    assert!(
        crate::wbq_try_send_counted(
            &sender,
            &wbq.op_depth,
            crate::WbqMsg::Op(crate::WbqOpEnvelope {
                enqueued_at: Instant::now().checked_sub(age).unwrap(),
                op: Box::new(op),
            }),
            Some(&wbq.lifecycle),
        )
        .is_ok()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lag = crate::archive_lag_snapshot();
        if lag.wbq_inflight_oldest_age_us >= crate::duration_as_micros_u64(age) {
            assert!(lag.oldest_unmaterialized_us >= lag.wbq_inflight_oldest_age_us);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "WBQ drain never entered its batch"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    test();
    release_tx.send(()).unwrap();
    owner.join().unwrap().unwrap();
    assert_eq!(crate::wbq_flush_status(), crate::WbqFlushOutcome::Drained);
    crate::wbq_shutdown();
    assert_eq!(crate::archive_lag_snapshot().oldest_unmaterialized_us, 0);
}

#[test]
fn archive_lag_grace_leaves_queued_mail_to_drain_then_reconciles_orphans() {
    fixture(|cx, pool, config| {
        let created = mcp_agent_mail_db::now_micros() - 60 * 1_000_000;
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(&format!("UPDATE messages SET created_ts = {created}"))
            .unwrap();
        drop(conn);
        let before = database_evidence(cx, pool);
        let mut cursor = ReconcileCursor::default();
        let stop = AtomicBool::new(false);
        with_queued_archive_lag(config, Duration::from_secs(120), || {
            let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(
                (
                    report.scanned,
                    report.repaired,
                    report.deferred,
                    report.retry_len
                ),
                (0, 0, 0, 0),
                "still-queued mail must not contend with its drain: {report:?}"
            );
            assert!(!report.budget_exhausted);
            assert_eq!(database_evidence(cx, pool), before);
        });
        // No failure ticket was needed: once the actual queue drains, ordinary
        // scanning must repair same-process orphans on the next bounded pass.
        let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
        assert_eq!((report.repaired, report.deferred), (2, 0));
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn archive_lag_grace_does_not_hide_pre_restart_mail() {
    fixture(|cx, pool, config| {
        let now = mcp_agent_mail_db::now_micros();
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(&format!(
            "UPDATE messages SET created_ts = {}",
            now - 60 * 1_000_000
        ))
        .unwrap();
        drop(conn);
        let before = database_evidence(cx, pool);
        let mut cursor = ReconcileCursor::settled_before(now - 30 * 1_000_000);
        let stop = AtomicBool::new(false);
        with_queued_archive_lag(config, Duration::from_secs(120), || {
            let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(
                (report.scanned, report.deferred_transient, report.retry_len),
                (2, 2, 2),
                "old-process mail must reach repair despite new-process queue lag"
            );
        });
        let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
        assert_eq!((report.repaired, report.retry_len), (2, 0));
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn archive_lag_grace_does_not_hide_known_failed_writes() {
    fixture(|cx, pool, config| {
        let created = mcp_agent_mail_db::now_micros() - 60 * 1_000_000;
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(&format!("UPDATE messages SET created_ts = {created}"))
            .unwrap();
        drop(conn);
        let before = database_evidence(cx, pool);
        let prepared = prepare_message(cx, pool, 901).unwrap();
        let failed = crate::WriteOp::MessageBundle {
            config: config.clone(),
            project_slug: prepared.project_slug,
            message_json: prepared.message,
            body_md: prepared.body,
            sender: prepared.sender,
            recipients: prepared.recipients,
            extra_paths: Vec::new(),
        };
        durability::record_failures(std::iter::once(&failed));
        assert!(crate::durability_degraded());
        let stop = AtomicBool::new(false);
        with_queued_archive_lag(config, Duration::from_secs(120), || {
            let mut cursor = ReconcileCursor::default();
            let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(
                (report.scanned, report.deferred_transient, report.retry_len),
                (1, 1, 1),
                "the failed-write lane must bypass normal-scan grace"
            );
            assert_eq!(cursor.retries.front().unwrap().id, 901);
            assert!(crate::durability_degraded());
        });
        // Release the refusal only through real archive publication and proof,
        // never by resetting the ledger or ignoring its retry backoff.
        let report =
            reconcile_message_batch(cx, pool, config, &mut ReconcileCursor::default(), &stop)
                .unwrap();
        assert_eq!(report.repaired, 2);
        assert!(!crate::durability_degraded());
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn archive_lag_grace_is_capped_for_stalled_or_unrelated_work() {
    fixture(|cx, pool, config| {
        let now = mcp_agent_mail_db::now_micros();
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(&format!(
            "UPDATE messages SET created_ts = {} WHERE id = 901; \
             UPDATE messages SET created_ts = {} WHERE id = 902",
            now - 700 * 1_000_000,
            now - 60 * 1_000_000,
        ))
        .unwrap();
        drop(conn);
        let before = database_evidence(cx, pool);
        let stop = AtomicBool::new(false);
        with_queued_archive_lag(config, Duration::from_secs(3_600), || {
            let mut cursor = ReconcileCursor::default();
            let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert_eq!(
                (report.scanned, report.deferred_transient, report.retry_len),
                (1, 1, 1),
                "an hour of lag may shield recent mail, but not an old orphan"
            );
            assert_eq!(cursor.retries.front().unwrap().id, 901);
        });
        let report =
            reconcile_message_batch(cx, pool, config, &mut ReconcileCursor::default(), &stop)
                .unwrap();
        assert_eq!(report.repaired, 2);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn archive_lag_grace_saturates_extreme_measurements_without_losing_startup_anchor() {
    assert_eq!(normal_archive_cutoff(1_000_000_000, 0, 0), 970_000_000);
    assert_eq!(
        normal_archive_cutoff(1_000_000_000, 0, u64::MAX),
        370_000_000
    );
    assert_eq!(
        normal_archive_cutoff(1_000_000_000, 990_000_000, u64::MAX),
        990_000_000
    );
    assert_eq!(normal_archive_cutoff(i64::MIN, 0, u64::MAX), 0);
}

fn seed_outbox(config: &Config, prepared: &PreparedMessage) -> (std::path::PathBuf, Vec<u8>) {
    let archive = crate::ensure_archive(config, &prepared.project_slug).unwrap();
    let paths = crate::message_paths_for_bundle(
        &archive,
        &prepared.message,
        &prepared.sender,
        &prepared.recipients,
    )
    .unwrap()
    .0;
    let mut message = prepared.message.clone();
    message["operator_note"] = json!("surviving evidence");
    let bytes = crate::render_message_bundle_content(&message, &prepared.body)
        .unwrap()
        .into_bytes();
    crate::ensure_parent_dir(&paths.outbox).unwrap();
    fs::write(&paths.outbox, &bytes).unwrap();
    (paths.outbox, bytes)
}

fn assert_recovered(cx: &Cx, pool: &DbPool, config: &Config, id: i64, note: bool) {
    let prepared = prepare_message(cx, pool, id).unwrap();
    let archive = crate::open_archive(config, &prepared.project_slug)
        .unwrap()
        .unwrap();
    let paths = crate::message_paths_for_bundle(
        &archive,
        &prepared.message,
        &prepared.sender,
        &prepared.recipients,
    )
    .unwrap()
    .0;
    let mut full = prepared.message.clone();
    if note {
        full["operator_note"] = json!("surviving evidence");
    }
    let inbox = crate::redact_message_bcc_for_inbox(&full);
    let repo = git2::Repository::open(&archive.repo_root).unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    for (path, expected) in [&paths.canonical, &paths.outbox]
        .into_iter()
        .map(|path| (path, &full))
        .chain(paths.inbox.iter().map(|path| (path, &inbox)))
    {
        let (message, body) = read_surviving_message(path).unwrap().unwrap();
        assert_eq!(&message, expected);
        assert_eq!(body, prepared.body);
        assert_eq!(message["reply_to"], 700);
        let relative = crate::rel_path_cached(&archive.canonical_repo_root, path).unwrap();
        let entry = tree.get_path(Path::new(&relative)).unwrap();
        assert_eq!(entry.kind(), Some(git2::ObjectType::Blob));
        assert_eq!(
            repo.find_blob(entry.id()).unwrap().content(),
            fs::read(path).unwrap().as_slice()
        );
    }
    assert_eq!(full["bcc"], json!(["RedFox"]));
    assert_eq!(inbox["bcc"], json!([]));
}

fn catch_up(cx: &Cx, pool: &DbPool, config: &Config, cursor: &mut ReconcileCursor) -> usize {
    // Backfill deliberately has an empty boundary pass before wrapping. Do
    // not reset the cursor to make the test conceal a failed finite revisit.
    let mut repaired = 0;
    for _ in 0..3 {
        let report =
            reconcile_message_batch(cx, pool, config, cursor, &AtomicBool::new(false)).unwrap();
        assert_eq!(report.deferred, 0, "{report:?}");
        assert!(!report.interrupted);
        repaired += report.repaired;
    }
    repaired
}

fn exercise_busy_fence(existing: bool) {
    fixture(|cx, pool, config| {
        let original = prepare_message(cx, pool, 901).unwrap();
        let survivor = existing.then(|| seed_outbox(config, &original));
        crate::flush_async_commits();
        let before_db = database_evidence(cx, pool);
        let head = || {
            git2::Repository::open(&config.storage_root)
                .ok()
                .and_then(|repo| repo.head().ok().and_then(|head| head.target()))
        };
        let before_head = head();
        let token_path = config
            .storage_root
            .join(".git")
            .join(crate::ARCHIVE_EPOCH_FILE_NAME);
        let before_token = fs::read(&token_path).ok();
        let before_epoch = crate::archive_mutation_epoch();
        let before_active = crate::archive_mutations_active();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            crate::with_archive_snapshot_publication_fence(|| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(20))
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut cursor = ReconcileCursor::default();
        let result =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false));
        let observed_db = database_evidence(cx, pool);
        let observed_head = head();
        let observed_token = fs::read(&token_path).ok();
        let observed_epoch = crate::archive_mutation_epoch();
        let observed_active = crate::archive_mutations_active();
        let held_writers = active_writer_count();
        let promotion_available = try_acquire_promotion_barrier_if_idle().is_some();
        let owner_still_held = crate::archive_publication_fence_holder().is_some();
        let initialized = config.storage_root.join(".git").exists();
        let other_created = config.storage_root.join("projects/other").exists();
        let survivor_during = survivor.as_ref().map(|(path, _)| fs::read(path).unwrap());
        let _ = release_tx.send(());
        assert!(
            owner.join().unwrap().is_ok(),
            "replay waited for the fence owner's timeout"
        );
        let report = result.unwrap();
        assert_eq!(
            (
                report.scanned,
                report.deferred,
                report.repaired,
                report.files_created
            ),
            (2, 2, 0, 0)
        );
        assert_eq!(observed_db, before_db);
        assert_eq!(observed_head, before_head);
        assert_eq!(observed_token, before_token);
        assert_eq!(observed_epoch, before_epoch);
        assert_eq!(observed_active, before_active);
        assert_eq!(held_writers, 0);
        assert!(promotion_available && owner_still_held);
        assert_eq!(initialized, existing);
        assert!(
            !other_created,
            "a busy fence cannot initialize an unrelated project"
        );
        assert_eq!(
            survivor_during,
            survivor.as_ref().map(|(_, bytes)| bytes.clone())
        );
        assert_eq!(cursor.tail_after, Some(902));
        assert_eq!(
            (
                report.deferred_transient,
                report.deferred_conflict,
                report.retry_len
            ),
            (2, 0, 2)
        );

        // Deferred work must be reread, not retained as a stale publication
        // payload. The second message has never had any archive copy.
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw("UPDATE messages SET body_md='Live body after admission reopens', topic='updated' WHERE id=902").unwrap();
        drop(conn);
        let before_resume = database_evidence(cx, pool);
        let resumed =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(
            (resumed.repaired, resumed.retry_len),
            (2, 0),
            "transient refusals must retry on the next pass, without a cursor wrap"
        );
        assert_recovered(cx, pool, config, 901, existing);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before_resume);
        assert_eq!(active_writer_count(), 0);
    });
}

#[test]
fn busy_global_fence_preserves_existing_mail_and_releases_database_admission() {
    exercise_busy_fence(true);
}

#[test]
fn busy_global_fence_defers_missing_archive_initialization_and_revisits_live_mail() {
    exercise_busy_fence(false);
}

fn while_publication_fence_is_held<T>(observe: impl FnOnce() -> T) -> T {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let owner = std::thread::spawn(move || {
        crate::with_archive_snapshot_publication_fence(|| {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(20))
        })
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = observe();
    let _ = release_tx.send(());
    assert!(owner.join().unwrap().is_ok(), "fence owner timed out");
    result
}

#[test]
fn complete_bundles_verify_without_acquiring_the_global_publication_fence() {
    fixture(|cx, pool, config| {
        let initial = reconcile_message_batch(
            cx,
            pool,
            config,
            &mut ReconcileCursor::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(initial.repaired, 2);
        crate::flush_async_commits();
        let before_db = database_evidence(cx, pool);
        let before = crate::archive_epoch_read_stamp(&config.storage_root)
            .unwrap()
            .unwrap();
        let (report, owner_held, writers) = while_publication_fence_is_held(|| {
            let report = reconcile_message_batch(
                cx,
                pool,
                config,
                &mut ReconcileCursor::default(),
                &AtomicBool::new(false),
            );
            (
                report,
                crate::archive_publication_fence_holder().is_some(),
                active_writer_count(),
            )
        });
        let report = report.unwrap();
        assert_eq!(
            (
                report.scanned,
                report.unchanged,
                report.repaired,
                report.deferred
            ),
            (2, 2, 0, 0),
            "{report:?}"
        );
        assert!(owner_held);
        assert_eq!(writers, 0);
        assert_eq!(database_evidence(cx, pool), before_db);
        assert_eq!(
            crate::archive_epoch_read_stamp(&config.storage_root).unwrap(),
            Some(before)
        );
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
    });
}

#[test]
fn complete_failed_bundle_requires_guarded_verification_before_clearing_durability_debt() {
    fixture(|cx, pool, config| {
        durability::clear_by_operator();
        let stop = AtomicBool::new(false);
        let initial =
            reconcile_message_batch(cx, pool, config, &mut ReconcileCursor::default(), &stop)
                .unwrap();
        assert_eq!(initial.repaired, 2);
        crate::flush_async_commits();
        let prepared = prepare_message(cx, pool, 901).unwrap();
        let failed = crate::WriteOp::MessageBundle {
            config: config.clone(),
            project_slug: prepared.project_slug.clone(),
            message_json: prepared.message.clone(),
            body_md: prepared.body.clone(),
            sender: prepared.sender.clone(),
            recipients: prepared.recipients.clone(),
            extra_paths: Vec::new(),
        };
        // The real failure ledger can outlive a completed archive write, for
        // example when acknowledgement failed after publication. Complete
        // files do not make its refusal an ordinary read-only observation.
        durability::record_failures(std::iter::once(&failed));
        assert!(crate::durability_degraded());
        assert!(durability::ticket_for(config, &prepared).is_some());
        let before_db = database_evidence(cx, pool);
        let mut cursor = ReconcileCursor::default();
        let (report, refused, retained) = while_publication_fence_is_held(|| {
            (
                reconcile_message_batch(cx, pool, config, &mut cursor, &stop),
                crate::durability_degraded(),
                durability::ticket_for(config, &prepared).is_some(),
            )
        });
        let report = report.unwrap();
        assert_eq!(
            (report.scanned, report.unchanged, report.deferred_transient),
            (2, 1, 1),
            "only the healthy companion may use read-only verification: {report:?}"
        );
        assert!(
            refused && retained,
            "an observation cannot clear durability debt"
        );
        assert_eq!(database_evidence(cx, pool), before_db);

        // Failure-ledger seeding plus the refused attempt owns a cooldown.
        // Advance only its scheduling hint, without waiting on wall time or
        // changing the retained ticket/source/archive proof obligations.
        for retry in &mut cursor.retries {
            retry.ready_at = Instant::now();
        }
        let resumed = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
        assert_eq!(resumed.deferred, 0, "{resumed:?}");
        assert!(resumed.unchanged > 0, "{resumed:?}");
        assert!(!crate::durability_degraded());
        assert!(durability::ticket_for(config, &prepared).is_none());
        assert_eq!(database_evidence(cx, pool), before_db);
        assert_recovered(cx, pool, config, 901, false);
    });
}

#[test]
fn missing_committed_copy_cannot_be_reported_as_an_unchanged_bundle() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        reconcile_prepared(config, &prepared).unwrap();
        crate::flush_async_commits();
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &prepared.message,
            &prepared.sender,
            &prepared.recipients,
        )
        .unwrap()
        .0;
        let retained = config.storage_root.join("retained-inbox.md");
        fs::rename(&paths.inbox[0], &retained).unwrap();
        let bytes = fs::read(&retained).unwrap();
        let stop = AtomicBool::new(false);
        let error = while_publication_fence_is_held(|| {
            reconcile_prepared_inner(
                config,
                &prepared,
                Some(RepairControl { cx, stop: &stop }),
                false,
            )
        })
        .unwrap_err();
        assert!(error.transient, "{error:?}");
        assert!(!paths.inbox[0].exists());
        assert_eq!(fs::read(&retained).unwrap(), bytes);
        assert_eq!(
            reconcile_prepared(config, &prepared).unwrap().files_created,
            1
        );
        assert_recovered(cx, pool, config, 901, false);
    });
}

#[test]
fn uncommitted_complete_files_still_require_repair_publication_admission() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        let archive = crate::ensure_archive(config, "project").unwrap();
        crate::flush_async_commits();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &prepared.message,
            &prepared.sender,
            &prepared.recipients,
        )
        .unwrap()
        .0;
        let full = crate::render_message_bundle_content(&prepared.message, &prepared.body).unwrap();
        let inbox = crate::render_message_bundle_content(
            &crate::redact_message_bcc_for_inbox(&prepared.message),
            &prepared.body,
        )
        .unwrap();
        for (path, bytes) in [&paths.canonical, &paths.outbox]
            .into_iter()
            .map(|path| (path, &full))
            .chain(paths.inbox.iter().map(|path| (path, &inbox)))
        {
            crate::ensure_parent_dir(path).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let stop = AtomicBool::new(false);
        let error = while_publication_fence_is_held(|| {
            reconcile_prepared_inner(
                config,
                &prepared,
                Some(RepairControl { cx, stop: &stop }),
                false,
            )
        })
        .unwrap_err();
        assert!(error.transient, "{error:?}");
        assert!(
            CommittedMessages::open(&archive)
                .unwrap()
                .file_identity(&archive, &paths.canonical)
                .is_err()
        );
        let repaired = reconcile_prepared(config, &prepared).unwrap();
        assert_eq!(repaired.files_created, 0);
        assert!(repaired.git_commit_needed);
        assert_recovered(cx, pool, config, 901, false);
    });
}

#[test]
fn read_only_verification_keeps_staged_conflicting_identity_visible() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        reconcile_prepared(config, &prepared).unwrap();
        crate::flush_async_commits();
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &prepared.message,
            &prepared.sender,
            &prepared.recipients,
        )
        .unwrap()
        .0;
        let original = fs::read(&paths.canonical).unwrap();
        let mut conflicting = prepared.message.clone();
        conflicting["reply_to"] = json!(701);
        fs::write(
            &paths.canonical,
            crate::render_message_bundle_content(&conflicting, &prepared.body).unwrap(),
        )
        .unwrap();
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let mut index = repo.index().unwrap();
        let relative =
            crate::rel_path_cached(&archive.canonical_repo_root, &paths.canonical).unwrap();
        index.add_path(Path::new(&relative)).unwrap();
        index.write().unwrap();
        fs::write(&paths.canonical, &original).unwrap();
        let before_index = fs::read(repo.path().join("index")).unwrap();
        let before_head = repo.head().unwrap().target();
        let error = verify_prepared_without_mutation(config, &prepared, None).unwrap_err();
        assert!(
            error.detail.contains("staged canonical payload conflicts"),
            "{error:?}"
        );
        assert!(!error.transient);
        let error = reconcile_prepared_inner(config, &prepared, None, false).unwrap_err();
        assert!(
            error.detail.contains("staged canonical payload conflicts"),
            "{error:?}"
        );
        assert!(!error.transient);
        assert_eq!(fs::read(repo.path().join("index")).unwrap(), before_index);
        assert_eq!(repo.head().unwrap().target(), before_head);
        assert_eq!(fs::read(&paths.canonical).unwrap(), original);
    });
}

#[test]
fn read_only_verification_retries_when_archive_epoch_changes_during_its_reads() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        reconcile_prepared(config, &prepared).unwrap();
        crate::flush_async_commits();
        let root = config.storage_root.clone();
        let before = crate::archive_epoch_read_stamp(&root).unwrap();
        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let observed = fired.clone();
        super::super::SCAN_HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |stage| {
                if stage == "git root" && !observed.swap(true, Ordering::AcqRel) {
                    drop(crate::ArchiveMutationGuard::begin_at(&root));
                }
            }));
        });
        let stop = AtomicBool::new(false);
        let result = verify_prepared_without_mutation(
            config,
            &prepared,
            Some(RepairControl { cx, stop: &stop }),
        );
        super::super::SCAN_HOOK.with(|slot| drop(slot.borrow_mut().take()));
        assert!(fired.load(Ordering::Acquire));
        let error = result.unwrap_err();
        assert!(error.transient, "{error:?}");
        assert_ne!(
            crate::archive_epoch_read_stamp(&config.storage_root).unwrap(),
            before
        );
        assert!(verify_prepared_without_mutation(config, &prepared, None).unwrap());
        assert_recovered(cx, pool, config, 901, false);
    });
}

#[test]
fn read_only_verification_retries_when_head_changes_even_with_identical_tree() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        reconcile_prepared(config, &prepared).unwrap();
        crate::flush_async_commits();
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        let before = CommittedMessages::open(&archive).unwrap();
        let root = config.storage_root.clone();
        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let observed = fired.clone();
        super::super::SCAN_HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |stage| {
                if stage == "git root" && !observed.swap(true, Ordering::AcqRel) {
                    // An external Git writer need not use our process-local
                    // mutation counter. Change actual HEAD without changing
                    // any tree object or on-disk message bytes.
                    let repo = git2::Repository::open(&root).unwrap();
                    let parent = repo.head().unwrap().peel_to_commit().unwrap();
                    let tree = parent.tree().unwrap();
                    let signature = git2::Signature::now("test", "test@example.invalid").unwrap();
                    repo.commit(
                        Some("HEAD"),
                        &signature,
                        &signature,
                        "advance HEAD during read-only verification",
                        &tree,
                        &[&parent],
                    )
                    .unwrap();
                }
            }));
        });
        let stop = AtomicBool::new(false);
        let result = verify_prepared_without_mutation(
            config,
            &prepared,
            Some(RepairControl { cx, stop: &stop }),
        );
        super::super::SCAN_HOOK.with(|slot| drop(slot.borrow_mut().take()));
        assert!(fired.load(Ordering::Acquire));
        let error = result.unwrap_err();
        assert!(error.transient, "{error:?}");
        let after = CommittedMessages::open(&archive).unwrap();
        assert_ne!(before.head_id, after.head_id);
        assert_eq!(before.tree_id, after.tree_id);
        assert!(verify_prepared_without_mutation(config, &prepared, None).unwrap());
    });
}

#[test]
fn optimistic_epoch_drift_frees_saturated_retry_capacity_for_a_crash_orphan() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        reconcile_prepared(config, &prepared).unwrap();
        crate::flush_async_commits();
        let before = database_evidence(cx, pool);
        let mut cursor = ReconcileCursor {
            source_identity: pool.sqlite_identity_key(),
            retries: std::iter::once(DeferredRetry {
                id: 901,
                attempts: 1,
                ready_at: Instant::now(),
            })
            .chain(
                (10_000..10_000 + i64::try_from(MAX_RETRY_IDS - 1).unwrap()).map(|id| {
                    DeferredRetry {
                        id,
                        attempts: 1,
                        ready_at: Instant::now() + MAX_RETRY_BACKOFF,
                    }
                }),
            )
            .collect(),
            ..Default::default()
        };
        let retained = cursor
            .retries
            .iter()
            .skip(1)
            .map(|retry| (retry.id, retry.attempts, retry.ready_at))
            .collect::<Vec<_>>();
        let root = config.storage_root.clone();
        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let observed = fired.clone();
        super::super::SCAN_HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |stage| {
                if stage == "git root" && !observed.swap(true, Ordering::AcqRel) {
                    drop(crate::ArchiveMutationGuard::begin_at(&root));
                }
            }));
        });
        let result =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false));
        super::super::SCAN_HOOK.with(|slot| drop(slot.borrow_mut().take()));
        let report = result.unwrap();
        assert!(fired.load(Ordering::Acquire));
        assert_eq!(
            (report.unchanged, report.repaired, report.deferred),
            (1, 1, 0)
        );
        assert_eq!(report.retry_len, MAX_RETRY_IDS - 1);
        assert!(report.scanned <= MAX_IDS_PER_PASS);
        assert!(report.repaired <= MAX_REPAIRS_PER_BATCH);
        assert!(report.payload_bytes <= MAX_BATCH_PAYLOAD_BYTES);
        assert_eq!(
            cursor
                .retries
                .iter()
                .map(|retry| (retry.id, retry.attempts, retry.ready_at))
                .collect::<Vec<_>>(),
            retained
        );
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn optimistic_head_drift_still_requires_publication_admission() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        reconcile_prepared(config, &prepared).unwrap();
        crate::flush_async_commits();
        let root = config.storage_root.clone();
        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let observed = fired.clone();
        super::super::SCAN_HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |stage| {
                if stage == "git root" && !observed.swap(true, Ordering::AcqRel) {
                    // The competing Git writer need not own our process-local
                    // fence. Change HEAD while preserving every message byte.
                    let repo = git2::Repository::open(&root).unwrap();
                    let parent = repo.head().unwrap().peel_to_commit().unwrap();
                    let tree = parent.tree().unwrap();
                    let signature = git2::Signature::now("test", "test@example.invalid").unwrap();
                    repo.commit(
                        Some("HEAD"),
                        &signature,
                        &signature,
                        "advance HEAD while publication is fenced",
                        &tree,
                        &[&parent],
                    )
                    .unwrap();
                }
            }));
        });
        let stop = AtomicBool::new(false);
        let result = while_publication_fence_is_held(|| {
            reconcile_prepared_inner(
                config,
                &prepared,
                Some(RepairControl { cx, stop: &stop }),
                false,
            )
        });
        super::super::SCAN_HOOK.with(|slot| drop(slot.borrow_mut().take()));
        assert!(fired.load(Ordering::Acquire));
        let error = result.unwrap_err();
        assert!(error.transient, "{error:?}");
        assert!(!error.optimistic_observation_changed, "{error:?}");
        assert!(error.detail.contains("publication fence busy"), "{error:?}");
        assert_recovered(cx, pool, config, 901, false);
    });
}

#[test]
fn busy_project_is_skipped_while_another_mailbox_project_repairs_then_is_revisited() {
    fixture(|cx, pool, config| {
        let original = prepare_message(cx, pool, 901).unwrap();
        let (survivor, bytes) = seed_outbox(config, &original);
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        crate::ensure_archive(config, "other").unwrap();
        crate::flush_async_commits();
        let before = database_evidence(cx, pool);
        let process = crate::archive_process_lock(&archive).unwrap();
        let owner = process.lock().unwrap();
        let mut cursor = ReconcileCursor::default();
        let report =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(
            (
                report.scanned,
                report.deferred,
                report.repaired,
                report.files_created
            ),
            (2, 1, 1, 4)
        );
        assert!(matches!(
            process.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        assert_eq!(fs::read(&survivor).unwrap(), bytes);
        assert!(!archive.root.join("messages").exists());
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
        assert_eq!(active_writer_count(), 0);
        drop(owner);
        let resumed =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!((resumed.repaired, resumed.retry_len), (1, 0));
        assert_recovered(cx, pool, config, 901, true);
        assert_eq!(database_evidence(cx, pool), before);
        assert_eq!(fs::read(&survivor).unwrap(), bytes);
    });
}

#[test]
fn large_retained_lock_refusals_cannot_starve_fresh_mail_for_another_project() {
    fixture(|cx, pool, config| {
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_sync(
            "UPDATE messages SET body_md = ? WHERE id = 901",
            &["x".repeat(7 * 1024 * 1024 / 2).into()],
        )
        .unwrap();
        for id in 903..=906 {
            conn.execute_raw(&format!(
                "INSERT INTO messages(id, project_id, sender_id, thread_id, topic, subject, body_md, importance, \
                 ack_required, created_ts, recipients_json, attachments, archive_metadata_json) \
                 SELECT {id}, project_id, sender_id, thread_id, topic, subject, body_md, importance, \
                 ack_required, created_ts, recipients_json, attachments, archive_metadata_json FROM messages WHERE id = 901"
            )).unwrap();
            conn.execute_raw(&format!(
                "INSERT INTO message_recipients(message_id, agent_id, kind, read_ts, ack_ts) \
                 SELECT {id}, agent_id, kind, read_ts, ack_ts FROM message_recipients WHERE message_id = 901"
            )).unwrap();
        }
        drop(conn);
        let archive = crate::ensure_archive(config, "project").unwrap();
        crate::ensure_archive(config, "other").unwrap();
        crate::flush_async_commits();
        let process = crate::archive_process_lock(&archive).unwrap();
        let owner = process.lock().unwrap();
        let mut cursor = ReconcileCursor::default();
        for _ in 0..4 {
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
            if cursor.retries.len() == 5 {
                break;
            }
        }
        assert_eq!(
            cursor.retries.len(),
            5,
            "real lock refusals retain all large rows"
        );
        assert!(!archive.root.join("messages").exists());
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(
            "INSERT INTO messages(id, project_id, sender_id, thread_id, topic, subject, body_md, importance, \
             ack_required, created_ts, recipients_json, attachments, archive_metadata_json) \
             SELECT 907, project_id, sender_id, thread_id, topic, subject, body_md, importance, \
             ack_required, created_ts, recipients_json, attachments, archive_metadata_json FROM messages WHERE id = 902",
        ).unwrap();
        conn.execute_raw(
            "INSERT INTO message_recipients(message_id, agent_id, kind, read_ts, ack_ts) \
             SELECT 907, agent_id, kind, read_ts, ack_ts FROM message_recipients WHERE message_id = 902",
        ).unwrap();
        drop(conn);
        let before = database_evidence(cx, pool);
        for retry in &mut cursor.retries {
            retry.ready_at = Instant::now();
        }
        // Even when retry owns the first turn, five large refusals cannot
        // exhaust the byte budget before fresh unrelated mail gets a turn.
        cursor.next_lane_is_retry = true;
        let report =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert!(report.deferred_transient > 0, "{report:?}");
        assert_eq!(report.retry_len, 5);
        assert!(report.repaired > 0, "fresh mail was starved: {report:?}");
        assert!(report.payload_bytes <= MAX_BATCH_PAYLOAD_BYTES);
        assert!(matches!(
            process.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        assert_recovered(cx, pool, config, 907, false);
        assert!(!archive.root.join("messages").exists());
        assert_eq!(database_evidence(cx, pool), before);
        drop(owner);
    });
}

#[test]
fn repeated_fence_refusal_backs_off_without_scan_lanes_bypassing_the_retry() {
    fixture(|cx, pool, config| {
        let before = database_evidence(cx, pool);
        let mut cursor = ReconcileCursor::default();
        crate::with_archive_snapshot_publication_fence(|| {
            for attempt in 0..2 {
                let report =
                    reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                        .unwrap();
                assert_eq!((report.deferred_transient, report.retry_len), (2, 2));
                assert!(cursor.retries.iter().all(|retry| retry.attempts == attempt));
            }
            let deadline = Instant::now() + MAX_RETRY_BACKOFF;
            // Hold the recorded cooldown fixed so a slow test host cannot
            // accidentally turn this into a wall-clock scheduling test.
            for retry in &mut cursor.retries {
                retry.ready_at = deadline;
            }
            for _ in 0..3 {
                let report =
                    reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                        .unwrap();
                assert_eq!(
                    (report.scanned, report.deferred, report.retry_len),
                    (0, 0, 2)
                );
            }
        });
        assert_eq!(database_evidence(cx, pool), before);
        assert!(!config.storage_root.join(".git").exists());
        for retry in &mut cursor.retries {
            retry.ready_at = Instant::now();
        }
        let resumed =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!((resumed.repaired, resumed.retry_len), (2, 0));
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn unavailable_source_query_preserves_an_existing_transient_retry_until_recovery() {
    fixture(|cx, pool, config| {
        let before = database_evidence(cx, pool);
        let mut cursor = ReconcileCursor::default();
        crate::with_archive_snapshot_publication_fence(|| {
            let report =
                reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                    .unwrap();
            assert_eq!(report.retry_len, 2);
        });
        // Keep every source row, but make the real source statement temporarily
        // unavailable. Selection still succeeds against the messages table.
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw("ALTER TABLE agents RENAME TO retained_agents")
            .unwrap();
        drop(conn);
        let unavailable =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false));
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw("ALTER TABLE retained_agents RENAME TO agents")
            .unwrap();
        drop(conn);
        let report = unavailable.unwrap();
        assert_eq!(
            (
                report.deferred_transient,
                report.deferred_conflict,
                report.retry_len
            ),
            (2, 0, 2),
            "an unavailable source is not conflict evidence and cannot forget a deferred ID"
        );
        assert_eq!(database_evidence(cx, pool), before);
        for retry in &mut cursor.retries {
            retry.ready_at = Instant::now();
        }
        let resumed =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!((resumed.repaired, resumed.retry_len), (2, 0));
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn conflicting_evidence_is_preserved_and_does_not_enter_the_transient_retry_lane() {
    fixture(|cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        let (path, _) = seed_outbox(config, &prepared);
        let conflict =
            crate::render_message_bundle_content(&prepared.message, "conflicting body").unwrap();
        fs::write(&path, &conflict).unwrap();
        crate::flush_async_commits();
        let before = database_evidence(cx, pool);
        let mut cursor = ReconcileCursor::default();
        let report =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(
            (
                report.repaired,
                report.deferred_transient,
                report.deferred_conflict,
                report.retry_len
            ),
            (1, 0, 1, 0)
        );
        for _ in 0..3 {
            let report =
                reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                    .unwrap();
            assert_eq!(
                report.deferred, 0,
                "wrapping history must respect conflict cooldown"
            );
        }
        assert_eq!(fs::read(&path).unwrap(), conflict.as_bytes());
        assert_eq!(database_evidence(cx, pool), before);
        for (_, until) in &mut cursor.conflict_backoff {
            *until = Instant::now();
        }
        let mut conflicts = 0;
        for _ in 0..3 {
            conflicts +=
                reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                    .unwrap()
                    .deferred_conflict;
        }
        assert_eq!(
            conflicts, 1,
            "a cooled conflict may be checked again without hot-looping"
        );
        assert_eq!(fs::read(path).unwrap(), conflict.as_bytes());
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn full_retry_lane_preserves_unconsumed_scan_positions_instead_of_evicting_ids() {
    fixture(|cx, pool, config| {
        let deadline = Instant::now() + MAX_RETRY_BACKOFF;
        let mut cursor = ReconcileCursor {
            source_identity: pool.sqlite_identity_key(),
            retries: (10_000..10_000 + i64::try_from(MAX_RETRY_IDS).unwrap())
                .map(|id| DeferredRetry {
                    id,
                    attempts: 1,
                    ready_at: deadline,
                })
                .collect(),
            ..Default::default()
        };
        let ids = cursor
            .retries
            .iter()
            .map(|retry| retry.id)
            .collect::<Vec<_>>();
        let report =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!((report.scanned, report.retry_len), (0, MAX_RETRY_IDS));
        assert_eq!(cursor.tail_after, None);
        assert_eq!(cursor.backfill_ceiling, None);
        assert_eq!(
            cursor
                .retries
                .iter()
                .map(|retry| retry.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert!(!config.storage_root.join(".git").exists());
        // A due real retry must drain even while the full queue and untouched
        // normal-first cursor also expose fresh IDs. Do not free capacity by
        // hand: that would conceal a permanently pinned saturated queue.
        cursor.retries.front_mut().unwrap().id = 901;
        for retry in &mut cursor.retries {
            retry.ready_at = Instant::now();
        }
        let resumed =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(resumed.repaired, 2);
        assert!(resumed.retry_len < MAX_RETRY_IDS);
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
    });
}

fn retained_cursor() -> ReconcileCursor {
    ReconcileCursor {
        source_identity: "not-yet-rebound".to_string(),
        tail_after: Some(17),
        backfill_ceiling: Some(29),
        next_lane_is_history: true,
        settled_before_us: 1_234_567,
        ..Default::default()
    }
}

fn assert_cursor_retained(cursor: &ReconcileCursor) {
    assert_eq!(cursor.source_identity, "not-yet-rebound");
    assert_eq!(cursor.tail_after, Some(17));
    assert_eq!(cursor.backfill_ceiling, Some(29));
    assert!(cursor.next_lane_is_history);
    assert_eq!(cursor.settled_before_us, 1_234_567);
}

fn exercise_promotion(parent_writer: bool) {
    fixture(|cx, pool, config| {
        let before = database_evidence(cx, pool);
        let parent = parent_writer.then(begin_write_activity);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let promotion = std::thread::spawn(move || {
            let owner = if parent_writer {
                let (owner, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
                assert!(matches!(
                    outcome,
                    DrainOutcome::TimedOut {
                        remaining_writers: 1
                    }
                ));
                owner
            } else {
                try_acquire_promotion_barrier_if_idle().expect("idle promotion owner")
            };
            ready_tx.send(()).unwrap();
            let explicitly_released = release_rx.recv_timeout(Duration::from_secs(20));
            drop(owner);
            explicitly_released
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut cursor = retained_cursor();
        let result =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false));
        let writers = active_writer_count();
        let no_archive = !config.storage_root.join(".git").exists()
            && !config.storage_root.join("projects").exists();
        drop(parent);
        let _ = release_tx.send(());
        assert!(
            promotion.join().unwrap().is_ok(),
            "batch waited for promotion-owner expiry"
        );
        assert!(
            result
                .unwrap_err()
                .contains("recovery promotion or admission contention")
        );
        assert_cursor_retained(&cursor);
        assert_eq!(writers, usize::from(parent_writer));
        assert_eq!(active_writer_count(), 0);
        assert!(no_archive);
        assert_eq!(database_evidence(cx, pool), before);
        assert_eq!(catch_up(cx, pool, config, &mut cursor), 2);
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

#[test]
fn active_promotion_defers_before_source_selection_without_consuming_cursors() {
    exercise_promotion(false);
}

#[test]
fn failed_promotion_drain_cannot_trap_nested_message_recovery() {
    exercise_promotion(true);
}

#[test]
fn worker_stop_preserves_source_and_cursors_until_a_later_uncancelled_pass() {
    fixture(|cx, pool, config| {
        let before = database_evidence(cx, pool);
        let mut cursor = retained_cursor();
        let stop = AtomicBool::new(true);
        let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
        assert!(report.interrupted);
        assert_eq!(
            (report.scanned, report.repaired, report.files_created),
            (0, 0, 0)
        );
        assert_cursor_retained(&cursor);
        assert!(!config.storage_root.join(".git").exists());
        assert_eq!(active_writer_count(), 0);
        assert_eq!(database_evidence(cx, pool), before);
        stop.store(false, Ordering::Release);
        let report = reconcile_message_batch(cx, pool, config, &mut cursor, &stop).unwrap();
        assert_eq!((report.repaired, report.deferred), (2, 0));
        assert_recovered(cx, pool, config, 901, false);
        assert_recovered(cx, pool, config, 902, false);
        assert_eq!(database_evidence(cx, pool), before);
    });
}

fn seed_retention_mail(cx: &Cx, pool: &DbPool, config: &Config) {
    let conn = outcome(block_on(pool.acquire(cx))).unwrap();
    conn.execute_raw("UPDATE message_recipients SET read_ts=2000000, ack_ts=2500000")
        .unwrap();
    drop(conn);
    for id in [901, 902] {
        let prepared = prepare_message(cx, pool, id).unwrap();
        reconcile_prepared(config, &prepared).unwrap();
    }
    crate::flush_async_commits();
}

#[test]
fn retention_admission_refuses_promotion_without_consuming_source_or_cursor() {
    for nested in [false, true] {
        fixture(|cx, pool, config| {
            seed_retention_mail(cx, pool, config);
            let before = database_evidence(cx, pool);
            let parent = nested.then(begin_write_activity);
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let owner = std::thread::spawn(move || {
                let gate = if nested {
                    let (gate, result) = acquire_promotion_barrier_draining(Duration::ZERO);
                    assert!(matches!(
                        result,
                        DrainOutcome::TimedOut {
                            remaining_writers: 1
                        }
                    ));
                    gate
                } else {
                    try_acquire_promotion_barrier_if_idle().unwrap()
                };
                ready_tx.send(()).unwrap();
                let released = release_rx.recv_timeout(Duration::from_secs(3));
                drop(gate);
                released
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut cursor = ArchivePruneCursor {
                source_identity: "unconsumed source".into(),
                after: 17,
                ceiling: Some(1000),
            };
            let result = prune_archived_message_batch(
                cx,
                pool,
                config,
                3_000_000,
                &mut cursor,
                &AtomicBool::new(false),
            );
            let writers = active_writer_count();
            let _ = release_tx.send(());
            let released = owner.join().unwrap();
            drop(parent);
            assert!(
                released.is_ok(),
                "retention waited for the promotion owner to expire"
            );
            assert!(result.unwrap_err().contains("admission contention"));
            assert_eq!(writers, usize::from(nested));
            assert_eq!(
                (
                    cursor.source_identity.as_str(),
                    cursor.after,
                    cursor.ceiling
                ),
                ("unconsumed source", 17, Some(1000))
            );
            assert_eq!(database_evidence(cx, pool), before);
            let resumed = prune_archived_message_batch(
                cx,
                pool,
                config,
                3_000_000,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                (
                    resumed.pruned.deleted_messages,
                    resumed.pruned.deleted_recipients
                ),
                (2, 4)
            );
            assert_eq!(active_writer_count(), 0);
        });
    }
}

#[test]
fn retention_admission_defers_busy_fence_without_deleting_verified_mail() {
    fixture(|cx, pool, config| {
        seed_retention_mail(cx, pool, config);
        let before = database_evidence(cx, pool);
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let head = repo.head().unwrap().target();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            crate::with_archive_snapshot_publication_fence(|| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3))
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut cursor = ArchivePruneCursor::default();
        let result = prune_archived_message_batch(
            cx,
            pool,
            config,
            3_000_000,
            &mut cursor,
            &AtomicBool::new(false),
        );
        let writers = active_writer_count();
        let _ = release_tx.send(());
        let released = owner.join().unwrap();
        assert!(
            released.is_ok(),
            "retention waited for the unrelated archive publisher to expire"
        );
        let report = result.unwrap();
        assert_eq!(
            (
                report.scanned,
                report.deferred,
                report.pruned.deleted_messages
            ),
            (2, 2, 0)
        );
        assert!(report.pruned.more);
        assert_eq!(writers, 0);
        assert_eq!(database_evidence(cx, pool), before);
        assert_eq!(repo.head().unwrap().target(), head);
        let resumed = prune_archived_message_batch(
            cx,
            pool,
            config,
            3_000_000,
            &mut cursor,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(resumed.pruned.deleted_messages, 2);
    });
}

#[test]
fn retention_admission_skips_busy_project_and_prunes_other_verified_mail() {
    fixture(|cx, pool, config| {
        seed_retention_mail(cx, pool, config);
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        let process = crate::archive_process_lock(&archive).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let held = process.lock().unwrap();
            ready_tx.send(()).unwrap();
            let released = release_rx.recv_timeout(Duration::from_secs(3));
            drop(held);
            released
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut cursor = ArchivePruneCursor::default();
        let result = prune_archived_message_batch(
            cx,
            pool,
            config,
            3_000_000,
            &mut cursor,
            &AtomicBool::new(false),
        );
        let _ = release_tx.send(());
        let released = owner.join().unwrap();
        assert!(
            released.is_ok(),
            "one project stranded the entire retention sweep"
        );
        let report = result.unwrap();
        assert_eq!(
            (
                report.scanned,
                report.deferred,
                report.pruned.deleted_messages
            ),
            (2, 1, 1)
        );
        assert!(report.pruned.more);
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        let ids = conn
            .query_sync("SELECT id FROM messages ORDER BY id", &[])
            .unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].get_named::<i64>("id").unwrap(), 901);
        drop(conn);
        let resumed = prune_archived_message_batch(
            cx,
            pool,
            config,
            3_000_000,
            &mut cursor,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((resumed.pruned.deleted_messages, resumed.deferred), (1, 0));
        assert_eq!(active_writer_count(), 0);
    });
}

#[test]
fn retention_admission_preserves_a_live_filesystem_lock_and_revalidates_deferred_evidence() {
    use fs2::FileExt;
    fixture(|cx, pool, config| {
        seed_retention_mail(cx, pool, config);
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        let root = crate::archive_project_root_checked(&archive).unwrap();
        let lock_path = root.join(".archive.lock");
        let metadata_path = root.join(".archive.lock.owner.json");
        let held = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        held.lock_exclusive().unwrap();
        let metadata = serde_json::to_vec(&json!({"pid":std::process::id(),"created_ts":mcp_agent_mail_db::now_micros() as f64 / 1_000_000.0})).unwrap();
        fs::write(&metadata_path, &metadata).unwrap();
        let mut cursor = ArchivePruneCursor::default();
        let report = prune_archived_message_batch(
            cx,
            pool,
            config,
            3_000_000,
            &mut cursor,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            (
                report.scanned,
                report.deferred,
                report.pruned.deleted_messages
            ),
            (2, 1, 1)
        );
        assert_eq!(fs::read(&metadata_path).unwrap(), metadata);
        let contender = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
        assert!(
            contender.try_lock_exclusive().is_err(),
            "retention must not replace a held lock inode"
        );
        fs2::FileExt::unlock(&held).unwrap();
        drop(held);
        // A successful proof before contention is not retained authority. The
        // next attempt must compare the changed source against committed bytes.
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw("UPDATE messages SET body_md='changed while deferred' WHERE id=901")
            .unwrap();
        drop(conn);
        let changed = prune_archived_message_batch(
            cx,
            pool,
            config,
            3_000_000,
            &mut cursor,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((changed.pruned.deleted_messages, changed.deferred), (0, 1));
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        let rows = conn
            .query_sync("SELECT body_md FROM messages WHERE id=901", &[])
            .unwrap();
        assert_eq!(
            rows[0].get_named::<String>("body_md").unwrap(),
            "changed while deferred"
        );
        conn.execute_sync(
            "UPDATE messages SET body_md=? WHERE id=901",
            &[mcp_agent_mail_db::sqlmodel_core::Value::Text(
                "Keep λ and every byte.".into(),
            )],
        )
        .unwrap();
        drop(conn);
        let resumed = prune_archived_message_batch(
            cx,
            pool,
            config,
            3_000_000,
            &mut cursor,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((resumed.pruned.deleted_messages, resumed.deferred), (1, 0));
        assert_eq!(active_writer_count(), 0);
        assert_eq!(crate::archive_mutations_active(), 0);
    });
}

#[test]
fn retention_admission_shutdown_during_project_wait_preserves_unconsumed_work() {
    fixture(|cx, pool, config| {
        seed_retention_mail(cx, pool, config);
        let before = database_evidence(cx, pool);
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        let process = crate::archive_process_lock(&archive).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let cancel = std::sync::Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let _held = process.lock().unwrap();
            ready_tx.send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while crate::archive_mutations_active() == 0 && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            let observed = crate::archive_mutations_active() > 0;
            cancel.store(true, Ordering::Release);
            (observed, release_rx.recv_timeout(Duration::from_secs(5)))
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut cursor = ArchivePruneCursor::default();
        let result = prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop);
        let _ = release_tx.send(());
        let (observed, released) = owner.join().unwrap();
        assert!(
            observed && released.is_ok(),
            "shutdown must finish while the project owner still holds its lock"
        );
        let report = result.unwrap();
        assert!(report.interrupted && report.pruned.more);
        assert_eq!(
            (
                report.scanned,
                report.deferred,
                report.pruned.deleted_messages,
                cursor.after
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(cursor.ceiling, Some(902));
        assert_eq!(database_evidence(cx, pool), before);
        assert_eq!(active_writer_count(), 0);
        assert_eq!(crate::archive_mutations_active(), 0);
        stop.store(false, Ordering::Release);
        let resumed =
            prune_archived_message_batch(cx, pool, config, 3_000_000, &mut cursor, &stop).unwrap();
        assert_eq!(
            (
                resumed.pruned.deleted_messages,
                resumed.pruned.deleted_recipients
            ),
            (2, 4)
        );
    });
}
