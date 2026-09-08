#![recursion_limit = "256"]

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    ensure_project, fetch_inbox, macro_contact_handshake, register_agent, request_contact,
    respond_contact, send_message, set_contact_policy,
};
use serde_json::Value;
use std::fs;
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
    let time_component = u64::try_from(micros).unwrap_or(u64::MAX);
    time_component.wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn run_serial_async<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env_suffix = unique_suffix();
    let db_path = format!("/tmp/contact-policy-parity-{env_suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/contact-policy-storage-{env_suffix}");
    with_process_env_overrides_for_test(
        &[
            ("DATABASE_URL", database_url.as_str()),
            ("STORAGE_ROOT", storage_root.as_str()),
        ],
        || {
            Config::reset_cached();
            let cx = Cx::for_testing();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            rt.block_on(f(cx))
        },
    )
}

fn error_object(err: &fastmcp::McpError) -> serde_json::Map<String, Value> {
    err.data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|root| root.get("error"))
        .and_then(Value::as_object)
        .cloned()
        .expect("error payload should contain root.error object")
}

fn create_test_ppm(path: &std::path::Path) {
    fs::write(path, b"P6\n1 1\n255\n\xff\x00\x00").expect("write test image");
}

fn assert_message_eq(expected: &str, actual: &str, scenario: &str) {
    if expected == actual {
        return;
    }
    let expected_chars: Vec<char> = expected.chars().collect();
    let actual_chars: Vec<char> = actual.chars().collect();
    let mismatch_idx = expected_chars
        .iter()
        .zip(actual_chars.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| expected_chars.len().min(actual_chars.len()));
    panic!(
        "Testing contact violation: scenario={scenario} message mismatch at char_index={mismatch_idx}\n\
         expected:\n{expected}\n\
         actual:\n{actual}"
    );
}

async fn setup_project_and_agents(ctx: &McpContext, project_key: &str, agents: &[&str]) -> String {
    let ensured = ensure_project(ctx, project_key.to_string(), None)
        .await
        .expect("ensure_project");
    let project_slug = serde_json::from_str::<Value>(&ensured)
        .ok()
        .and_then(|value| {
            value
                .get("slug")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| mcp_agent_mail_core::slugify(project_key));
    for name in agents {
        register_agent(
            ctx,
            project_key.to_string(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some((*name).to_string()),
            Some("contact policy parity test".to_string()),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("register_agent");
    }
    project_slug
}

async fn send_basic_message(
    ctx: &McpContext,
    project_key: &str,
    to: Vec<String>,
    auto_contact_if_blocked: Option<bool>,
) -> Result<String, fastmcp::McpError> {
    send_message(
        ctx,
        project_key.to_string(),
        "GreenCastle".to_string(),
        to,
        "Parity test subject".to_string(),
        "Parity test body".to_string(),
        None,
        None,
        None,
        Some(false),
        Some("normal".to_string()),
        Some(false),
        None,
        None,
        None,
        auto_contact_if_blocked,
        None,
        None, // idempotency_key
    )
    .await
}

#[test]
fn test_request_contact_uses_canonical_agent_names_in_intro() {
    run_serial_async(|cx| async move {
        let scenario = "request_contact_uses_canonical_agent_names_in_intro";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        let _project_slug =
            setup_project_and_agents(&ctx, &project_key, &["BlueLake", "GreenCastle"]).await;

        let response_json = request_contact(
            &ctx,
            project_key.clone(),
            "bluelake".to_string(),
            "greencastle".to_string(),
            None,
            Some("case mismatch regression".to_string()),
            Some(60),
            Some(false),
            None,
            None,
            None,
        )
        .await
        .expect("request_contact");
        let response: Value = serde_json::from_str(&response_json).expect("parse response");
        assert_eq!(
            response.get("from").and_then(Value::as_str),
            Some("BlueLake")
        );
        assert_eq!(
            response.get("to").and_then(Value::as_str),
            Some("GreenCastle")
        );

        let inbox_json = fetch_inbox(
            &ctx,
            project_key,
            "GreenCastle".to_string(),
            None,
            None,
            Some(20),
            Some(true),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("fetch GreenCastle inbox");
        let inbox: Vec<Value> = serde_json::from_str(&inbox_json).expect("parse inbox");
        let intro = inbox
            .iter()
            .find(|message| {
                message
                    .get("subject")
                    .and_then(Value::as_str)
                    .is_some_and(|subject| subject == "Contact request from BlueLake")
            })
            .expect("canonical contact request intro");
        assert_eq!(intro.get("from").and_then(Value::as_str), Some("BlueLake"));
        assert_eq!(
            intro.get("body_md").and_then(Value::as_str),
            Some("BlueLake requests permission to contact GreenCastle.")
        );
    });
}

async fn fetch_inbox_with_bodies(ctx: &McpContext, project_key: &str, agent: &str) -> Vec<Value> {
    let inbox_json = fetch_inbox(
        ctx,
        project_key.to_string(),
        agent.to_string(),
        None,
        None,
        Some(20),
        Some(true),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("fetch inbox");
    serde_json::from_str(&inbox_json).expect("parse inbox")
}

fn assert_only_approval_notice(
    inbox: &[Value],
    project_key: &str,
    requester: &str,
    target: &str,
) -> Value {
    let subjects: Vec<&str> = inbox
        .iter()
        .filter_map(|message| message.get("subject").and_then(Value::as_str))
        .collect();
    assert!(
        !subjects
            .iter()
            .any(|subject| subject.starts_with("Contact request from")),
        "GH#313: an auto-approved handshake must not leave a pending-looking intro: {subjects:?}"
    );
    let expected_subject = format!("Contact approved: {requester} -> {target}");
    let notice = inbox
        .iter()
        .find(|message| {
            message
                .get("subject")
                .and_then(Value::as_str)
                .is_some_and(|subject| subject == expected_subject)
        })
        .unwrap_or_else(|| panic!("approval notice missing from inbox: {subjects:?}"))
        .clone();
    assert_eq!(
        notice.get("ack_required"),
        Some(&Value::Bool(false)),
        "the approval notice must not be actionable"
    );
    let body = notice
        .get("body_md")
        .and_then(Value::as_str)
        .expect("notice body");
    assert!(body.contains("no action is required"), "{body}");
    assert!(
        body.contains(&format!(
            "respond_contact(project_key='{project_key}', to_agent='{target}', \
             from_agent='{requester}', from_project='{project_key}', accept=false)"
        )),
        "the notice must spell out the exact approved tuple: {body}"
    );
    notice
}

/// GH#313: `send_message(auto_contact_if_blocked=true)` against the default
/// `auto` policy approves the link inline. The recipient must get the intended
/// message plus a non-actionable approval notice naming the tuple — not an
/// ack-required "Contact request from X" that looks pending after approval.
#[test]
fn test_auto_contact_send_leaves_no_pending_intro_and_names_the_approved_tuple() {
    run_serial_async(|cx| async move {
        let scenario = "auto_contact_send_no_pending_intro";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        let _project_slug =
            setup_project_and_agents(&ctx, &project_key, &["GreenCastle", "RedStone"]).await;

        send_basic_message(&ctx, &project_key, vec!["RedStone".to_string()], Some(true))
            .await
            .expect("auto-contact send must succeed against the default auto policy");

        let inbox = fetch_inbox_with_bodies(&ctx, &project_key, "RedStone").await;
        assert!(
            inbox.iter().any(|message| {
                message
                    .get("subject")
                    .and_then(Value::as_str)
                    .is_some_and(|subject| subject == "Parity test subject")
            }),
            "the intended message must be delivered"
        );
        assert_only_approval_notice(&inbox, &project_key, "GreenCastle", "RedStone");

        // Answering with the requester's home project instead of the approved
        // tuple is a lookup miss on a healthy mailbox: NOT_FOUND, and the
        // envelope must not claim reads are unsafe or block edits.
        let home_key = format!("/tmp/{scenario}-home-{}", unique_suffix());
        setup_project_and_agents(&ctx, &home_key, &["GreenCastle"]).await;
        let err = respond_contact(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            "GreenCastle".to_string(),
            Some(home_key),
            true,
            None,
        )
        .await
        .expect_err("the home-project tuple was never linked");
        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("NOT_FOUND")
        );
        let envelope = payload
            .get("data")
            .and_then(|data| data.get("failure_envelope"))
            .expect("NOT_FOUND carries the failure envelope");
        assert_eq!(
            envelope.get("class").and_then(Value::as_str),
            Some("request_semantic_error")
        );
        assert_eq!(
            envelope.pointer("/policy/blocks_edits"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            envelope.pointer("/policy/safe_to_continue_read_only"),
            Some(&Value::Bool(true))
        );

        // The tuple the notice names still resolves.
        let response_json = respond_contact(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            "GreenCastle".to_string(),
            Some(project_key.clone()),
            true,
            None,
        )
        .await
        .expect("the approved tuple must resolve");
        let response: Value = serde_json::from_str(&response_json).expect("parse response");
        assert_eq!(response.get("approved"), Some(&Value::Bool(true)));
    });
}

/// GH#313: the explicit macro with `auto_accept=true` behaves the same way,
/// while a plain request still sends the actionable pending intro.
#[test]
fn test_handshake_auto_accept_sends_approval_notice_not_pending_intro() {
    run_serial_async(|cx| async move {
        let scenario = "handshake_auto_accept_notice";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        let _project_slug =
            setup_project_and_agents(&ctx, &project_key, &["BlueLake", "GreenCastle"]).await;

        let response_json = macro_contact_handshake(
            &ctx,
            project_key.clone(),
            Some("BlueLake".to_string()),
            Some("GreenCastle".to_string()),
            None,
            None,
            None,
            Some("auto-accept parity".to_string()),
            Some(true),
            Some(3600),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("auto-accept handshake");
        let response: Value = serde_json::from_str(&response_json).expect("parse handshake");
        assert_eq!(
            response.pointer("/response/approved"),
            Some(&Value::Bool(true))
        );

        let inbox = fetch_inbox_with_bodies(&ctx, &project_key, "GreenCastle").await;
        assert_eq!(inbox.len(), 1, "exactly one notice: {inbox:?}");
        assert_only_approval_notice(&inbox, &project_key, "BlueLake", "GreenCastle");
    });
}

#[test]
fn test_contact_blocked_message() {
    run_serial_async(|cx| async move {
        let scenario = "contact_blocked_message";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing contact violation: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        let _project_slug =
            setup_project_and_agents(&ctx, &project_key, &["GreenCastle", "RedStone"]).await;

        set_contact_policy(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            "block_all".to_string(),
        )
        .await
        .expect("set block_all policy");

        let err = send_basic_message(
            &ctx,
            &project_key,
            vec!["RedStone".to_string()],
            Some(false),
        )
        .await
        .expect_err("block_all should return CONTACT_BLOCKED");

        assert_message_eq(
            "Recipient is not accepting messages.",
            &err.message,
            scenario,
        );

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("CONTACT_BLOCKED")
        );
        assert_eq!(
            payload.get("message").and_then(Value::as_str),
            Some("Recipient is not accepting messages.")
        );
        assert!(
            !payload.contains_key("data"),
            "CONTACT_BLOCKED parity requires no error.data payload"
        );

        let inbox_json = fetch_inbox(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            None,
            None,
            Some(20),
            Some(true),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("fetch_inbox");
        let inbox: Vec<Value> = serde_json::from_str(&inbox_json).expect("parse inbox");
        assert!(
            inbox.is_empty(),
            "blocked sends must not create inbox entries for recipient"
        );
    });
}

#[test]
fn test_contact_required_message() {
    run_serial_async(|cx| async move {
        let scenario = "contact_required_message";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing contact violation: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        let _project_slug =
            setup_project_and_agents(&ctx, &project_key, &["GreenCastle", "BlueLake"]).await;

        set_contact_policy(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            "contacts_only".to_string(),
        )
        .await
        .expect("set contacts_only policy");

        let err = send_basic_message(
            &ctx,
            &project_key,
            vec!["BlueLake".to_string()],
            Some(false),
        )
        .await
        .expect_err("contacts_only without link should return CONTACT_REQUIRED");

        let expected = format!(
            "Contact approval required for recipients: BlueLake. \
             Before retrying, request approval with \
             `request_contact(project_key='{project_key}', from_agent='GreenCastle', to_agent='BlueLake')` \
             or run `macro_contact_handshake(project_key='{project_key}', requester='GreenCastle', \
             target='BlueLake', auto_accept=True)`. \
             Alternatively, send your message inside a recent thread that already includes them by reusing its thread_id."
        );
        assert_message_eq(&expected, &err.message, scenario);

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("CONTACT_REQUIRED")
        );
        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("CONTACT_REQUIRED should include error.data");
        assert_eq!(
            data.get("recipients_blocked"),
            Some(&serde_json::json!(["BlueLake"]))
        );
        assert_eq!(
            data.get("remedies"),
            Some(&serde_json::json!([
                "Call request_contact(project_key, from_agent, to_agent) to request approval",
                "Call macro_contact_handshake(project_key, requester, target, auto_accept=true) to automate"
            ]))
        );
        assert_eq!(
            data.get("auto_contact_attempted"),
            Some(&serde_json::json!([]))
        );

        let ttl_seconds = i64::try_from(Config::get().contact_auto_ttl_seconds).unwrap_or(i64::MAX);
        let suggested = data
            .get("suggested_tool_calls")
            .and_then(Value::as_array)
            .expect("suggested_tool_calls should be an array");
        assert_eq!(
            suggested[0],
            serde_json::json!({
                "tool": "macro_contact_handshake",
                "arguments": {
                    "project_key": project_key,
                    "requester": "GreenCastle",
                    "target": "BlueLake",
                    "auto_accept": true,
                    "ttl_seconds": ttl_seconds,
                }
            })
        );
        assert_eq!(
            suggested[1],
            serde_json::json!({
                "tool": "request_contact",
                "arguments": {
                    "project_key": project_key,
                    "from_agent": "GreenCastle",
                    "to_agent": "BlueLake",
                    "ttl_seconds": ttl_seconds,
                }
            })
        );
    });
}

#[test]
fn test_mixed_recipients_partial_block() {
    run_serial_async(|cx| async move {
        let scenario = "mixed_recipients_partial_block";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing contact violation: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        let _project_slug =
            setup_project_and_agents(&ctx, &project_key, &["GreenCastle", "BlueLake", "RedStone"])
                .await;

        set_contact_policy(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            "open".to_string(),
        )
        .await
        .expect("set open policy");
        set_contact_policy(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            "contacts_only".to_string(),
        )
        .await
        .expect("set contacts_only policy");

        let err = send_basic_message(
            &ctx,
            &project_key,
            vec!["BlueLake".to_string(), "RedStone".to_string()],
            Some(false),
        )
        .await
        .expect_err("mixed recipients should fail when one requires approval");
        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("CONTACT_REQUIRED")
        );

        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("CONTACT_REQUIRED should include data");
        assert_eq!(
            data.get("recipients_blocked"),
            Some(&serde_json::json!(["RedStone"]))
        );

        // Blocking checks must run before persisting/sending any message.
        let blue_inbox_json = fetch_inbox(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            None,
            None,
            Some(20),
            Some(true),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("fetch BlueLake inbox");
        let blue_inbox: Vec<Value> = serde_json::from_str(&blue_inbox_json).expect("parse inbox");
        assert!(
            blue_inbox.is_empty(),
            "blocking checks must happen before any recipient delivery"
        );
    });
}

#[test]
fn test_contact_block_prevents_attachment_archive_artifacts() {
    run_serial_async(|cx| async move {
        let scenario = "contact_block_prevents_attachment_archive_artifacts";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        let project_slug =
            setup_project_and_agents(&ctx, &project_key, &["GreenCastle", "BlueLake"]).await;

        set_contact_policy(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            "contacts_only".to_string(),
        )
        .await
        .expect("set contacts_only policy");

        let attachment_path = std::path::Path::new(&project_key).join("pixel.ppm");
        fs::create_dir_all(&project_key).expect("create project dir");
        create_test_ppm(&attachment_path);

        let err = send_message(
            &ctx,
            project_key.clone(),
            "GreenCastle".to_string(),
            vec!["BlueLake".to_string()],
            "Attachment parity test".to_string(),
            "![pixel](pixel.ppm)".to_string(),
            None,
            None,
            Some(vec!["pixel.ppm".to_string()]),
            Some(true),
            Some("normal".to_string()),
            Some(false),
            None,
            None,
            None,
            Some(false),
            None,
            None, // idempotency_key
        )
        .await
        .expect_err("contacts_only recipient should block send before attachment writes");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("CONTACT_REQUIRED")
        );

        let attachments_dir = Config::get()
            .storage_root
            .join("projects")
            .join(project_slug)
            .join("attachments");
        assert!(
            !attachments_dir.exists(),
            "blocked send must not materialize archive attachments at {}",
            attachments_dir.display()
        );
    });
}
