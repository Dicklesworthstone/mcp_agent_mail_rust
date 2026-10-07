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

fn database_evidence(cx: &Cx, pool: &DbPool) -> Value {
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

        // Deferred work must be reread, not retained as a stale publication
        // payload. The second message has never had any archive copy.
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw("UPDATE messages SET body_md='Live body after admission reopens', topic='updated' WHERE id=902").unwrap();
        drop(conn);
        let before_resume = database_evidence(cx, pool);
        assert_eq!(catch_up(cx, pool, config, &mut cursor), 2);
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
        assert_eq!(catch_up(cx, pool, config, &mut cursor), 1);
        assert_recovered(cx, pool, config, 901, true);
        assert_eq!(database_evidence(cx, pool), before);
        assert_eq!(fs::read(&survivor).unwrap(), bytes);
    });
}

fn retained_cursor() -> ReconcileCursor {
    ReconcileCursor {
        source_identity: "not-yet-rebound".to_string(),
        tail_after: Some(17),
        backfill_ceiling: Some(29),
        next_lane_is_history: true,
        settled_before_us: 1_234_567,
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
