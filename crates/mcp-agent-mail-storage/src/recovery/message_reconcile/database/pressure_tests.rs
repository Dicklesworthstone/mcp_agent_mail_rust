//! Storage-pressure retry retention through the production message recorder.
//!
//! The pressure cases use real kernel errors at the recording boundary, then
//! the real mailbox/Git repair path. Descriptor limits apply only to a private
//! child process; the ENOSPC case is not a full-filesystem fault test.

use super::*;
use std::io::{Error, ErrorKind};

fn io_failure(kind: ErrorKind) -> ReconcileFailure {
    crate::StorageError::Io(Error::new(kind, "opaque storage failure")).into()
}

#[test]
fn typed_pressure_keeps_retry_debt_without_trusting_error_text() {
    #[cfg(any(unix, windows))]
    for error in crate::recovery::descriptor_exhaustion_errors() {
        assert!(ReconcileFailure::from(crate::StorageError::Io(error)).transient);
    }
    for kind in [
        ErrorKind::WouldBlock,
        ErrorKind::TimedOut,
        ErrorKind::ResourceBusy,
        ErrorKind::StorageFull,
        ErrorKind::QuotaExceeded,
        ErrorKind::OutOfMemory,
    ] {
        assert!(io_failure(kind).transient, "{kind:?}");
    }
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::InvalidData,
        ErrorKind::NotFound,
        ErrorKind::AlreadyExists,
        ErrorKind::Interrupted,
        ErrorKind::Other,
    ] {
        let failure = ReconcileFailure::from(crate::StorageError::Io(Error::new(
            kind,
            "disk full; resource busy; quota exceeded",
        )));
        assert!(!failure.transient, "error text must not promote {kind:?}");
    }
    for error in [
        crate::StorageError::InvalidPath("disk full; foreign identity".into()),
        crate::StorageError::Git(git2::Error::from_str("disk full")),
    ] {
        assert!(!ReconcileFailure::from(error).transient);
    }
    assert!(ReconcileFailure::from(crate::StorageError::LockTimeout("held".into())).transient);
}

#[test]
fn pressure_recording_rotates_ids_and_preserves_bounded_backoff() {
    let now = Instant::now();
    let mut cursor = ReconcileCursor {
        tail_after: Some(940),
        backfill_ceiling: Some(924),
        ..Default::default()
    };
    let mut report = ReconcileReport {
        scanned: 7,
        payload_bytes: 123,
        ..Default::default()
    };
    for (index, kind) in [
        ErrorKind::StorageFull,
        ErrorKind::QuotaExceeded,
        ErrorKind::OutOfMemory,
        ErrorKind::ResourceBusy,
    ]
    .into_iter()
    .enumerate()
    {
        record_failure(
            901 + i64::try_from(index).unwrap(),
            io_failure(kind),
            &mut cursor,
            &mut report,
            now,
        );
    }
    assert_eq!(
        (
            report.deferred,
            report.deferred_transient,
            report.deferred_conflict
        ),
        (4, 4, 0)
    );
    assert_eq!(
        cursor
            .retries
            .iter()
            .map(|retry| retry.id)
            .collect::<Vec<_>>(),
        [901, 902, 903, 904]
    );
    assert!(cursor.retries.iter().all(|retry| retry.ready_at == now));
    for _ in 0..100 {
        record_failure(
            901,
            io_failure(ErrorKind::StorageFull),
            &mut cursor,
            &mut report,
            now,
        );
    }
    assert_eq!(cursor.retries.len(), 4);
    assert_eq!(cursor.retries.back().unwrap().id, 901);
    assert_eq!(
        cursor.retries.back().unwrap().ready_at,
        now + MAX_RETRY_BACKOFF
    );
    assert_eq!(
        cursor
            .retries
            .iter()
            .filter(|retry| retry.ready_at <= now)
            .map(|retry| retry.id)
            .collect::<Vec<_>>(),
        [902, 903, 904]
    );
    assert!(cursor.conflict_backoff.is_empty());
    assert_eq!(
        (cursor.tail_after, cursor.backfill_ceiling),
        (Some(940), Some(924))
    );
    assert_eq!(
        (
            report.scanned,
            report.payload_bytes,
            report.repaired,
            report.files_created
        ),
        (7, 123, 0, 0)
    );

    record_failure(
        902,
        "resource busy; conflicting reply parent".to_string().into(),
        &mut cursor,
        &mut report,
        now,
    );
    assert_eq!(report.deferred_conflict, 1);
    assert_eq!(
        cursor
            .retries
            .iter()
            .map(|retry| retry.id)
            .collect::<Vec<_>>(),
        [903, 904, 901]
    );
    assert_eq!(cursor.conflict_backoff.front().unwrap().0, 902);
}

fn evidence(cx: &Cx, pool: &DbPool) -> Value {
    let conn = outcome(block_on(pool.acquire(cx))).unwrap();
    let messages = conn
        .query_sync(
            "SELECT id, body_md, recipients_json, archive_metadata_json FROM messages ORDER BY id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| {
            json!([
                row.get_named::<i64>("id").unwrap(),
                row.get_named::<String>("body_md").unwrap(),
                row.get_named::<String>("recipients_json").unwrap(),
                row.get_named::<Option<String>>("archive_metadata_json")
                    .unwrap(),
            ])
        })
        .collect::<Vec<_>>();
    let receipts = conn.query_sync(
        "SELECT message_id, agent_id, kind, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id",
        &[],
    ).unwrap().iter().map(|row| json!([
        row.get_named::<i64>("message_id").unwrap(),
        row.get_named::<i64>("agent_id").unwrap(),
        row.get_named::<String>("kind").unwrap(),
        row.get_named::<Option<i64>>("read_ts").unwrap(),
        row.get_named::<Option<i64>>("ack_ts").unwrap(),
    ])).collect::<Vec<_>>();
    json!({"messages": messages, "receipts": receipts})
}

#[test]
fn full_cooling_retry_queue_does_not_consume_an_unretained_message() {
    tests::retention_fixture(1, |cx, pool, config| {
        let now = Instant::now();
        let mut cursor = ReconcileCursor {
            source_identity: pool.sqlite_identity_key(),
            tail_after: Some(900),
            ..Default::default()
        };
        for id in 10_000..10_000 + i64::try_from(MAX_RETRY_IDS).unwrap() {
            record_failure(
                id,
                io_failure(ErrorKind::QuotaExceeded),
                &mut cursor,
                &mut ReconcileReport::default(),
                now,
            );
            cursor.retries.back_mut().unwrap().ready_at = now + MAX_RETRY_BACKOFF;
        }
        let before = evidence(cx, pool);
        let report =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(
            (report.scanned, report.repaired, report.retry_len),
            (0, 0, MAX_RETRY_IDS)
        );
        assert_eq!(cursor.tail_after, Some(900));
        assert_eq!(cursor.retries.front().unwrap().id, 10_000);
        assert_eq!(evidence(cx, pool), before);
        assert!(!config.storage_root.join("projects").exists());

        cursor.forget_retry(10_000);
        let resumed =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(resumed.repaired, 1);
        assert_eq!(resumed.retry_len, MAX_RETRY_IDS - 1);
        assert_eq!(evidence(cx, pool), before);
    });
}

#[cfg(target_os = "linux")]
#[test]
fn real_enospc_outcome_repairs_current_mail_before_history_wrap() {
    use std::io::Write;

    assert_pressure_outcome_repairs_current_mail_before_history_wrap(|| {
        let error = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap()
            .write_all(b"message archive witness")
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ENOSPC));
        assert_eq!(error.kind(), ErrorKind::StorageFull);
        error.into()
    });
}

#[cfg(unix)]
#[test]
fn real_emfile_outcome_repairs_current_mail_before_history_wrap() {
    const CHILD: &str = "AM_TEST_MESSAGE_EMFILE_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest thread");
    if std::env::var(CHILD).as_deref() != Ok(name) {
        let output = std::process::Command::new("sh")
            .args(["-c", "ulimit -n 128 && exec \"$@\"", "archive-emfile-test"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .env("AM_TEST_MESSAGE_BATCH_ADMISSION_CHILD", name)
            .output()
            .expect("run descriptor-limited archive repair child");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "descriptor-limited {name} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    assert_pressure_outcome_repairs_current_mail_before_history_wrap(|| {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("message.md");
        std::fs::write(&path, "---json\n{}\n---\n\nretained witness").unwrap();
        let mut held = Vec::new();
        let exhaustion = loop {
            match std::fs::File::open("/dev/null") {
                Ok(file) => held.push(file),
                Err(error) => break error,
            }
        };
        // Obtain the failure from the production no-follow artifact reader,
        // not a fabricated error string. Release pressure before assertions
        // or the real database/Git repair needs descriptors of its own.
        let failed_read = read_surviving_message(&path);
        drop(held);
        assert_eq!(exhaustion.raw_os_error(), Some(nix::libc::EMFILE));
        let error = failed_read.expect_err("artifact open must hit the real descriptor limit");
        assert!(matches!(&error, crate::StorageError::Io(error)
            if error.raw_os_error() == Some(nix::libc::EMFILE)));
        assert!(read_surviving_message(&path).unwrap().is_some());
        error
    });
}

#[cfg(unix)]
fn assert_pressure_outcome_repairs_current_mail_before_history_wrap(
    fail: impl FnOnce() -> crate::StorageError,
) {
    tests::retention_fixture(40, |cx, pool, config| {
        for id in 902..941 {
            reconcile_prepared(config, &prepare_message(cx, pool, id).unwrap()).unwrap();
        }
        let past_orphan = || ReconcileCursor {
            source_identity: pool.sqlite_identity_key(),
            tail_after: Some(940),
            backfill_ceiling: Some(924),
            startup_backfill_complete: true,
            next_lane_is_retry: true,
            ..Default::default()
        };
        let mut control = past_orphan();
        let scanned =
            reconcile_message_batch(cx, pool, config, &mut control, &AtomicBool::new(false))
                .unwrap();
        assert_eq!((scanned.repaired, scanned.unchanged), (0, 16));

        let error = fail();
        let mut cursor = past_orphan();
        let mut failed = ReconcileReport::default();
        record_failure(901, error.into(), &mut cursor, &mut failed, Instant::now());
        assert_eq!(
            (failed.deferred_transient, failed.deferred_conflict),
            (1, 0)
        );
        assert_eq!(cursor.retries.front().unwrap().id, 901);
        let stopped =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(true)).unwrap();
        assert!(stopped.interrupted);
        assert_eq!(cursor.retries.len(), 1);
        assert_eq!(cursor.backfill_ceiling, Some(924));

        // The retry is an ID, not an old message payload. The live source is
        // read again after recovery; no receipt is changed by reconciliation.
        let conn = outcome(block_on(pool.acquire(cx))).unwrap();
        conn.execute_raw(
            "UPDATE messages SET body_md = 'Current source after disk recovery' WHERE id = 901",
        )
        .unwrap();
        drop(conn);
        let before = evidence(cx, pool);
        let report =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(
            (
                report.repaired,
                report.files_created,
                report.retry_len,
                report.deferred
            ),
            (1, 4, 0, 0)
        );
        assert!(cursor.backfill_ceiling.is_some_and(|ceiling| ceiling > 901));
        assert!(report.scanned <= MAX_IDS_PER_PASS);
        assert!(report.repaired <= MAX_REPAIRS_PER_BATCH);
        assert!(report.payload_bytes <= MAX_BATCH_PAYLOAD_BYTES);
        let prepared = prepare_message(cx, pool, 901).unwrap();
        let archive = crate::open_archive(config, "project").unwrap().unwrap();
        assert!(
            verify_retention_archive(&archive, &prepared, &mut 0)
                .unwrap()
                .is_some()
        );
        let paths = crate::message_paths_for_bundle(
            &archive,
            &prepared.message,
            &prepared.sender,
            &prepared.recipients,
        )
        .unwrap()
        .0;
        let (full, body) = read_surviving_message(&paths.canonical).unwrap().unwrap();
        assert_eq!(body, "Current source after disk recovery");
        assert_eq!(full["reply_to"], 700);
        assert_eq!(full["bcc"], json!(["RedFox"]));
        for path in paths.inbox {
            let (message, body) = read_surviving_message(&path).unwrap().unwrap();
            assert_eq!(message["bcc"], json!([]));
            assert_eq!(body, prepared.body);
        }
        assert_eq!(evidence(cx, pool), before);
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let head = repo.head().unwrap().target().unwrap();
        assert_eq!(
            reconcile_prepared(config, &prepared).unwrap(),
            ReconcileResult::default()
        );
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
    });
}

#[test]
fn resource_retry_does_not_overwrite_conflicting_surviving_mail() {
    tests::retention_fixture(1, |cx, pool, config| {
        let prepared = prepare_message(cx, pool, 901).unwrap();
        let archive = crate::ensure_archive(config, "project").unwrap();
        let paths = crate::message_paths_for_bundle(
            &archive,
            &prepared.message,
            &prepared.sender,
            &prepared.recipients,
        )
        .unwrap()
        .0;
        let bytes =
            crate::render_message_bundle_content(&prepared.message, "disk full; conflicting body")
                .unwrap();
        crate::ensure_parent_dir(&paths.outbox).unwrap();
        std::fs::write(&paths.outbox, &bytes).unwrap();
        let repo = git2::Repository::open(&archive.repo_root).unwrap();
        let head = repo.head().unwrap().target().unwrap();
        let before = evidence(cx, pool);
        let mut cursor = ReconcileCursor {
            source_identity: pool.sqlite_identity_key(),
            next_lane_is_retry: true,
            ..Default::default()
        };
        record_failure(
            901,
            io_failure(ErrorKind::StorageFull),
            &mut cursor,
            &mut ReconcileReport::default(),
            Instant::now(),
        );
        let report =
            reconcile_message_batch(cx, pool, config, &mut cursor, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(
            (
                report.repaired,
                report.deferred_conflict,
                report.deferred_transient,
                report.retry_len
            ),
            (0, 1, 0, 0)
        );
        assert_eq!(std::fs::read(&paths.outbox).unwrap(), bytes.as_bytes());
        assert!(!paths.canonical.exists());
        assert_eq!(repo.head().unwrap().target().unwrap(), head);
        assert_eq!(evidence(cx, pool), before);
    });
}
