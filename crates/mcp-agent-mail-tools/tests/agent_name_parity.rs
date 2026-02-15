use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::models::{
    BROADCAST_TOKENS, KNOWN_PROGRAM_NAMES, MODEL_NAME_PATTERNS, detect_agent_name_mistake,
};
use mcp_agent_mail_tools::{ensure_project, register_agent};
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

fn assert_message_eq(expected: &str, actual: &str, name: &str) {
    if expected == actual {
        return;
    }
    let mismatch = expected
        .chars()
        .zip(actual.chars())
        .position(|(e, a)| e != a)
        .unwrap_or_else(|| expected.chars().count().min(actual.chars().count()));
    panic!(
        "FAIL: \"{name}\" message diff at char {mismatch}: expected \"{expected}\", got \"{actual}\""
    );
}

fn assert_detect(name: &str, expected_category: &str, expected_message: &str) {
    eprintln!("Testing agent name \"{name}\"...");
    let Some((actual_category, actual_msg)) = detect_agent_name_mistake(name) else {
        panic!("FAIL: \"{name}\" detected as none, expected {expected_category}");
    };
    eprintln!("Detected as {actual_category}: message=\"{actual_msg}\"");
    assert_eq!(
        actual_category, expected_category,
        "FAIL: \"{name}\" detected as {actual_category}, expected {expected_category}"
    );
    assert_message_eq(expected_message, &actual_msg, name);
}

#[test]
fn test_program_name_detection() {
    for name in KNOWN_PROGRAM_NAMES {
        let expected = format!(
            "'{name}' looks like a program name, not an agent name. \
             Agent names must be adjective+noun combinations like 'BlueLake' or 'GreenCastle'. \
             Use the 'program' parameter for program names, and omit 'name' to auto-generate a valid agent name."
        );
        assert_detect(name, "PROGRAM_NAME_AS_AGENT", &expected);
    }
}

#[test]
fn test_model_name_detection() {
    let pattern_samples: Vec<(&str, String)> = MODEL_NAME_PATTERNS
        .iter()
        .map(|p| (*p, format!("{p}sample")))
        .collect();
    for (_pattern, sample) in pattern_samples {
        let expected = format!(
            "'{sample}' looks like a model name, not an agent name. \
             Agent names must be adjective+noun combinations like 'RedStone' or 'PurpleBear'. \
             Use the 'model' parameter for model names, and omit 'name' to auto-generate a valid agent name."
        );
        assert_detect(&sample, "MODEL_NAME_AS_AGENT", &expected);
    }
}

#[test]
fn test_email_detection() {
    let name = "user@example.com";
    let expected = format!(
        "'{name}' looks like an email address. Agent names are simple identifiers like 'BlueDog', \
         not email addresses. Check the 'to' parameter format."
    );
    assert_detect(name, "EMAIL_AS_AGENT", &expected);
}

#[test]
fn test_broadcast_detection() {
    for token in BROADCAST_TOKENS {
        let expected = format!(
            "'{token}' looks like a broadcast attempt. Agent Mail doesn't support broadcasting to all agents. \
             List specific recipient agent names in the 'to' parameter."
        );
        assert_detect(token, "BROADCAST_ATTEMPT", &expected);
    }
}

#[test]
fn test_descriptive_name_detection() {
    for name in ["my-agent", "test_agent", "Agent1"] {
        let expected = format!(
            "'{name}' looks like a descriptive role name. Agent names must be randomly generated \
             adjective+noun combinations like 'WhiteMountain' or 'BrownCreek', NOT descriptive of the agent's task. \
             Omit the 'name' parameter to auto-generate a valid name."
        );
        assert_detect(name, "DESCRIPTIVE_NAME", &expected);
    }
}

#[test]
fn test_unix_username_detection() {
    for name in ["ubuntu", "root", "admin"] {
        let expected = format!(
            "'{name}' looks like a Unix username (possibly from $USER environment variable). \
             Agent names must be adjective+noun combinations like 'BlueLake' or 'GreenCastle'. \
             When you called register_agent, the system likely auto-generated a valid name for you. \
             To find your actual agent name, check the response from register_agent or use \
             resource://agents/{{project_key}} to list all registered agents in this project."
        );
        assert_detect(name, "UNIX_USERNAME_AS_AGENT", &expected);
    }
}

#[test]
fn test_invalid_format_detection() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/agent-name-invalid-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);

        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let invalid = "FooBar123".to_string();
        let err = register_agent(
            &ctx,
            project_key,
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some(invalid.clone()),
            Some("parity test".to_string()),
            None,
        )
        .await
        .expect_err("invalid format should fail");

        let payload = err
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("error"))
            .and_then(Value::as_object)
            .cloned()
            .expect("error payload should contain root.error object");
        let actual_category = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            actual_category, "INVALID_AGENT_NAME",
            "FAIL: \"{invalid}\" detected as {actual_category}, expected INVALID_AGENT_NAME"
        );

        let expected = format!(
            "Invalid agent name format: '{invalid}'. \
Agent names MUST be randomly generated adjective+noun combinations \
(e.g., 'GreenLake', 'BlueDog'), NOT descriptive names. \
Omit the 'name' parameter to auto-generate a valid name."
        );
        assert_message_eq(&expected, &err.message, &invalid);
    });
}

#[test]
fn test_valid_names_accepted() {
    for name in ["BlueLake", "RedStone", "GoldHawk"] {
        eprintln!("Testing agent name \"{name}\"...");
        assert!(
            detect_agent_name_mistake(name).is_none(),
            "valid name should not be classified as a mistake"
        );
    }
}
