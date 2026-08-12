//! Integration tests for client-supplied idempotency keys on mutating DB entry
//! points (br-idempotency-keys-mutating-tools-h0x9k).
//!
//! Exercises the real DB layer (no mocks) against a temp FrankenSQLite file to
//! prove the acceptance criteria at the transactional core that the tool layer
//! calls into:
//!   (a) POSITIVE  — a retry with the same key + byte-identical payload replays
//!       the original result (same id/timestamps), applies nothing, and marks
//!       the outcome replayed; the row exists exactly once.
//!   (b) NEGATIVE  — the same key with a DIFFERENT payload fingerprint is a
//!       typed conflict carrying both fingerprints, and writes no second row.
//!   (c) RETENTION — a replay inside the window succeeds; once the record's
//!       `expires_ts` is in the past, the pruning check treats the key as fresh.
//! Plus per-(project, tool) key scoping so unrelated tools never collide.

#![allow(clippy::too_many_lines, clippy::cast_possible_wrap)]

mod common;

use asupersync::Outcome;
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, IdempotencyClaim, IdempotentOutcome, MessageRow};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(asupersync::Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    common::block_on(f)
}

/// Build a temp-file pool and return it together with the db file path (needed
/// to drive the retention test by ageing `expires_ts` directly).
fn make_pool() -> (DbPool, tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("idem_{}.db", unique_suffix()));

    let init_conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
        .expect("open connection for test pool");
    init_conn
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_BASE_SQL)
        .expect("apply init PRAGMAs");
    let cx = asupersync::Cx::for_testing();
    match common::spin_poll(mcp_agent_mail_db::schema::migrate_to_latest_base(
        &cx, &init_conn,
    )) {
        Outcome::Ok(_) => {}
        other => panic!("test pool migration failed: {other:?}"),
    }
    drop(init_conn);

    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        max_connections: 5,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: false,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir, db_path)
}

fn setup_project(pool: &DbPool) -> i64 {
    let pool = pool.clone();
    let key = format!("/tmp/idem_test_project_{}", unique_suffix());
    block_on(|cx| async move {
        match queries::ensure_project(&cx, &pool, &key).await {
            Outcome::Ok(p) => p.id.unwrap(),
            other => panic!("ensure_project failed: {other:?}"),
        }
    })
}

fn setup_agent(pool: &DbPool, project_id: i64, name: &str) -> i64 {
    let pool = pool.clone();
    let name = name.to_string();
    block_on(|cx| async move {
        match queries::register_agent(
            &cx,
            &pool,
            project_id,
            &name,
            "test",
            "test-model",
            Some("idempotency test"),
            None,
            None,
        )
        .await
        {
            Outcome::Ok(a) => a.id.unwrap(),
            other => panic!("register_agent({name}) failed: {other:?}"),
        }
    })
}

/// Count rows via an independent read connection so the assertion is grounded in
/// the durable table state, not the entry point's own return value.
fn count_rows(db_path: &Path, sql: &str) -> i64 {
    let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
        .expect("open count connection");
    let rows = conn.query_sync(sql, &[]).expect("count query");
    rows.first()
        .and_then(|r| r.get_as::<i64>(0).ok())
        .unwrap_or(-1)
}

/// Force every recorded idempotency key to be expired (retention test seam).
fn expire_all_idempotency_keys(db_path: &Path) {
    let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
        .expect("open expire connection");
    conn.execute_raw("UPDATE idempotency_keys SET expires_ts = 1")
        .expect("age idempotency keys");
}

/// Convenience: send an idempotent message with the given key + fingerprint.
fn send_idem(
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    recipient_id: i64,
    body: &str,
    key: &str,
    fingerprint: &str,
) -> IdempotentOutcome<MessageRow> {
    let pool = pool.clone();
    let body = body.to_string();
    let key = key.to_string();
    let fingerprint = fingerprint.to_string();
    block_on(|cx| async move {
        let recipients = [(recipient_id, "to")];
        let claim = IdempotencyClaim {
            project_id,
            tool: "send_message",
            key: &key,
            fingerprint: &fingerprint,
        };
        match queries::create_message_with_recipients_idempotent(
            &cx,
            &pool,
            project_id,
            sender_id,
            "subject",
            &body,
            None,
            "normal",
            true,
            "[]",
            &recipients,
            claim,
        )
        .await
        {
            Outcome::Ok(outcome) => outcome,
            other => panic!("create_message_with_recipients_idempotent failed: {other:?}"),
        }
    })
}

fn ack_idem(
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    message_id: i64,
    key: &str,
    fingerprint: &str,
) -> IdempotentOutcome<(i64, i64)> {
    let pool = pool.clone();
    let key = key.to_string();
    let fingerprint = fingerprint.to_string();
    block_on(|cx| async move {
        let claim = IdempotencyClaim {
            project_id,
            tool: "acknowledge_message",
            key: &key,
            fingerprint: &fingerprint,
        };
        match queries::acknowledge_message_idempotent(&cx, &pool, agent_id, message_id, claim).await
        {
            Outcome::Ok(outcome) => outcome,
            other => panic!("acknowledge_message_idempotent failed: {other:?}"),
        }
    })
}

fn reserve_idem(
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    path: &str,
    key: &str,
    fingerprint: &str,
) -> IdempotentOutcome<Vec<mcp_agent_mail_db::FileReservationRow>> {
    let pool = pool.clone();
    let path = path.to_string();
    let key = key.to_string();
    let fingerprint = fingerprint.to_string();
    block_on(|cx| async move {
        let paths = [path.as_str()];
        let claim = IdempotencyClaim {
            project_id,
            tool: "file_reservation_paths",
            key: &key,
            fingerprint: &fingerprint,
        };
        match queries::create_file_reservations_idempotent(
            &cx, &pool, project_id, agent_id, &paths, 3600, true, "reason", claim,
        )
        .await
        {
            Outcome::Ok(outcome) => outcome,
            other => panic!("create_file_reservations_idempotent failed: {other:?}"),
        }
    })
}

fn active_reservation_count(pool: &DbPool, project_id: i64) -> usize {
    let pool = pool.clone();
    block_on(|cx| async move {
        match queries::get_active_reservations(&cx, &pool, project_id).await {
            Outcome::Ok(rows) => rows.len(),
            other => panic!("get_active_reservations failed: {other:?}"),
        }
    })
}

#[test]
fn send_message_idempotent_replays_same_id_and_writes_once() {
    // Criterion (a): the br-hpv61 failure mode — the write commits but the client
    // sees a timeout and retries with the same key + identical payload.
    let (pool, _dir, db_path) = make_pool();
    let pid = setup_project(&pool);
    let sender = setup_agent(&pool, pid, "GreenCastle");
    let recipient = setup_agent(&pool, pid, "BlueLake");

    let fresh = send_idem(&pool, pid, sender, recipient, "hello", "K1", "fp-A");
    let IdempotentOutcome::Fresh(fresh_row) = fresh else {
        panic!("first send must be Fresh, got {fresh:?}");
    };
    let original_id = fresh_row.id.expect("fresh row has id");

    // Retry with the SAME key + payload: replay, no second insert.
    let replay = send_idem(&pool, pid, sender, recipient, "hello", "K1", "fp-A");
    match &replay {
        IdempotentOutcome::Replayed(row) => {
            assert_eq!(row.id, Some(original_id), "replay must return original id");
            assert_eq!(row.body_md, "hello", "replay must return original body");
        }
        other => panic!("retry must be Replayed, got {other:?}"),
    }
    assert!(replay.is_replayed());

    // Exactly one message row and exactly one key record.
    assert_eq!(
        count_rows(
            &db_path,
            &format!("SELECT COUNT(*) FROM messages WHERE project_id = {pid}"),
        ),
        1,
        "message must exist exactly once after a replayed retry"
    );
    assert_eq!(
        count_rows(&db_path, "SELECT COUNT(*) FROM idempotency_keys"),
        1,
        "exactly one idempotency key must be recorded"
    );
}

#[test]
fn send_message_same_key_different_payload_is_typed_conflict() {
    // Criterion (b): a planted negative — same key, different body -> conflict.
    let (pool, _dir, db_path) = make_pool();
    let pid = setup_project(&pool);
    let sender = setup_agent(&pool, pid, "GreenCastle");
    let recipient = setup_agent(&pool, pid, "BlueLake");

    let fresh = send_idem(&pool, pid, sender, recipient, "original", "K2", "fp-A");
    assert!(matches!(fresh, IdempotentOutcome::Fresh(_)));

    // Same key K2, DIFFERENT fingerprint (body changed) -> conflict.
    let conflict = send_idem(&pool, pid, sender, recipient, "tampered", "K2", "fp-B");
    match conflict {
        IdempotentOutcome::Conflict(info) => {
            assert_eq!(info.tool, "send_message");
            assert_eq!(info.key, "K2");
            assert_eq!(info.original_fingerprint, "fp-A");
            assert_eq!(info.attempted_fingerprint, "fp-B");
            assert!(info.original_created_ts > 0);
        }
        other => panic!("mismatched payload must conflict, got {other:?}"),
    }

    // No second row was written under either payload.
    assert_eq!(
        count_rows(
            &db_path,
            &format!("SELECT COUNT(*) FROM messages WHERE project_id = {pid}"),
        ),
        1,
        "a conflict must not write a second message"
    );
}

#[test]
fn acknowledge_message_idempotent_replays_and_conflicts() {
    let (pool, _dir, _db_path) = make_pool();
    let pid = setup_project(&pool);
    let sender = setup_agent(&pool, pid, "GreenCastle");
    let recipient = setup_agent(&pool, pid, "BlueLake");

    // Seed a message so there is a recipient row to acknowledge.
    let fresh = send_idem(
        &pool, pid, sender, recipient, "ack me", "SEED-ACK", "fp-seed",
    );
    let IdempotentOutcome::Fresh(row) = fresh else {
        panic!("seed send must be Fresh");
    };
    let message_id = row.id.expect("seed message id");

    let ack1 = ack_idem(&pool, pid, recipient, message_id, "AK1", "fp-A");
    let IdempotentOutcome::Fresh((read1, ackts1)) = ack1 else {
        panic!("first ack must be Fresh, got {ack1:?}");
    };
    assert!(read1 > 0 && ackts1 > 0);

    // (a) replay: same key + payload -> Replayed with identical timestamps.
    let ack2 = ack_idem(&pool, pid, recipient, message_id, "AK1", "fp-A");
    match ack2 {
        IdempotentOutcome::Replayed((read2, ackts2)) => {
            assert_eq!((read2, ackts2), (read1, ackts1), "replay returns stored ts");
        }
        other => panic!("ack retry must be Replayed, got {other:?}"),
    }

    // (b) conflict: same key, different fingerprint.
    let ack3 = ack_idem(&pool, pid, recipient, message_id, "AK1", "fp-B");
    match ack3 {
        IdempotentOutcome::Conflict(info) => {
            assert_eq!(info.tool, "acknowledge_message");
            assert_eq!(info.original_fingerprint, "fp-A");
            assert_eq!(info.attempted_fingerprint, "fp-B");
        }
        other => panic!("mismatched ack payload must conflict, got {other:?}"),
    }
}

#[test]
fn file_reservation_idempotent_replays_and_conflicts() {
    let (pool, _dir, _db_path) = make_pool();
    let pid = setup_project(&pool);
    let agent = setup_agent(&pool, pid, "GreenCastle");

    let fresh = reserve_idem(&pool, pid, agent, "src/**", "R1", "fp-A");
    let IdempotentOutcome::Fresh(rows1) = fresh else {
        panic!("first reserve must be Fresh, got {fresh:?}");
    };
    assert_eq!(rows1.len(), 1);
    let original_id = rows1[0].id.expect("reservation id");
    assert_eq!(active_reservation_count(&pool, pid), 1);

    // (a) replay: same key + payload -> same reservation row, no second lease.
    let replay = reserve_idem(&pool, pid, agent, "src/**", "R1", "fp-A");
    match replay {
        IdempotentOutcome::Replayed(rows2) => {
            assert_eq!(rows2.len(), 1);
            assert_eq!(rows2[0].id, Some(original_id), "replay returns original id");
        }
        other => panic!("reserve retry must be Replayed, got {other:?}"),
    }
    assert_eq!(
        active_reservation_count(&pool, pid),
        1,
        "replay must not create a second reservation"
    );

    // (b) conflict: same key, different payload.
    let conflict = reserve_idem(&pool, pid, agent, "src/**", "R1", "fp-B");
    match conflict {
        IdempotentOutcome::Conflict(info) => {
            assert_eq!(info.tool, "file_reservation_paths");
            assert_eq!(info.original_fingerprint, "fp-A");
            assert_eq!(info.attempted_fingerprint, "fp-B");
        }
        other => panic!("mismatched reservation payload must conflict, got {other:?}"),
    }
    assert_eq!(
        active_reservation_count(&pool, pid),
        1,
        "a conflict must not create a second reservation"
    );
}

#[test]
fn retention_window_replays_in_window_but_prunes_after_expiry() {
    // Criterion (c): a replay inside the window succeeds (covered above and here),
    // and once the record is past `expires_ts` the pruning check treats the key
    // as fresh — a subsequent call applies a NEW mutation instead of replaying.
    let (pool, _dir, db_path) = make_pool();
    let pid = setup_project(&pool);
    let agent = setup_agent(&pool, pid, "GreenCastle");

    // Prove retention via the idempotency_keys table itself (fully under this
    // layer's control), not via reservation row counts — the reservation layer
    // renews a holder's same-path lease instead of inserting a distinct row, so
    // it is the wrong observable. `created_ts` of the single key record tells us
    // whether a call recorded a NEW key (fresh) or left the record untouched
    // (replay). The Fresh/Replayed outcomes are themselves the criterion-(c)
    // proof; this corroborates them at the durable layer.
    let key_count = || count_rows(&db_path, "SELECT COUNT(*) FROM idempotency_keys");
    let key_created_ts = || count_rows(&db_path, "SELECT created_ts FROM idempotency_keys");

    // Fresh record (default 24h retention window).
    let fresh = reserve_idem(&pool, pid, agent, "docs/**", "TTL1", "fp-A");
    assert!(matches!(fresh, IdempotentOutcome::Fresh(_)));
    assert_eq!(key_count(), 1);
    let ts_original = key_created_ts();
    assert!(ts_original > 0, "fresh key must record a created_ts");

    // In-window retry replays: the stored record is returned untouched.
    let in_window = reserve_idem(&pool, pid, agent, "docs/**", "TTL1", "fp-A");
    assert!(in_window.is_replayed(), "in-window retry must replay");
    assert_eq!(key_count(), 1, "a replay records no new key");
    assert_eq!(
        key_created_ts(),
        ts_original,
        "a replay must not rewrite the key record"
    );

    // Age the key past its window; the next call prunes it and is treated fresh.
    expire_all_idempotency_keys(&db_path);
    let after_expiry = reserve_idem(&pool, pid, agent, "docs/**", "TTL1", "fp-A");
    assert!(
        matches!(after_expiry, IdempotentOutcome::Fresh(_)),
        "an expired key must be pruned and treated as fresh, got {after_expiry:?}"
    );
    // Exactly one key remains (expired pruned, fresh inserted), and it is a NEW
    // record — a strictly newer created_ts proves the prune-then-record path ran
    // rather than a stale replay.
    assert_eq!(key_count(), 1, "pruning must leave exactly the one fresh key");
    assert!(
        key_created_ts() > ts_original,
        "the pruned key was re-recorded fresh (newer created_ts)"
    );
}

#[test]
fn keys_are_scoped_per_tool() {
    // The same key value under different tools must not collide.
    let (pool, _dir, _db_path) = make_pool();
    let pid = setup_project(&pool);
    let sender = setup_agent(&pool, pid, "GreenCastle");
    let recipient = setup_agent(&pool, pid, "BlueLake");

    // Key "SHARED" first used by send_message.
    let msg = send_idem(&pool, pid, sender, recipient, "hi", "SHARED", "fp-msg");
    assert!(matches!(msg, IdempotentOutcome::Fresh(_)));

    // Same key value under file_reservation_paths must be independent (Fresh),
    // not a conflict or replay of the send_message record.
    let res = reserve_idem(&pool, pid, sender, "src/**", "SHARED", "fp-res");
    assert!(
        matches!(res, IdempotentOutcome::Fresh(_)),
        "same key under a different tool must be independent, got {res:?}"
    );
}
