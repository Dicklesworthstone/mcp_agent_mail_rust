//! End-to-end tests for per-transport lifecycle authorization (PR #310
//! follow-up, option (c)).
//!
//! These drive the REAL `register_agent` / `retire_agent` / `unretire_agent`
//! / `deregister_agent` tool functions against a real SQLite-backed pool and
//! assert the transport policy at the tool boundary:
//!
//! - HTTP (`call_transport = "http"`) + pane context but no token => refused
//!   with `AUTHENTICATION_REQUIRED`, `reason = token_required`, and the
//!   message says how to obtain the token;
//! - HTTP + the agent's registration token => allowed, for every lifecycle
//!   tool;
//! - stdio (argument absent) + the token => allowed (unchanged); the stdio
//!   pane-only path is covered by the decision-function unit tests in
//!   `identity.rs`, because it needs a live tmux pane binding;
//! - a supplied-but-wrong token is refused on both transports, and a malformed
//!   `call_transport` is a typed `INVALID_ARGUMENT` refusal.

#![recursion_limit = "256"]

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    deregister_agent, ensure_project, register_agent, retire_agent, unretire_agent,
};
use serde_json::Value;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` serially with a fresh temp DB/storage. Mirrors the harness used by
/// the other tool-level integration tests.
fn run_with_fresh_state<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx, String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp_root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
    let tempdir = tempfile::Builder::new()
        .prefix("mcp-agent-mail-lifecycle-auth-")
        .tempdir_in(temp_root)
        .expect("lifecycle auth temp directory");
    let database_url = format!("sqlite://{}", tempdir.path().join("mail.sqlite3").display());
    let storage_root = tempdir.path().join("storage");
    let project_key = tempdir.path().join("project");
    std::fs::create_dir_all(&project_key).expect("project dir");
    let storage_root = storage_root.display().to_string();
    let project_key = project_key.display().to_string();
    let env: Vec<(&str, &str)> = vec![
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
    ];
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx, project_key.clone()))
    })
}

fn error_payload(err: &fastmcp::McpError) -> Value {
    err.data
        .as_ref()
        .and_then(|root| root.get("error"))
        .cloned()
        .unwrap_or(Value::Null)
}

async fn register(ctx: &McpContext, project_key: &str, name: &str) -> String {
    let raw = register_agent(
        ctx,
        project_key.to_string(),
        "claude-code".to_string(),
        "opus-4.1".to_string(),
        Some(name.to_string()),
        Some("lifecycle auth transport test".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("register_agent");
    let value: Value = serde_json::from_str(&raw).expect("register_agent JSON");
    value["registration_token"]
        .as_str()
        .expect("registration_token returned on registration")
        .to_string()
}

const HTTP: Option<&str> = Some("http");

fn opt(value: Option<&str>) -> Option<String> {
    value.map(str::to_string)
}

#[test]
fn http_pane_context_without_token_is_refused_with_guidance() {
    run_with_fresh_state(|cx, project_key| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");
        let _token = register(&ctx, &project_key, "BlueLake").await;

        // The daemon-injected shape: pane + socket from the headers, no token.
        let err = retire_agent(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            None,
            Some("%23".to_string()),
            Some("/tmp/tmux-1000/ntm".to_string()),
            opt(HTTP),
        )
        .await
        .expect_err("HTTP retire without a token must be refused");
        let payload = error_payload(&err);
        assert_eq!(payload["type"], "AUTHENTICATION_REQUIRED");
        assert_eq!(payload["data"]["transport"], "http");
        assert_eq!(payload["data"]["policy"], "token_required");
        assert_eq!(payload["data"]["reason"], "token_required");
        assert_eq!(payload["data"]["token_param"], "registration_token");
        assert!(
            payload["data"]["token_source"]
                .as_str()
                .is_some_and(|s| s.contains("register_agent")),
            "{payload}"
        );
        assert!(
            err.message
                .contains("over HTTP requires the registration_token")
                && err.message.contains("register_agent"),
            "message must say a token is required over HTTP and how to obtain it: {}",
            err.message
        );

        // The same refusal without any pane context at all.
        let err = deregister_agent(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            None,
            None,
            None,
            opt(HTTP),
        )
        .await
        .expect_err("HTTP deregister without a token must be refused");
        assert_eq!(error_payload(&err)["data"]["reason"], "token_required");
    });
}

#[test]
fn http_with_valid_token_is_allowed_for_every_lifecycle_tool() {
    run_with_fresh_state(|cx, project_key| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");
        let token = register(&ctx, &project_key, "GreenCastle").await;

        let retired = retire_agent(
            &ctx,
            project_key.clone(),
            "GreenCastle".to_string(),
            Some(token.clone()),
            Some("%23".to_string()),
            Some("/tmp/tmux-1000/ntm".to_string()),
            opt(HTTP),
        )
        .await
        .expect("HTTP retire with the token");
        let retired: Value = serde_json::from_str(&retired).expect("retire JSON");
        assert_eq!(retired["status"], "retired");

        let active = unretire_agent(
            &ctx,
            project_key.clone(),
            "GreenCastle".to_string(),
            Some(token.clone()),
            None,
            None,
            opt(HTTP),
        )
        .await
        .expect("HTTP unretire with the token");
        let active: Value = serde_json::from_str(&active).expect("unretire JSON");
        assert_eq!(active["status"], "active");

        let gone = deregister_agent(
            &ctx,
            project_key.clone(),
            "GreenCastle".to_string(),
            Some(token),
            None,
            None,
            opt(HTTP),
        )
        .await
        .expect("HTTP deregister with the token");
        let gone: Value = serde_json::from_str(&gone).expect("deregister JSON");
        assert_eq!(gone["status"], "deregistered");
    });
}

#[test]
fn stdio_token_path_is_unchanged_and_wrong_tokens_stay_loud_everywhere() {
    run_with_fresh_state(|cx, project_key| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");
        let token = register(&ctx, &project_key, "RedStone").await;

        // stdio: `call_transport` absent (and blank) both select the stdio policy.
        for transport in [None, Some("  ")] {
            let err = retire_agent(
                &ctx,
                project_key.clone(),
                "RedStone".to_string(),
                Some("not-the-token".to_string()),
                Some("%23".to_string()),
                None,
                opt(transport),
            )
            .await
            .expect_err("a wrong token is refused over stdio");
            let payload = error_payload(&err);
            assert_eq!(payload["type"], "AUTHENTICATION_REQUIRED");
            assert_eq!(payload["data"]["transport"], "stdio");
            assert_eq!(payload["data"]["policy"], "token_or_bound_pane");
            assert_eq!(payload["data"]["reason"], "token_mismatch");
        }
        let err = retire_agent(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            Some("not-the-token".to_string()),
            Some("%23".to_string()),
            None,
            opt(HTTP),
        )
        .await
        .expect_err("a wrong token is refused over HTTP");
        assert_eq!(error_payload(&err)["data"]["reason"], "token_mismatch");

        // stdio without a token and without a bound pane: the pre-existing
        // refusal, now labelled.
        let err = retire_agent(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            None,
            Some("%2147483647".to_string()),
            None,
            None,
        )
        .await
        .expect_err("stdio without token or bound pane is refused");
        assert_eq!(error_payload(&err)["data"]["reason"], "no_bound_pane");

        // stdio with the token still works exactly as before.
        let retired = retire_agent(
            &ctx,
            project_key.clone(),
            "RedStone".to_string(),
            Some(token),
            None,
            None,
            None,
        )
        .await
        .expect("stdio retire with the token");
        let retired: Value = serde_json::from_str(&retired).expect("retire JSON");
        assert_eq!(retired["status"], "retired");
    });
}

#[test]
fn malformed_call_transport_is_a_typed_argument_refusal() {
    run_with_fresh_state(|cx, project_key| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");
        let token = register(&ctx, &project_key, "GoldHarbor").await;

        let err = unretire_agent(
            &ctx,
            project_key.clone(),
            "GoldHarbor".to_string(),
            Some(token),
            None,
            None,
            Some("sse".to_string()),
        )
        .await
        .expect_err("unknown call_transport must not be accepted");
        let payload = error_payload(&err);
        assert_eq!(payload["type"], "INVALID_ARGUMENT");
        assert_eq!(payload["data"]["field"], "call_transport");
        assert_eq!(
            payload["data"]["allowed"],
            serde_json::json!(["stdio", "http"])
        );
    });
}
