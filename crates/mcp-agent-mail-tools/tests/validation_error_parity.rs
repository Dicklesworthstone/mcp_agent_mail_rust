//! Parity tests verifying validation error messages match the Python reference.
//!
//! Each test calls an MCP tool with invalid input and verifies the error type,
//! message, recoverable flag, and data payload match the Python implementation.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_tools::{
    ensure_project, fetch_inbox, file_reservation_paths, register_agent, send_message,
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
        Some("validation parity test".to_string()),
        None,
    )
    .await
    .expect("register_agent");
}

// -----------------------------------------------------------------------
// T8.1: INVALID_TIMESTAMP
// -----------------------------------------------------------------------

#[test]
fn test_invalid_timestamp_message() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/inv_ts-{}", unique_suffix());
        eprintln!("Testing validation: input=invalid since_ts...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = fetch_inbox(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            None,
            Some("not-a-timestamp".to_string()),
            None,
            None,
            None,
        )
        .await
        .expect_err("invalid since_ts should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("INVALID_TIMESTAMP"),
            "error type mismatch"
        );
        assert_eq!(
            payload.get("recoverable").and_then(Value::as_bool),
            Some(true),
            "recoverable mismatch"
        );

        let msg = payload
            .get("message")
            .and_then(Value::as_str)
            .expect("message field");
        assert!(
            msg.starts_with("Invalid since_ts format: 'not-a-timestamp'."),
            "message should start with param_name and raw_value: {msg}"
        );
        assert!(
            msg.contains("Expected ISO-8601 format like '2025-01-15T10:30:00+00:00' or '2025-01-15T10:30:00Z'."),
            "message should contain format examples: {msg}"
        );
        assert!(
            msg.contains("Common mistakes:"),
            "message should contain common mistakes section: {msg}"
        );

        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("data payload");
        assert_eq!(
            data.get("provided").and_then(Value::as_str),
            Some("not-a-timestamp"),
            "data.provided mismatch"
        );
        assert_eq!(
            data.get("expected_format").and_then(Value::as_str),
            Some("YYYY-MM-DDTHH:MM:SS+HH:MM"),
            "data.expected_format mismatch"
        );
    });
}

// -----------------------------------------------------------------------
// T8.1: INVALID_THREAD_ID
// -----------------------------------------------------------------------

#[test]
fn test_invalid_thread_id_message() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/inv_tid-{}", unique_suffix());
        eprintln!("Testing validation: input=invalid thread_id...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["BlueLake".to_string()],
            "Test".to_string(),
            "Test body".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("-invalid-thread".to_string()),
            None,
            None,
            None,
        )
        .await
        .expect_err("invalid thread_id should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("INVALID_THREAD_ID"),
            "error type mismatch"
        );
        assert_eq!(
            payload.get("recoverable").and_then(Value::as_bool),
            Some(true),
            "recoverable mismatch"
        );

        let msg = payload
            .get("message")
            .and_then(Value::as_str)
            .expect("message field");
        assert!(
            msg.starts_with("Invalid thread_id: '-invalid-thread'."),
            "message should start with raw_value: {msg}"
        );
        assert!(
            msg.contains("Examples: 'TKT-123', 'bd-42', 'feature-xyz'."),
            "message should contain example IDs: {msg}"
        );

        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("data payload");
        assert_eq!(
            data.get("provided").and_then(Value::as_str),
            Some("-invalid-thread"),
            "data.provided mismatch"
        );
        let examples = data
            .get("examples")
            .and_then(Value::as_array)
            .expect("data.examples array");
        assert_eq!(examples.len(), 3, "should have 3 examples");
        assert_eq!(examples[0], "TKT-123");
        assert_eq!(examples[1], "bd-42");
        assert_eq!(examples[2], "feature-xyz");
    });
}

// -----------------------------------------------------------------------
// T8.2: EMPTY_PROGRAM
// -----------------------------------------------------------------------

#[test]
fn test_empty_program_message() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/empty_prog-{}", unique_suffix());
        eprintln!("Testing validation: input=empty program...");

        let ctx = McpContext::new(cx.clone(), 1);
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let err = register_agent(
            &ctx,
            project_key.clone(),
            "   ".to_string(), // whitespace-only
            "gpt-5".to_string(),
            Some("BlueLake".to_string()),
            None,
            None,
        )
        .await
        .expect_err("empty program should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("EMPTY_PROGRAM"),
            "error type mismatch"
        );

        let expected_msg = "program cannot be empty. Provide the name of your AI coding tool \
             (e.g., 'claude-code', 'codex-cli', 'cursor', 'cline').";
        assert_eq!(
            payload.get("message").and_then(Value::as_str),
            Some(expected_msg),
            "message mismatch"
        );
    });
}

// -----------------------------------------------------------------------
// T8.2: EMPTY_MODEL
// -----------------------------------------------------------------------

#[test]
fn test_empty_model_message() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/empty_model-{}", unique_suffix());
        eprintln!("Testing validation: input=empty model...");

        let ctx = McpContext::new(cx.clone(), 1);
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let err = register_agent(
            &ctx,
            project_key.clone(),
            "codex-cli".to_string(),
            "  ".to_string(), // whitespace-only
            Some("BlueLake".to_string()),
            None,
            None,
        )
        .await
        .expect_err("empty model should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("EMPTY_MODEL"),
            "error type mismatch"
        );

        let expected_msg = "model cannot be empty. Provide the underlying model identifier \
             (e.g., 'claude-opus-4.5', 'gpt-4-turbo', 'claude-sonnet-4').";
        assert_eq!(
            payload.get("message").and_then(Value::as_str),
            Some(expected_msg),
            "message mismatch"
        );
    });
}

// -----------------------------------------------------------------------
// T8.2: INVALID_LIMIT
// -----------------------------------------------------------------------

#[test]
fn test_invalid_limit_message() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/inv_limit-{}", unique_suffix());
        eprintln!("Testing validation: input=invalid limit...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = fetch_inbox(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            None,
            None,
            Some(0),
            None,
            None,
        )
        .await
        .expect_err("limit=0 should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("INVALID_LIMIT"),
            "error type mismatch"
        );

        let expected_msg = "limit must be at least 1, got 0. Use a positive integer.";
        assert_eq!(
            payload.get("message").and_then(Value::as_str),
            Some(expected_msg),
            "message mismatch"
        );

        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("data payload");
        assert_eq!(data.get("provided"), Some(&serde_json::json!(0)));
        assert_eq!(data.get("min"), Some(&serde_json::json!(1)));
        assert_eq!(data.get("max"), Some(&serde_json::json!(1000)));
    });
}

// -----------------------------------------------------------------------
// T8.2: INVALID_LIMIT (negative)
// -----------------------------------------------------------------------

#[test]
fn test_invalid_limit_negative() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/inv_limit_neg-{}", unique_suffix());
        eprintln!("Testing validation: input=negative limit...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = fetch_inbox(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            None,
            None,
            Some(-5),
            None,
            None,
        )
        .await
        .expect_err("limit=-5 should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("INVALID_LIMIT"),
        );
        assert_eq!(
            payload.get("message").and_then(Value::as_str),
            Some("limit must be at least 1, got -5. Use a positive integer."),
        );
    });
}

// -----------------------------------------------------------------------
// T8.5: Limit capping (> 1000 caps silently)
// -----------------------------------------------------------------------

#[test]
fn test_limit_capped_at_1000() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/limit_cap-{}", unique_suffix());
        eprintln!("Testing validation: input=limit > 1000 caps...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        // Should succeed (capped at 1000, not error)
        let result = fetch_inbox(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            None,
            None,
            Some(5000),
            None,
            None,
        )
        .await
        .expect("limit > 1000 should succeed with capping");

        // Verify it returned a valid result (empty inbox)
        let parsed: Value = serde_json::from_str(&result).expect("parse result");
        assert!(
            parsed.get("messages").is_some() || parsed.is_array(),
            "capped result should be valid inbox data"
        );
    });
}

// -----------------------------------------------------------------------
// T8.5: Subject truncation at 200 chars
// -----------------------------------------------------------------------

#[test]
fn test_subject_truncation_at_200() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/subj_trunc-{}", unique_suffix());
        eprintln!("Testing validation: input=subject > 200 chars...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;
        setup_project_and_agent(&ctx, &project_key, "RedStone").await;

        let long_subject = "A".repeat(250);
        let result = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["RedStone".to_string()],
            long_subject,
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
        .expect("long subject should succeed with truncation");

        let parsed: Value = serde_json::from_str(&result).expect("parse result");
        let stored_subject = parsed["deliveries"][0]["payload"]["subject"]
            .as_str()
            .expect("result should contain deliveries[0].payload.subject");
        assert_eq!(
            stored_subject.len(),
            200,
            "subject should be truncated to 200 chars"
        );
    });
}

// -----------------------------------------------------------------------
// T8.5: Subject exactly 200 chars is NOT truncated
// -----------------------------------------------------------------------

#[test]
fn test_subject_exactly_200_not_truncated() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/subj_200-{}", unique_suffix());
        eprintln!("Testing validation: input=subject exactly 200 chars...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;
        setup_project_and_agent(&ctx, &project_key, "RedStone").await;

        let subject = "B".repeat(200);
        let result = send_message(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["RedStone".to_string()],
            subject.clone(),
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
        .expect("200-char subject should succeed without truncation");

        let parsed: Value = serde_json::from_str(&result).expect("parse result");
        let stored_subject = parsed["deliveries"][0]["payload"]["subject"]
            .as_str()
            .expect("result should contain deliveries[0].payload.subject");
        assert_eq!(
            stored_subject, &subject,
            "200-char subject should not be truncated"
        );
    });
}

// -----------------------------------------------------------------------
// T8.3: File reservation empty paths
// -----------------------------------------------------------------------

#[test]
fn test_empty_paths_validation_message() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/empty_paths-{}", unique_suffix());
        eprintln!("Testing validation: input=empty paths array...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agent(&ctx, &project_key, "BlueLake").await;

        let err = file_reservation_paths(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect_err("empty paths should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("EMPTY_PATHS"),
        );

        let msg = payload
            .get("message")
            .and_then(Value::as_str)
            .expect("message");
        assert!(
            msg.contains("paths list cannot be empty"),
            "message should mention empty paths: {msg}"
        );
        assert!(
            msg.contains("['src/api/*.py', 'config/settings.yaml']"),
            "message should contain example paths: {msg}"
        );
    });
}
