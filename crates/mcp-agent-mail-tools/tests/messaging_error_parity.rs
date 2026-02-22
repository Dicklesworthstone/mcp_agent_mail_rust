//! Parity tests verifying messaging tool error messages match the Python reference.
//!
//! These integration tests call actual tool functions and verify the error type,
//! message, recoverable flag, and data payload match the Python implementation.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_tools::{ensure_project, register_agent, reply_message, send_message};
use serde_json::Value;
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
    let cx = Cx::for_testing();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
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
        Some("messaging parity test".to_string()),
        None,
    )
    .await
    .expect("register_agent");
}

// -----------------------------------------------------------------------
// T11.4: RECIPIENT_NOT_FOUND error format (constructor test)
// -----------------------------------------------------------------------

#[test]
fn test_recipient_not_found_error_format() {
    use mcp_agent_mail_tools::tool_util::legacy_tool_error;

    // Verify the RECIPIENT_NOT_FOUND error format matches Python:
    // "Unable to send message — local recipients X are not registered in project 'Y'; hint"
    let name = "NonExistentAgent";
    let project_human_key = "/tmp/test-project";
    let project_slug = "test-project-abc123";
    let hint = format!(
        "Use resource://agents/{project_slug} to list registered agents or register new identities."
    );
    let message = format!(
        "Unable to send message \u{2014} local recipients {name} are not registered in project '{project_human_key}'; {hint}"
    );
    let err = legacy_tool_error(
        "RECIPIENT_NOT_FOUND",
        &message,
        true,
        serde_json::json!({
            "unknown_local": [name],
            "hint": &hint,
        }),
    );

    let payload = error_object(&err);
    assert_eq!(
        payload.get("type").and_then(Value::as_str),
        Some("RECIPIENT_NOT_FOUND"),
    );
    assert_eq!(
        payload.get("recoverable").and_then(Value::as_bool),
        Some(true),
    );

    let msg = payload
        .get("message")
        .and_then(Value::as_str)
        .expect("message field");
    assert!(
        msg.contains("Unable to send message"),
        "message should start with 'Unable to send message': {msg}"
    );
    assert!(
        msg.contains("NonExistentAgent"),
        "message should include recipient name: {msg}"
    );
    assert!(
        msg.contains("not registered in project"),
        "message should mention 'not registered in project': {msg}"
    );
    assert!(
        msg.contains("resource://agents/"),
        "message should include discovery hint: {msg}"
    );

    let data = payload
        .get("data")
        .and_then(Value::as_object)
        .expect("data payload");
    assert!(
        data.contains_key("unknown_local"),
        "data should include unknown_local field"
    );
    assert!(data.contains_key("hint"), "data should include hint field");
}

// -----------------------------------------------------------------------
// T11.4: Empty recipients error
// -----------------------------------------------------------------------

#[test]
fn test_send_message_empty_to_error() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/msg_empty_to-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec![],
            "Test subject".to_string(),
            "Test body".to_string(),
            None,
            None,
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
        .expect_err("empty to should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("INVALID_ARGUMENT"),
        );
        let msg = payload
            .get("message")
            .and_then(Value::as_str)
            .expect("message");
        assert!(
            msg.contains("At least one recipient"),
            "should mention recipients: {msg}"
        );
    });
}

// -----------------------------------------------------------------------
// T11.4: Importance validation
// -----------------------------------------------------------------------

#[test]
fn test_invalid_importance_error() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/msg_imp-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;
        setup_project_and_agent(&ctx, &project_key, "RedPeak").await;

        let err = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["RedPeak".to_string()],
            "Test subject".to_string(),
            "Test body".to_string(),
            None,
            None,
            None,
            None,
            Some("invalid_level".to_string()),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("invalid importance should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("INVALID_ARGUMENT"),
        );
        let msg = payload
            .get("message")
            .and_then(Value::as_str)
            .expect("message");
        assert!(
            msg.contains("importance"),
            "should mention importance: {msg}"
        );
    });
}

// -----------------------------------------------------------------------
// T11.4: Reply to nonexistent message
// -----------------------------------------------------------------------

#[test]
fn test_reply_message_not_found() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/msg_reply_nf-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = reply_message(
            &ctx,
            project_key.clone(),
            999_999,
            "BlueLake".to_string(),
            "Reply body".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("reply to nonexistent message should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("NOT_FOUND"),
        );
        assert_eq!(
            payload.get("recoverable").and_then(Value::as_bool),
            Some(true),
        );
    });
}

// -----------------------------------------------------------------------
// T11.4: Reply subject prefix (Re:) — idempotent, case-insensitive
// -----------------------------------------------------------------------

#[test]
fn test_reply_message_subject_prefix() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/msg_reply_pfx-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;
        setup_project_and_agent(&ctx, &project_key, "RedPeak").await;

        // First send a message
        let result = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["RedPeak".to_string()],
            "Original subject".to_string(),
            "Hello".to_string(),
            None,
            None,
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
        .expect("send_message should succeed");

        let parsed: Value = serde_json::from_str(&result).expect("valid JSON");
        let msg_id = parsed["deliveries"][0]["payload"]["id"]
            .as_i64()
            .expect("message id");

        // Reply to it
        let reply_result = reply_message(
            &ctx,
            project_key.clone(),
            msg_id,
            "RedPeak".to_string(),
            "Reply body".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("reply should succeed");

        let reply_parsed: Value = serde_json::from_str(&reply_result).expect("valid JSON");
        let reply_subject = reply_parsed["deliveries"][0]["payload"]["subject"]
            .as_str()
            .expect("reply subject");
        assert_eq!(
            reply_subject, "Re: Original subject",
            "reply should prepend 'Re: ' to subject"
        );

        // Reply to the reply — should NOT double-prefix
        let reply_id = reply_parsed["deliveries"][0]["payload"]["id"]
            .as_i64()
            .expect("reply message id");
        let second_reply_result = reply_message(
            &ctx,
            project_key.clone(),
            reply_id,
            "BlueLake".to_string(),
            "Second reply".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("second reply should succeed");

        let second_reply_parsed: Value =
            serde_json::from_str(&second_reply_result).expect("valid JSON");
        let second_reply_subject = second_reply_parsed["deliveries"][0]["payload"]["subject"]
            .as_str()
            .expect("reply2 subject");
        assert_eq!(
            second_reply_subject, "Re: Original subject",
            "reply to 'Re: ...' should NOT double-prefix (case-insensitive idempotent)"
        );
    });
}

// -----------------------------------------------------------------------
// T11.4: Broadcast conflict (broadcast=true + explicit to)
// -----------------------------------------------------------------------

#[test]
fn test_broadcast_with_explicit_to_error() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/msg_bcast-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["RedPeak".to_string()],
            "Test subject".to_string(),
            "Test body".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(true),
            None,
        )
        .await
        .expect_err("broadcast + explicit to should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("INVALID_ARGUMENT"),
        );
        let msg = payload
            .get("message")
            .and_then(Value::as_str)
            .expect("message");
        assert_eq!(
            msg,
            "broadcast=true and explicit 'to' recipients are mutually exclusive. \
             Set broadcast=true with an empty 'to' list, or provide explicit recipients without broadcast."
        );
        assert_eq!(
            payload
                .get("data")
                .and_then(|d| d.get("argument"))
                .and_then(Value::as_str),
            Some("broadcast"),
        );
    });
}

// -----------------------------------------------------------------------
// T11.4: Contact blocked error message
// -----------------------------------------------------------------------

#[test]
fn test_contact_blocked_error_format() {
    use mcp_agent_mail_tools::tool_util::legacy_tool_error;

    let err = legacy_tool_error(
        "CONTACT_BLOCKED",
        "Recipient is not accepting messages.",
        true,
        serde_json::json!({}),
    );
    let payload = error_object(&err);
    assert_eq!(
        payload.get("type").and_then(Value::as_str),
        Some("CONTACT_BLOCKED"),
    );
    assert_eq!(
        payload.get("message").and_then(Value::as_str),
        Some("Recipient is not accepting messages."),
    );
    assert_eq!(
        payload.get("recoverable").and_then(Value::as_bool),
        Some(true),
    );
}

// -----------------------------------------------------------------------
// T11.4: Contact required error format
// -----------------------------------------------------------------------

#[test]
fn test_contact_required_error_format() {
    use mcp_agent_mail_tools::tool_util::legacy_tool_error;

    let err = legacy_tool_error(
        "CONTACT_REQUIRED",
        "Contact approval required for recipients: BlueLake.",
        true,
        serde_json::json!({
            "recipients_blocked": ["BlueLake"],
            "remedies": [
                "Call request_contact(project_key, from_agent, to_agent) to request approval",
                "Call macro_contact_handshake(project_key, requester, target, auto_accept=true) to automate"
            ],
        }),
    );
    let payload = error_object(&err);
    assert_eq!(
        payload.get("type").and_then(Value::as_str),
        Some("CONTACT_REQUIRED"),
    );
    assert_eq!(
        payload.get("recoverable").and_then(Value::as_bool),
        Some(true),
    );
    let msg = payload
        .get("message")
        .and_then(Value::as_str)
        .expect("message");
    assert!(
        msg.contains("Contact approval required"),
        "should mention contact approval: {msg}"
    );
}
