#![recursion_limit = "256"]

//! Tool-level acceptance tests for client-supplied idempotency keys
//! (br-idempotency-keys-mutating-tools-h0x9k).
//!
//! These exercise the REAL `send_message` / `reply_message` / `acknowledge_message`
//! tool functions (fingerprint → idempotent DB entry point → response marker →
//! side-effect gating) against a live temp DB + `STORAGE_ROOT`, so the criteria
//! are observed end-to-end at the tool surface, not just at the DB layer:
//!   (a) a same-key + same-payload retry returns the ORIGINAL result exactly once
//!       (same message id) with `"idempotent_replay": true`, and does NOT dispatch
//!       a second git-archive write (archive dispatch at-most-once — the replay
//!       leaves the on-disk canonical-message count unchanged);
//!   (b) a same-key + DIFFERENT-payload call is rejected with the typed
//!       `IDEMPOTENCY_KEY_CONFLICT` error and writes nothing new;
//!   (c) omitting the key preserves default behavior.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{acknowledge_message, ensure_project, register_agent, send_message};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    u64::try_from(micros)
        .unwrap_or(u64::MAX)
        .wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Run `f` with a fresh temp DB + `STORAGE_ROOT`, passing the storage-root path
/// so the test can inspect the on-disk archive. Serialized (process-global env).
fn run_with_storage<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx, String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let suffix = unique_suffix();
    let db_path = format!("/tmp/idem-tool-{suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/idem-tool-storage-{suffix}");
    let env = [
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
    ];
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let out = rt.block_on(f(cx, storage_root.clone()));
        Config::reset_cached();
        out
    })
}

async fn setup_project_and_agent(ctx: &McpContext, project_key: &str, agent: &str) {
    ensure_project(ctx, project_key.to_string(), None)
        .await
        .expect("ensure_project");
    register_agent(
        ctx,
        project_key.to_string(),
        "codex-cli".to_string(),
        "gpt-5".to_string(),
        Some(agent.to_string()),
        Some("idempotency tool acceptance".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("register_agent");
    mcp_agent_mail_tools::contacts::set_contact_policy(
        ctx,
        project_key.to_string(),
        agent.to_string(),
        "open".to_string(),
    )
    .await
    .expect("set_contact_policy");
}

/// Count canonical message `.md` artifacts on disk (those under a `messages`
/// directory), which is what `try_write_message_archive` dispatches. Inbox/outbox
/// copies live under `agents/`, so they are not counted.
fn count_canonical_messages(storage_root: &str) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip the git object store; canonical .md live in the work tree.
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                walk(&path, count);
            } else if path.extension().is_some_and(|ext| ext == "md")
                && path.components().any(|c| c.as_os_str() == "messages")
            {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(Path::new(storage_root), &mut count);
    count
}

#[allow(clippy::too_many_arguments)]
async fn send_with_key(
    ctx: &McpContext,
    project_key: &str,
    sender: &str,
    to: &str,
    subject: &str,
    body: &str,
    key: Option<&str>,
) -> Result<String, fastmcp::McpError> {
    send_message(
        ctx,
        project_key.to_string(),
        sender.to_string(),
        vec![to.to_string()],
        subject.to_string(),
        body.to_string(),
        None, // cc
        None, // bcc
        None, // attachment_paths
        None, // convert_images
        None, // importance
        None, // ack_required
        None, // thread_id
        None, // topic
        None, // broadcast
        None, // auto_contact_if_blocked
        None, // sender_token
        key.map(str::to_string),
    )
    .await
}

#[test]
fn send_message_replay_is_exactly_once_and_archive_at_most_once() {
    run_with_storage(|cx, storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/idem-send-{}", unique_suffix());
        setup_project_and_agent(&ctx, &project_key, "GreenCastle").await;
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        // (a) Fresh send with key K1 — no replay marker.
        let fresh_json = send_with_key(
            &ctx,
            &project_key,
            "GreenCastle",
            "BlueLake",
            "Plan",
            "body one",
            Some("K1"),
        )
        .await
        .expect("fresh send");
        let fresh: Value = serde_json::from_str(&fresh_json).expect("fresh JSON");
        assert!(
            fresh.get("idempotent_replay").is_none(),
            "a fresh send must not carry the replay marker: {fresh_json}"
        );
        let msg_id = fresh["deliveries"][0]["payload"]["id"]
            .as_i64()
            .expect("message id");

        mcp_agent_mail_storage::wbq_flush();
        let after_fresh = count_canonical_messages(&storage_root);
        assert!(
            after_fresh >= 1,
            "fresh send must archive at least one canonical message"
        );

        // The br-hpv61 failure mode: the write committed but the client retries.
        let replay_json = send_with_key(
            &ctx,
            &project_key,
            "GreenCastle",
            "BlueLake",
            "Plan",
            "body one",
            Some("K1"),
        )
        .await
        .expect("replay send");
        let replay: Value = serde_json::from_str(&replay_json).expect("replay JSON");
        assert_eq!(
            replay.get("idempotent_replay"),
            Some(&Value::Bool(true)),
            "a same-key same-payload retry must be marked idempotent_replay: {replay_json}"
        );
        assert_eq!(
            replay["deliveries"][0]["payload"]["id"].as_i64(),
            Some(msg_id),
            "replay must return the ORIGINAL message id"
        );

        mcp_agent_mail_storage::wbq_flush();
        assert_eq!(
            count_canonical_messages(&storage_root),
            after_fresh,
            "archive dispatch must be at-most-once: a replay adds no new canonical message"
        );

        // (b) Same key K1, DIFFERENT payload -> typed conflict, nothing written.
        let conflict = send_with_key(
            &ctx,
            &project_key,
            "GreenCastle",
            "BlueLake",
            "Plan",
            "body TWO",
            Some("K1"),
        )
        .await
        .expect_err("mismatched payload under a spent key must be rejected");
        let data = conflict.data.as_ref().expect("conflict error carries data");
        assert_eq!(
            data.get("error")
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str),
            Some("IDEMPOTENCY_KEY_CONFLICT"),
            "conflict must be the typed IDEMPOTENCY_KEY_CONFLICT error"
        );
        mcp_agent_mail_storage::wbq_flush();
        assert_eq!(
            count_canonical_messages(&storage_root),
            after_fresh,
            "a conflict must not archive anything"
        );
    });
}

#[test]
fn send_message_without_key_preserves_default_behavior() {
    run_with_storage(|cx, _storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/idem-nokey-{}", unique_suffix());
        setup_project_and_agent(&ctx, &project_key, "GreenCastle").await;
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        // (c) No key: two identical sends are two distinct messages, neither
        // marked as a replay (today's behavior is preserved exactly).
        let first = send_with_key(
            &ctx,
            &project_key,
            "GreenCastle",
            "BlueLake",
            "Hi",
            "b",
            None,
        )
        .await
        .expect("first send");
        let second = send_with_key(
            &ctx,
            &project_key,
            "GreenCastle",
            "BlueLake",
            "Hi",
            "b",
            None,
        )
        .await
        .expect("second send");
        let first: Value = serde_json::from_str(&first).unwrap();
        let second: Value = serde_json::from_str(&second).unwrap();
        assert!(first.get("idempotent_replay").is_none());
        assert!(second.get("idempotent_replay").is_none());
        assert_ne!(
            first["deliveries"][0]["payload"]["id"].as_i64(),
            second["deliveries"][0]["payload"]["id"].as_i64(),
            "keyless sends must create distinct messages"
        );
    });
}

#[test]
fn acknowledge_message_replay_and_conflict() {
    run_with_storage(|cx, _storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/idem-ack-{}", unique_suffix());
        setup_project_and_agent(&ctx, &project_key, "GreenCastle").await;
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        // Seed two messages to BlueLake so it has recipient rows to acknowledge.
        let m1: Value = serde_json::from_str(
            &send_with_key(
                &ctx,
                &project_key,
                "GreenCastle",
                "BlueLake",
                "One",
                "b1",
                None,
            )
            .await
            .expect("seed 1"),
        )
        .unwrap();
        let m2: Value = serde_json::from_str(
            &send_with_key(
                &ctx,
                &project_key,
                "GreenCastle",
                "BlueLake",
                "Two",
                "b2",
                None,
            )
            .await
            .expect("seed 2"),
        )
        .unwrap();
        let id1 = m1["deliveries"][0]["payload"]["id"].as_i64().expect("id1");
        let id2 = m2["deliveries"][0]["payload"]["id"].as_i64().expect("id2");

        // Fresh ack of message 1 with key AK.
        let fresh = acknowledge_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            id1,
            Some("AK".to_string()),
        )
        .await
        .expect("fresh ack");
        let fresh: Value = serde_json::from_str(&fresh).unwrap();
        assert!(fresh.get("idempotent_replay").is_none());
        assert_eq!(fresh["acknowledged"], Value::Bool(true));

        // (a) same key + same message -> replay with the marker.
        let replay = acknowledge_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            id1,
            Some("AK".to_string()),
        )
        .await
        .expect("replay ack");
        let replay: Value = serde_json::from_str(&replay).unwrap();
        assert_eq!(replay.get("idempotent_replay"), Some(&Value::Bool(true)));
        assert_eq!(
            replay["acknowledged_at"], fresh["acknowledged_at"],
            "replay must return the original ack timestamps"
        );

        // (b) same key AK, DIFFERENT message (fingerprint differs) -> conflict.
        let conflict = acknowledge_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            id2,
            Some("AK".to_string()),
        )
        .await
        .expect_err("same key, different message must conflict");
        assert_eq!(
            conflict
                .data
                .as_ref()
                .and_then(|d| d.get("error"))
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str),
            Some("IDEMPOTENCY_KEY_CONFLICT")
        );
    });
}
