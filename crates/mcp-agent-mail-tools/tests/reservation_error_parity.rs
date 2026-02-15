use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_tools::{
    ensure_project, file_reservation_paths, force_release_file_reservation, register_agent,
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
            Some("reservation parity test".to_string()),
            None,
        )
        .await
        .expect("register_agent");
    }
}

#[test]
fn test_empty_paths_error() {
    run_serial_async(|cx| async move {
        let scenario = "empty_paths_error";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing reservation error: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agents(&ctx, &project_key, &["BlueLake"]).await;

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
            "{scenario}: error type mismatch"
        );
        assert_eq!(
            payload.get("recoverable").and_then(Value::as_bool),
            Some(true),
            "{scenario}: recoverable mismatch"
        );

        let expected_msg = "paths list cannot be empty. Provide at least one file path or glob pattern \
            to reserve (e.g., ['src/api/*.py', 'config/settings.yaml']).";
        assert_eq!(
            payload.get("message").and_then(Value::as_str),
            Some(expected_msg),
            "{scenario}: message mismatch"
        );

        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("EMPTY_PATHS should include data");
        assert_eq!(
            data.get("provided"),
            Some(&serde_json::json!([])),
            "{scenario}: data.provided should be empty array"
        );
    });
}

#[test]
fn test_reservation_conflict_response_structure() {
    run_serial_async(|cx| async move {
        let scenario = "conflict_response_structure";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing reservation error: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agents(&ctx, &project_key, &["BlueLake", "RedStone"]).await;

        // BlueLake reserves src/main.rs exclusively
        let grant_json = file_reservation_paths(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["src/main.rs".to_string()],
            Some(3600),
            Some(true),
            Some("test".to_string()),
        )
        .await
        .expect("initial reservation should succeed");

        let grant: Value = serde_json::from_str(&grant_json).expect("parse grant");
        let granted = grant
            .get("granted")
            .and_then(Value::as_array)
            .expect("granted array");
        assert_eq!(granted.len(), 1, "{scenario}: should grant 1 path");

        // RedStone tries to reserve the same path
        let conflict_json = file_reservation_paths(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            vec!["src/main.rs".to_string()],
            Some(3600),
            Some(true),
            Some("test".to_string()),
        )
        .await
        .expect("conflicting reservation should succeed (returns conflicts, not error)");

        let result: Value = serde_json::from_str(&conflict_json).expect("parse conflict result");

        // Verify top-level structure
        assert!(
            result.get("granted").is_some(),
            "{scenario}: response must have 'granted' key"
        );
        assert!(
            result.get("conflicts").is_some(),
            "{scenario}: response must have 'conflicts' key"
        );

        let conflicts = result
            .get("conflicts")
            .and_then(Value::as_array)
            .expect("conflicts array");
        assert_eq!(conflicts.len(), 1, "{scenario}: should have 1 conflict");

        let conflict = &conflicts[0];
        assert_eq!(
            conflict.get("path").and_then(Value::as_str),
            Some("src/main.rs"),
            "{scenario}: conflict.path mismatch"
        );

        // Verify holder structure matches Python: {agent, path_pattern, exclusive, expires_ts}
        let holders = conflict
            .get("holders")
            .and_then(Value::as_array)
            .expect("conflict.holders array");
        assert_eq!(holders.len(), 1, "{scenario}: should have 1 holder");

        let holder = &holders[0];
        let holder_obj = holder.as_object().expect("holder must be object");

        // Check required keys
        let holder_keys: std::collections::BTreeSet<String> = holder_obj.keys().cloned().collect();
        let expected_keys: std::collections::BTreeSet<String> =
            ["agent", "path_pattern", "exclusive", "expires_ts"]
                .into_iter()
                .map(str::to_string)
                .collect();
        assert_eq!(
            holder_keys, expected_keys,
            "{scenario}: holder keys mismatch; expected {expected_keys:?}, got {holder_keys:?}"
        );

        assert_eq!(
            holder.get("agent").and_then(Value::as_str),
            Some("BlueLake"),
            "{scenario}: holder.agent mismatch"
        );
        assert_eq!(
            holder.get("path_pattern").and_then(Value::as_str),
            Some("src/main.rs"),
            "{scenario}: holder.path_pattern mismatch"
        );
        assert_eq!(
            holder.get("exclusive").and_then(Value::as_bool),
            Some(true),
            "{scenario}: holder.exclusive mismatch"
        );
        assert!(
            holder.get("expires_ts").and_then(Value::as_str).is_some(),
            "{scenario}: holder.expires_ts should be a string"
        );
    });
}

#[test]
fn test_glob_pattern_conflict() {
    run_serial_async(|cx| async move {
        let scenario = "glob_pattern_conflict";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing reservation error: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agents(&ctx, &project_key, &["BlueLake", "RedStone"]).await;

        // BlueLake reserves src/** exclusively
        file_reservation_paths(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["src/**".to_string()],
            Some(3600),
            Some(true),
            Some("test".to_string()),
        )
        .await
        .expect("glob reservation should succeed");

        // RedStone tries to reserve src/main.rs
        let conflict_json = file_reservation_paths(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            vec!["src/main.rs".to_string()],
            Some(3600),
            Some(true),
            Some("test".to_string()),
        )
        .await
        .expect("overlapping reservation returns conflicts");

        let result: Value = serde_json::from_str(&conflict_json).expect("parse result");
        let conflicts = result
            .get("conflicts")
            .and_then(Value::as_array)
            .expect("conflicts array");
        assert_eq!(conflicts.len(), 1, "{scenario}: glob should conflict");

        let holder = &conflicts[0]
            .get("holders")
            .and_then(Value::as_array)
            .expect("holders")[0];
        assert_eq!(
            holder.get("agent").and_then(Value::as_str),
            Some("BlueLake"),
            "{scenario}: holder.agent should be the glob owner"
        );
        assert_eq!(
            holder.get("path_pattern").and_then(Value::as_str),
            Some("src/**"),
            "{scenario}: holder.path_pattern should show the glob"
        );
    });
}

#[test]
fn test_not_found_force_release() {
    run_serial_async(|cx| async move {
        let scenario = "not_found_force_release";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing reservation error: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agents(&ctx, &project_key, &["BlueLake"]).await;

        let err = force_release_file_reservation(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            99999,
            None,
            None,
        )
        .await
        .expect_err("nonexistent reservation should fail");

        let payload = error_object(&err);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("NOT_FOUND"),
            "{scenario}: error type mismatch"
        );
        assert_eq!(
            payload.get("recoverable").and_then(Value::as_bool),
            Some(true),
            "{scenario}: recoverable mismatch"
        );

        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("NOT_FOUND should include data");
        assert_eq!(
            data.get("file_reservation_id"),
            Some(&serde_json::json!(99999)),
            "{scenario}: data.file_reservation_id mismatch"
        );
    });
}

#[test]
fn test_granted_response_structure() {
    run_serial_async(|cx| async move {
        let scenario = "granted_response_structure";
        let project_key = format!("/tmp/{scenario}-{}", unique_suffix());
        eprintln!("Testing reservation error: scenario={scenario}...");

        let ctx = McpContext::new(cx.clone(), 1);
        setup_project_and_agents(&ctx, &project_key, &["BlueLake"]).await;

        let result_json = file_reservation_paths(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            vec!["src/main.rs".to_string(), "docs/readme.md".to_string()],
            Some(3600),
            Some(true),
            Some("test".to_string()),
        )
        .await
        .expect("reservation should succeed");

        let result: Value = serde_json::from_str(&result_json).expect("parse result");

        let granted = result
            .get("granted")
            .and_then(Value::as_array)
            .expect("granted array");
        assert_eq!(granted.len(), 2, "{scenario}: should grant 2 paths");

        // Check each granted entry has required fields
        for entry in granted {
            let obj = entry.as_object().expect("granted entry must be object");
            assert!(
                obj.contains_key("id"),
                "{scenario}: granted entry missing 'id'"
            );
            assert!(
                obj.contains_key("path_pattern"),
                "{scenario}: granted entry missing 'path_pattern'"
            );
            assert!(
                obj.contains_key("exclusive"),
                "{scenario}: granted entry missing 'exclusive'"
            );
            assert!(
                obj.contains_key("expires_ts"),
                "{scenario}: granted entry missing 'expires_ts'"
            );
        }

        let conflicts = result
            .get("conflicts")
            .and_then(Value::as_array)
            .expect("conflicts array");
        assert!(conflicts.is_empty(), "{scenario}: should have no conflicts");
    });
}
