use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_tools::{
    ensure_project, fetch_inbox, register_agent, send_message, set_contact_policy,
};
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

async fn setup_project_and_agents(ctx: &McpContext, project_key: &str, agents: &[&str]) {
    ensure_project(ctx, project_key.to_string(), None)
        .await
        .expect("ensure_project");
    for name in agents {
        register_agent(
            ctx,
            project_key.to_string(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some((*name).to_string()),
            Some("contact policy parity test".to_string()),
            None,
        )
        .await
        .expect("register_agent");
    }
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
        auto_contact_if_blocked,
    )
    .await
}

#[test]
fn test_contact_blocked_message() {
    run_serial_async(|cx| async move {
        let scenario = "contact_blocked_message";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing contact violation: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
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
