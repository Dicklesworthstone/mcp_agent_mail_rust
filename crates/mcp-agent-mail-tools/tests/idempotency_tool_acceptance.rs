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
use mcp_agent_mail_tools::{
    acknowledge_message, ensure_project, fetch_inbox, register_agent, reply_message, send_message,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
    run_with_storage_env(&[("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "0")], f)
}

fn run_with_storage_env<F, Fut, T>(extra_env: &[(&str, &str)], f: F) -> T
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
    let mut env = vec![
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
    ];
    env.extend_from_slice(extra_env);
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let out = rt.block_on(async {
            let cx = Cx::current().expect("runtime installs the tool test context");
            f(cx, storage_root.clone()).await
        });
        Config::reset_cached();
        out
    })
}

async fn setup_project_and_agent(ctx: &McpContext, project_key: &str, agent: &str) -> String {
    ensure_project(ctx, project_key.to_string(), None)
        .await
        .expect("ensure_project");
    let registration = register_agent(
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
    serde_json::from_str::<Value>(&registration).unwrap()["registration_token"]
        .as_str()
        .expect("registration token")
        .to_string()
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

async fn attachment_request(
    ctx: &McpContext,
    project_key: &str,
    source: &Path,
    parent: Option<i64>,
    body: &str,
    token: Option<&str>,
) -> Result<String, fastmcp::McpError> {
    let attachments = Some(vec![source.to_string_lossy().into_owned()]);
    let bcc = Some(vec!["RedFox".to_string()]);
    let token = token.map(str::to_string);
    let key = Some("attachment-retry".to_string());
    if let Some(parent) = parent {
        reply_message(
            ctx,
            project_key.to_string(),
            parent,
            "GreenCastle".to_string(),
            body.to_string(),
            Some(vec!["BlueLake".to_string()]),
            None,
            bcc,
            None,
            None,
            None,
            attachments,
            Some(false),
            token,
            key,
        )
        .await
    } else {
        send_message(
            ctx,
            project_key.to_string(),
            "GreenCastle".to_string(),
            vec!["BlueLake".to_string()],
            "Attachment retry".to_string(),
            body.to_string(),
            None,
            bcc,
            attachments,
            Some(false),
            None,
            None,
            None,
            None,
            None,
            None,
            token,
            key,
        )
        .await
    }
}

fn snapshot_delivery_files(storage_root: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_none_or(|name| name != ".git") {
                    visit(&path, files);
                }
            } else if path.components().any(|part| {
                matches!(
                    part.as_os_str().to_str(),
                    Some("messages" | "inbox" | "outbox" | "attachments")
                )
            }) {
                files.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(&Path::new(storage_root).join("projects"), &mut files);
    files
}

async fn inbox_snapshot(ctx: &McpContext, project_key: &str) -> Value {
    let inbox = fetch_inbox(
        ctx,
        project_key.to_string(),
        "BlueLake".to_string(),
        None,
        None,
        Some(100),
        Some(true),
        None,
        None,
        None,
        Some(false),
    )
    .await
    .expect("peek persisted inbox");
    serde_json::from_str(&inbox).unwrap()
}

fn assert_error_type(error: &fastmcp::McpError, expected: &str) {
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("error"))
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str),
        Some(expected)
    );
}

fn assert_exact_replay(fresh: &Value, replay: &str) {
    let mut replay: Value = serde_json::from_str(replay).unwrap();
    assert_eq!(
        replay.as_object_mut().unwrap().remove("idempotent_replay"),
        Some(Value::Bool(true))
    );
    assert_eq!(&replay, fresh);
}

async fn attachment_replay_case(cx: Cx, storage_root: String, is_reply: bool) {
    let ctx = McpContext::new(cx, 1);
    let project_key = format!("{storage_root}/workspace");
    std::fs::create_dir_all(&project_key).unwrap();
    for agent in ["GreenCastle", "BlueLake", "RedFox"] {
        setup_project_and_agent(&ctx, &project_key, agent).await;
    }
    let parent = if is_reply {
        let original = send_with_key(
            &ctx,
            &project_key,
            "GreenCastle",
            "BlueLake",
            "Parent",
            "Original body",
            None,
        )
        .await
        .unwrap();
        Some(
            serde_json::from_str::<Value>(&original).unwrap()["deliveries"][0]["payload"]["id"]
                .as_i64()
                .unwrap(),
        )
    } else {
        None
    };
    let source = Path::new(&project_key).join("source.bin");
    let original_bytes = b"accepted attachment bytes";
    std::fs::write(&source, original_bytes).unwrap();
    let body = "The accepted body and attachment must survive retries.";
    let fresh = attachment_request(&ctx, &project_key, &source, parent, body, None)
        .await
        .expect("fresh attachment send");
    let fresh: Value = serde_json::from_str(&fresh).unwrap();
    assert!(
        !fresh["deliveries"][0]["payload"]["attachments"]
            .as_array()
            .unwrap()
            .is_empty(),
        "fresh delivery must contain the accepted attachment"
    );
    mcp_agent_mail_storage::wbq_flush();
    let files = snapshot_delivery_files(&storage_root);
    let inbox = inbox_snapshot(&ctx, &project_key).await;
    assert_eq!(
        count_canonical_messages(&storage_root),
        if is_reply { 2 } else { 1 }
    );

    let retained = source.with_extension("retained.bin");
    std::fs::rename(&source, &retained).unwrap();
    let replay = attachment_request(&ctx, &project_key, &source, parent, body, None)
        .await
        .expect("retry must not reopen a missing original attachment");
    assert_exact_replay(&fresh, &replay);
    let conflict = attachment_request(&ctx, &project_key, &source, parent, "changed body", None)
        .await
        .expect_err("a changed request must conflict before checking missing source files");
    assert_error_type(&conflict, "IDEMPOTENCY_KEY_CONFLICT");

    std::fs::write(&source, b"new bytes at the old source path").unwrap();
    let replay = attachment_request(&ctx, &project_key, &source, parent, body, None)
        .await
        .expect("retry must use the accepted attachment metadata");
    assert_exact_replay(&fresh, &replay);
    mcp_agent_mail_storage::wbq_flush();
    assert_eq!(snapshot_delivery_files(&storage_root), files);
    assert_eq!(inbox_snapshot(&ctx, &project_key).await, inbox);
    assert_eq!(std::fs::read(retained).unwrap(), original_bytes);
    assert_eq!(
        std::fs::read(source).unwrap(),
        b"new bytes at the old source path"
    );
}

#[test]
fn send_message_attachment_replay_ignores_missing_and_changed_sources() {
    run_with_storage(|cx, storage_root| attachment_replay_case(cx, storage_root, false));
}

#[test]
fn reply_message_attachment_replay_ignores_missing_and_changed_sources() {
    run_with_storage(|cx, storage_root| attachment_replay_case(cx, storage_root, true));
}

#[test]
fn attachment_replays_require_current_sender_proof_and_keep_redacted_receipts() {
    run_with_storage_env(
        &[("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "1")],
        |cx, storage_root| async move {
            let ctx = McpContext::new(cx, 1);
            let project_key = format!("{storage_root}/workspace");
            std::fs::create_dir_all(&project_key).unwrap();
            let token = setup_project_and_agent(&ctx, &project_key, "GreenCastle").await;
            for agent in ["BlueLake", "RedFox"] {
                setup_project_and_agent(&ctx, &project_key, agent).await;
            }
            let source = Path::new(&project_key).join("private.bin");
            std::fs::write(&source, b"private attachment").unwrap();
            let body = "Private accepted body";
            let fresh_send =
                attachment_request(&ctx, &project_key, &source, None, body, Some(&token))
                    .await
                    .expect("verified fresh send");
            let fresh_send: Value = serde_json::from_str(&fresh_send).unwrap();
            let parent = fresh_send["message_id"].as_i64().unwrap();
            let fresh_reply = attachment_request(
                &ctx,
                &project_key,
                &source,
                Some(parent),
                body,
                Some(&token),
            )
            .await
            .expect("verified fresh reply");
            let fresh_reply: Value = serde_json::from_str(&fresh_reply).unwrap();
            mcp_agent_mail_storage::wbq_flush();
            let files = snapshot_delivery_files(&storage_root);
            let inbox = inbox_snapshot(&ctx, &project_key).await;
            std::fs::rename(&source, source.with_extension("retained.bin")).unwrap();

            for (parent, fresh) in [(None, fresh_send), (Some(parent), fresh_reply)] {
                for (proof, expected) in [
                    (None, "SENDER_TOKEN_REQUIRED"),
                    (
                        Some("incorrect-registration-token"),
                        "SENDER_TOKEN_MISMATCH",
                    ),
                ] {
                    let error =
                        attachment_request(&ctx, &project_key, &source, parent, body, proof)
                            .await
                            .expect_err(
                                "a prior accepted key must not bypass current sender proof",
                            );
                    assert_error_type(&error, expected);
                }
                let replay =
                    attachment_request(&ctx, &project_key, &source, parent, body, Some(&token))
                        .await
                        .expect("verified replay after the source was moved");
                assert_exact_replay(&fresh, &replay);
                assert_eq!(fresh["receipt_mode"], "redacted");
                assert_eq!(fresh["verified_sender"], true);
                for field in [
                    "subject",
                    "body_md",
                    "attachments",
                    "deliveries",
                    "to",
                    "cc",
                    "bcc",
                ] {
                    assert!(
                        fresh.get(field).is_none(),
                        "redacted receipt exposed {field}"
                    );
                }
            }
            mcp_agent_mail_storage::wbq_flush();
            assert_eq!(snapshot_delivery_files(&storage_root), files);
            assert_eq!(inbox_snapshot(&ctx, &project_key).await, inbox);
        },
    );
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
