#![recursion_limit = "256"]

//! Parity tests verifying messaging tool error messages match the Python reference.
//!
//! These integration tests call actual tool functions and verify the error type,
//! message, recoverable flag, and data payload match the Python implementation.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
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
    run_serial_async_with_env(&[], f)
}

fn run_serial_async_with_env<F, Fut, T>(extra_env: &[(&str, &str)], f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env_suffix = unique_suffix();
    let db_path = format!("/tmp/messaging-error-parity-{env_suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/messaging-error-storage-{env_suffix}");
    let mut env: Vec<(&str, &str)> = vec![
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
    ];
    env.extend_from_slice(extra_env);
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let out = rt.block_on(f(cx));
        // Do not leak a profile-altered cached Config into later tests.
        Config::reset_cached();
        out
    })
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
        "Unable to send message &#x2014; local recipients {name} are not registered in project '{project_human_key}'; {hint}"
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
            None, // idempotency_key
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
            None,                              // cc
            None,                              // bcc
            None,                              // attachment_paths
            None,                              // convert_images
            Some("invalid_level".to_string()), // importance
            None,                              // ack_required
            None,                              // thread_id
            None,                              // topic
            None,                              // broadcast
            None,                              // auto_contact_if_blocked
            None,                              // sender_token
            None,                              // idempotency_key
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
            None, // to
            None, // cc
            None, // bcc
            None, // subject_prefix
            None, // importance
            None, // ack_required
            None, // attachment_paths
            None, // convert_images
            None, // sender_token
            None, // idempotency_key
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
// GH#204: cross-project reply stays rejected, but names the real owner
// -----------------------------------------------------------------------

/// `reply_message` is the only message surface that gates on project row
/// identity, which made a forked-identity mailbox surface as a bare
/// "Message not found" that misdirected debugging toward the message store.
///
/// This locks in both halves of the fix: a reply aimed at a genuinely
/// different project is still refused (the security property), and the error
/// now identifies the owning project instead of implying the id is unknown.
///
/// Note the alias-*acceptance* half of GH#204 only arises on case-insensitive
/// filesystems, where two case-variant keys denote one logical project. On a
/// case-sensitive filesystem they are legitimately distinct projects, so it is
/// not expressible here and is deliberately not asserted.
#[test]
fn test_reply_message_cross_project_reports_owning_project() {
    run_serial_async(|cx| async move {
        let suffix = unique_suffix();
        // Keys must not collide under `slugify`, which lowercases and collapses
        // every non-alphanumeric run to a single dash. `/tmp/a-b` and
        // `/tmp/a_b` produce the same slug, and since `projects.slug` is
        // UNIQUE, `ensure_project` on the second key simply returns the first
        // project's row — so a punctuation-variant pair is one project, not
        // two, and never exercises this gate at all.
        let owning_key = format!("/tmp/msg-reply-owner-{suffix}");
        let other_key = format!("/tmp/msg-reply-other-{suffix}");
        let ctx = McpContext::new(cx.clone(), 1);

        setup_project_and_agent(&ctx, &owning_key, "BlueLake").await;
        setup_project_and_agent(&ctx, &owning_key, "RedPeak").await;
        setup_project_and_agent(&ctx, &other_key, "GreenLake").await;

        let result = send_message(
            &ctx,
            owning_key.clone(),
            "BlueLake".to_string(),
            vec!["RedPeak".to_string()],
            "Owned subject".to_string(),
            "Hello".to_string(),
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
            None, // idempotency_key
        )
        .await
        .expect("send_message should succeed");

        let parsed: Value = serde_json::from_str(&result).expect("valid JSON");
        let msg_id = parsed["deliveries"][0]["payload"]["id"]
            .as_i64()
            .expect("message id");

        // Replying from an unrelated project must still be refused.
        let err = reply_message(
            &ctx,
            other_key.clone(),
            msg_id,
            "GreenLake".to_string(),
            "Reply body".to_string(),
            None, // to
            None, // cc
            None, // bcc
            None, // subject_prefix
            None, // importance
            None, // ack_required
            None, // attachment_paths
            None, // convert_images
            None, // sender_token
            None, // idempotency_key
        )
        .await
        .expect_err("cross-project reply must be refused");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("NOT_FOUND")
        );
        assert_eq!(
            payload.get("recoverable").and_then(Value::as_bool),
            Some(true),
        );

        // The diagnostic must say the id exists but is out of scope, rather
        // than implying it does not exist at all...
        let details = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("error should carry structured data");
        assert_eq!(
            details.get("requested_project_key").and_then(Value::as_str),
            Some(other_key.as_str()),
            "error should record which project was asked for"
        );
        assert_eq!(
            details
                .get("belongs_to_other_project")
                .and_then(Value::as_bool),
            Some(true),
            "error should distinguish 'wrong project' from 'no such message'"
        );

        // ...but it must NOT disclose the owning project. Message ids are
        // globally sequential, so leaking the owner's key here would let any
        // agent enumerate ids to map out projects it cannot access.
        let rendered = serde_json::to_string(&payload).expect("serialize error payload");
        assert!(
            !rendered.contains(owning_key.as_str()),
            "error payload must not disclose the owning project key: {rendered}"
        );
        assert!(
            !err.message.contains(owning_key.as_str()),
            "error message must not disclose the owning project key: {}",
            err.message
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
            None, // idempotency_key
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
            None, // to
            None, // cc
            None, // bcc
            None, // subject_prefix
            None, // importance
            None, // ack_required
            None, // attachment_paths
            None, // convert_images
            None, // sender_token
            None, // idempotency_key
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
            None, // to
            None, // cc
            None, // bcc
            None, // subject_prefix
            None, // importance
            None, // ack_required
            None, // attachment_paths
            None, // convert_images
            None, // sender_token
            None, // idempotency_key
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
            None,       // cc
            None,       // bcc
            None,       // attachment_paths
            None,       // convert_images
            None,       // importance
            None,       // ack_required
            None,       // thread_id
            None,       // topic
            Some(true), // broadcast
            None,       // auto_contact_if_blocked
            None,       // sender_token
            None,       // idempotency_key
        )
        .await
        .expect_err("broadcast + explicit to should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("BROADCAST_DISABLED"),
        );
        let msg = payload
            .get("message")
            .and_then(Value::as_str)
            .expect("message");
        assert_eq!(
            msg,
            "broadcast=true is intentionally unsupported to prevent agent spam. Address agents explicitly and omit the broadcast flag."
        );
        assert_eq!(
            payload
                .get("data")
                .and_then(|d| d.get("argument"))
                .and_then(Value::as_str),
            Some("broadcast"),
        );
        assert_eq!(
            payload
                .get("data")
                .and_then(|d| d.get("recipients_supplied"))
                .and_then(Value::as_bool),
            Some(true),
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

// -----------------------------------------------------------------------
// GH#237: reply_message must honor the fail-closed send profile — it is a
// message-creation path and must not be the token-free way to speak as
// another agent.
// -----------------------------------------------------------------------

async fn register_agent_with_token(ctx: &McpContext, project_key: &str, agent: &str) -> String {
    let response = register_agent(
        ctx,
        project_key.to_string(),
        "codex-cli".to_string(),
        "gpt-5".to_string(),
        Some(agent.to_string()),
        Some("fail-closed reply test".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("register_agent");
    let parsed: Value = serde_json::from_str(&response).expect("register_agent JSON");
    let token = parsed
        .get("registration_token")
        .and_then(Value::as_str)
        .expect("registration_token in register_agent response")
        .to_string();
    mcp_agent_mail_tools::contacts::set_contact_policy(
        ctx,
        project_key.to_string(),
        agent.to_string(),
        "open".to_string(),
    )
    .await
    .expect("set_contact_policy");
    token
}

#[test]
#[allow(clippy::too_many_lines)]
fn fail_closed_profile_gates_reply_message_sender_verification() {
    run_serial_async_with_env(
        &[("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "1")],
        |cx| async move {
            let project_key = format!("/tmp/msg_reply_fc-{}", unique_suffix());
            let ctx = McpContext::new(cx.clone(), 1);
            ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project");
            let blue_token = register_agent_with_token(&ctx, &project_key, "BlueLake").await;
            let red_token = register_agent_with_token(&ctx, &project_key, "RedPeak").await;

            let send_response = send_message(
                &ctx,
                project_key.clone(),
                "BlueLake".to_string(),
                vec!["RedPeak".to_string()],
                "Fail-closed reply gating".to_string(),
                "Original body".to_string(),
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
                Some(blue_token),
                None, // idempotency_key
            )
            .await
            .expect("verified send under the fail-closed profile");
            let send_json: Value = serde_json::from_str(&send_response).expect("send_message JSON");
            let message_id = send_json
                .get("message_id")
                .or_else(|| send_json.get("id"))
                .and_then(Value::as_i64)
                .expect("message id in send response");

            // Token-free reply must be refused BEFORE any write.
            let err = reply_message(
                &ctx,
                project_key.clone(),
                message_id,
                "RedPeak".to_string(),
                "Unverified reply".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None, // idempotency_key
            )
            .await
            .expect_err("token-free reply must be refused under the fail-closed profile");
            let payload = error_object(&err);
            assert_eq!(
                payload.get("type").and_then(Value::as_str),
                Some("SENDER_TOKEN_REQUIRED"),
                "reply must route through verify_sender_identity: {payload:?}"
            );

            // A verified reply succeeds — and returns the redacted receipt
            // shape, mirroring send_message under the profile (GH#237).
            let reply_response = reply_message(
                &ctx,
                project_key.clone(),
                message_id,
                "RedPeak".to_string(),
                "Verified reply".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(red_token),
                None, // idempotency_key
            )
            .await
            .expect("verified reply under the fail-closed profile");
            let reply_json: Value =
                serde_json::from_str(&reply_response).expect("reply_message JSON");
            assert_eq!(
                reply_json.get("receipt_mode").and_then(Value::as_str),
                Some("redacted"),
                "reply under the fail-closed profile must return a redacted receipt: {reply_json}"
            );
            assert_eq!(
                reply_json.get("reply_to").and_then(Value::as_i64),
                Some(message_id)
            );
            assert_eq!(
                reply_json.get("verified_sender").and_then(Value::as_bool),
                Some(true)
            );
            assert_eq!(
                reply_json
                    .pointer("/target_outcomes/0/recipient")
                    .and_then(Value::as_str),
                Some("BlueLake"),
                "per-target outcomes retained: {reply_json}"
            );
            for forbidden in ["subject", "body_md", "attachments", "deliveries"] {
                assert!(
                    reply_json.get(forbidden).is_none(),
                    "redacted reply receipt leaked {forbidden}: {reply_json}"
                );
            }
        },
    );
}

#[test]
fn reply_message_without_profile_returns_full_payload() {
    // GH#237 control: without the fail-closed profile the reply response is
    // the unchanged full shape (subject/body/deliveries present).
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/msg_reply_full-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;
        setup_project_and_agent(&ctx, &project_key, "RedPeak").await;

        let send_response = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["RedPeak".to_string()],
            "Full-shape reply control".to_string(),
            "Original body".to_string(),
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
            None,
            None, // idempotency_key
        )
        .await
        .expect("send without profile");
        let send_json: Value = serde_json::from_str(&send_response).expect("send_message JSON");
        let message_id = send_json
            .pointer("/deliveries/0/payload/id")
            .and_then(Value::as_i64)
            .expect("message id in full send response");

        let reply_response = reply_message(
            &ctx,
            project_key.clone(),
            message_id,
            "RedPeak".to_string(),
            "Reply body".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // idempotency_key
        )
        .await
        .expect("reply without profile");
        let reply_json: Value = serde_json::from_str(&reply_response).expect("reply_message JSON");
        assert!(
            reply_json.get("receipt_mode").is_none(),
            "no redaction without the profile: {reply_json}"
        );
        assert_eq!(
            reply_json.get("body_md").and_then(Value::as_str),
            Some("Reply body")
        );
        assert_eq!(
            reply_json.get("reply_to").and_then(Value::as_i64),
            Some(message_id)
        );
        assert!(reply_json.get("deliveries").is_some());
        assert!(reply_json.get("subject").is_some());
    });
}

// -----------------------------------------------------------------------
// GH#237: macro_contact_handshake must thread sender_token through to the
// welcome send_message call — a hardcoded None made the welcome message
// unconditionally fail closed under MESSAGING_FAIL_CLOSED_SEND_PROFILE.
// -----------------------------------------------------------------------

#[test]
fn fail_closed_profile_handshake_welcome_requires_and_accepts_sender_token() {
    run_serial_async_with_env(
        &[("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "1")],
        |cx| async move {
            let project_key = format!("/tmp/msg_handshake_fc-{}", unique_suffix());
            let ctx = McpContext::new(cx.clone(), 1);
            ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project");
            let blue_token = register_agent_with_token(&ctx, &project_key, "BlueLake").await;
            register_agent_with_token(&ctx, &project_key, "RedPeak").await;

            // Token-free handshake with a welcome message fails closed with
            // the same error class as send_message.
            let err = mcp_agent_mail_tools::macro_contact_handshake(
                &ctx,
                project_key.clone(),
                Some("BlueLake".to_string()),
                Some("RedPeak".to_string()),
                None,
                None,
                None,
                None,
                Some(true), // auto_accept
                None,
                Some("Welcome".to_string()),
                Some("Welcome aboard".to_string()),
                None,
                None,
                None,
                None,
                None,
                None, // sender_token
            )
            .await
            .expect_err("token-free welcome must fail closed under the profile");
            let payload = error_object(&err);
            assert_eq!(
                payload.get("type").and_then(Value::as_str),
                Some("SENDER_TOKEN_REQUIRED"),
                "handshake welcome must fail closed exactly like send_message: {payload:?}"
            );

            // With a valid requester token the handshake (including the
            // welcome message) succeeds.
            let response = mcp_agent_mail_tools::macro_contact_handshake(
                &ctx,
                project_key.clone(),
                Some("BlueLake".to_string()),
                Some("RedPeak".to_string()),
                None,
                None,
                None,
                None,
                Some(true), // auto_accept
                None,
                Some("Welcome".to_string()),
                Some("Welcome aboard".to_string()),
                None,
                None,
                None,
                None,
                None,
                Some(blue_token),
            )
            .await
            .expect("handshake welcome with a valid sender_token succeeds under the profile");
            let json: Value = serde_json::from_str(&response).expect("handshake JSON");
            let welcome = json
                .get("welcome_message")
                .expect("welcome_message present in handshake response");
            assert!(!welcome.is_null(), "welcome message must have been sent");
            assert_eq!(
                welcome.get("receipt_mode").and_then(Value::as_str),
                Some("redacted"),
                "welcome send under the profile is the redacted receipt: {welcome}"
            );
            assert_eq!(
                welcome.get("verified_sender").and_then(Value::as_bool),
                Some(true)
            );
        },
    );
}
