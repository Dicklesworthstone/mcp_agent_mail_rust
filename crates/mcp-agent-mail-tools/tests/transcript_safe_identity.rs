//! Transcript-safe identity contract tests (GH#255, GH#396).
//!
//! Covers registration output hygiene and retained Python messaging contracts:
//!
//! 1. `create_agent_identity(return_registration_token=false)` must succeed,
//!    omit the registration token from the tool result, mark the omission with
//!    `registration_token_returned: false`, and still persist the token
//!    server-side (the identity remains fully functional).
//! 2. `send_message` with `auto_contact_if_blocked` *omitted* or set to an
//!    explicit JSON `null` takes the server-default path (Python-parity).
//!    fastmcp 0.7.1 emits nullable `["<T>", "null"]` schemas for `Option<T>`
//!    params and treats explicit `null` as omitted at extraction, so both
//!    spellings are equivalent end-to-end — completing the parity this suite
//!    originally pinned as a loud rejection.
//! 3. Reusable registration and session macros omit tokens by default, while
//!    explicit opt-in still returns the persisted credential. Hidden rotation
//!    invalidates the old credential without exposing the replacement.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use fastmcp::{CallToolParams, Router};
use fastmcp_core::SessionState;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_db::queries;
use mcp_agent_mail_tools::{
    CreateAgentIdentity, EnsureProject, MacroPrepareThread, MacroStartSession, RegisterAgent,
    SendMessage, create_agent_identity, ensure_project, list_agents, register_agent, send_message,
    tool_util::get_db_pool, whois,
};
use serde_json::{Value, json};
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

/// Run `f` serially with a fresh temp DB/storage. Mirrors the harness used by
/// the other parity integration tests in this directory.
fn run_isolated<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let suffix = unique_suffix();
    let database_url = format!("sqlite:///tmp/transcript-safe-{suffix}.sqlite3");
    let storage_root = format!("/tmp/transcript-safe-storage-{suffix}");
    let env: Vec<(&str, &str)> = vec![
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
        ("MESSAGING_SESSION_IDENTITY", "false"),
        ("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "0"),
        ("AM_REGISTRATION_PROOF_GATE_ENABLED", "false"),
    ];
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx))
    })
}

fn parse(json_text: &str) -> Value {
    serde_json::from_str(json_text).expect("tool responses are valid JSON")
}

async fn dispatch_tool(router: &Router, cx: &Cx, name: &str, arguments: Value) -> Value {
    let ctx = McpContext::new(cx.clone(), 1);
    let result = router
        .handle_tools_call(
            &ctx,
            CallToolParams {
                name: name.to_string(),
                arguments: Some(arguments),
                meta: None,
            },
            SessionState::new(),
            None,
            None,
        )
        .await
        .expect("tool dispatch");
    parse(&tool_result_text(&result.content))
}

fn registration_arguments(project_key: &str, name: &str) -> Value {
    json!({
        "project_key": project_key,
        "program": "codex-cli",
        "model": "gpt-5",
        "name": name,
    })
}

async fn stored_registration_token(ctx: &McpContext, project_key: &str, name: &str) -> String {
    let pool = get_db_pool().expect("db pool");
    let project = queries::get_project_by_human_key(ctx.cx(), &pool, project_key)
        .await
        .into_result()
        .expect("project exists");
    queries::get_agent(ctx.cx(), &pool, project.id.expect("project id"), name)
    .await
    .into_result()
    .expect("registered agent exists")
    .registration_token
    .filter(|token| !token.is_empty())
    .expect("registration persists a nonempty token")
}

async fn send_with_token(
    ctx: &McpContext,
    project: &str,
    token: Option<&str>,
) -> Result<String, fastmcp::McpError> {
    send_message(
        ctx,
        project.to_string(),
        "BlueLake".to_string(),
        vec!["GreenCastle".to_string()],
        "Registration token verification".to_string(),
        "Only the current persisted token may verify this sender.".to_string(),
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
        token.map(str::to_string),
        None,
    )
    .await
}

#[test]
#[allow(clippy::too_many_lines)]
fn register_dispatch_returns_tokens_only_on_explicit_opt_in() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/transcript-register-{}", unique_suffix());
        let project = parse(
            &ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure project"),
        );
        let router: Router = fastmcp_server::ServerBuilder::new("register-token-test", "0")
            .tool(RegisterAgent)
            .build()
            .into_router();

        for (name, setting) in [
            ("BlueLake", None),
            ("GreenCastle", Some(Value::Bool(false))),
            ("RedStone", Some(Value::Bool(true))),
            ("SilverLake", Some(Value::Null)),
        ] {
            let expect_token = setting == Some(Value::Bool(true));
            let mut arguments = registration_arguments(&project_key, name);
            if let Some(setting) = setting {
                arguments["return_registration_token"] = setting;
            }
            let registered = dispatch_tool(&router, &cx, "register_agent", arguments).await;
            assert_eq!(registered["name"], name);
            let token = stored_registration_token(&ctx, &project_key, name).await;
            if expect_token {
                assert_eq!(registered["registration_token"].as_str(), Some(token.as_str()));
            } else {
                assert!(registered.get("registration_token").is_none());
                assert!(!registered.to_string().contains(&token));
            }

            let profile = whois(
                &ctx,
                project_key.clone(),
                name.to_string(),
                Some(false),
                None,
            )
            .await
            .expect("whois");
            assert!(parse(&profile).get("registration_token").is_none());
            assert!(!profile.contains(&token));
            let roster = list_agents(&ctx, project_key.clone(), None, None)
                .await
                .expect("list agents");
            assert!(!roster.contains("registration_token"));
            assert!(!roster.contains(&token));

            mcp_agent_mail_storage::wbq_flush();
            let profile_path = Config::get()
                .storage_root
                .join("projects")
                .join(project["slug"].as_str().expect("project slug"))
                .join("agents")
                .join(name)
                .join("profile.json");
            let archived = std::fs::read_to_string(profile_path).expect("archived agent profile");
            assert!(parse(&archived).get("registration_token").is_none());
            assert!(!archived.contains(&token));
        }
    });
}

#[test]
fn hidden_reregistration_rotates_the_persisted_token_and_refreshes_sender_cache() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/transcript-reregister-{}", unique_suffix());
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure project");
        let router: Router = fastmcp_server::ServerBuilder::new("reregister-token-test", "0")
            .tool(RegisterAgent)
            .build()
            .into_router();
        let mut arguments = registration_arguments(&project_key, "BlueLake");
        arguments["return_registration_token"] = Value::Bool(true);
        let first = dispatch_tool(&router, &cx, "register_agent", arguments).await;
        let old_token = first["registration_token"].as_str().expect("explicit token");
        dispatch_tool(
            &router,
            &cx,
            "register_agent",
            registration_arguments(&project_key, "GreenCastle"),
        )
        .await;
        mcp_agent_mail_tools::set_contact_policy(
            &ctx,
            project_key.clone(),
            "GreenCastle".to_string(),
            "open".to_string(),
        )
        .await
        .expect("open recipient contacts");
        let before = send_with_token(&ctx, &project_key, Some(old_token))
            .await
            .expect("initial token verifies and primes the sender cache");
        assert_eq!(parse(&before)["verified_sender"], true);

        let refreshed = dispatch_tool(
            &router,
            &cx,
            "register_agent",
            registration_arguments(&project_key, "BlueLake"),
        )
        .await;
        assert_eq!(refreshed["id"], first["id"]);
        assert!(refreshed.get("registration_token").is_none());
        let replacement = stored_registration_token(&ctx, &project_key, "BlueLake").await;
        assert_ne!(replacement, old_token);
        assert!(!refreshed.to_string().contains(&replacement));
        let error = send_with_token(&ctx, &project_key, Some(old_token))
            .await
            .expect_err("the previous credential must stop authenticating");
        assert_eq!(
            mcp_agent_mail_tools::tool_error_code(&error),
            Some("SENDER_TOKEN_MISMATCH")
        );
        let after = send_with_token(&ctx, &project_key, Some(&replacement))
            .await
            .expect("the hidden persisted replacement verifies");
        assert_eq!(parse(&after)["verified_sender"], true);
    });
}

#[test]
fn revoked_database_token_is_rejected_after_cache_reload_in_trusted_local_mode() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/transcript-revoked-token-{}", unique_suffix());
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure project");
        let router: Router = fastmcp_server::ServerBuilder::new("revoked-token-test", "0")
            .tool(RegisterAgent)
            .build()
            .into_router();
        let sender = dispatch_tool(
            &router,
            &cx,
            "register_agent",
            registration_arguments(&project_key, "BlueLake"),
        )
        .await;
        dispatch_tool(
            &router,
            &cx,
            "register_agent",
            registration_arguments(&project_key, "GreenCastle"),
        )
        .await;
        mcp_agent_mail_tools::set_contact_policy(
            &ctx,
            project_key.clone(),
            "GreenCastle".to_string(),
            "open".to_string(),
        )
        .await
        .expect("open recipient contacts");
        let token = stored_registration_token(&ctx, &project_key, "BlueLake").await;
        let verified = send_with_token(&ctx, &project_key, Some(&token))
            .await
            .expect("original token verifies and primes the cache");
        assert_eq!(parse(&verified)["verified_sender"], true);

        let pool = get_db_pool().expect("db pool");
        let agent_id = sender["id"].as_i64().expect("sender id");
        let project_id = sender["project_id"].as_i64().expect("project id");
        queries::update_agent_registration_token(ctx.cx(), &pool, agent_id, "")
        .await
        .into_result()
        .expect("revoke persisted credential");
        mcp_agent_mail_db::read_cache().invalidate_agent_scoped(
            &pool.sqlite_identity_key(),
            project_id,
            "BlueLake",
            Some(agent_id),
        );
        let reloaded = queries::get_agent(ctx.cx(), &pool, project_id, "BlueLake")
        .await
        .into_result()
        .expect("reload revoked identity from the database");
        assert!(reloaded.registration_token.is_none());
        drop(pool);

        let error = send_with_token(&ctx, &project_key, Some(&token))
            .await
            .expect_err("a revoked credential must not become an unverified success");
        assert_eq!(
            mcp_agent_mail_tools::tool_error_code(&error),
            Some("SENDER_TOKEN_UNAVAILABLE")
        );
        let trusted_local = send_with_token(&ctx, &project_key, None)
            .await
            .expect("omitted-token trusted-local sending remains available");
        assert_eq!(parse(&trusted_local)["verified_sender"], false);
    });
}

#[test]
fn session_macro_returns_tokens_only_on_explicit_opt_in() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/transcript-session-{}", unique_suffix());
        let router: Router = fastmcp_server::ServerBuilder::new("session-token-test", "0")
            .tool(MacroStartSession)
            .tool(MacroPrepareThread)
            .build()
            .into_router();
        for (name, setting) in [
            ("BlueLake", None),
            ("GreenCastle", Some(false)),
            ("RedStone", Some(true)),
        ] {
            let mut arguments = json!({
                "human_key": &project_key,
                "program": "codex-cli",
                "model": "gpt-5",
                "agent_name": name,
                "inbox_limit": 1,
            });
            if let Some(setting) = setting {
                arguments["return_registration_token"] = Value::Bool(setting);
            }
            let session = dispatch_tool(&router, &cx, "macro_start_session", arguments).await;
            assert_eq!(session["agent"]["name"], name);
            let token = stored_registration_token(&ctx, &project_key, name).await;
            if setting == Some(true) {
                assert_eq!(session["agent"]["registration_token"].as_str(), Some(token.as_str()));
            } else {
                assert!(session["agent"].get("registration_token").is_none());
                assert!(!session.to_string().contains(&token));
            }
        }

        let prepared = dispatch_tool(
            &router,
            &cx,
            "macro_prepare_thread",
            json!({
                "project_key": &project_key,
                "thread_id": "transcript-safe-thread",
                "program": "codex-cli",
                "model": "gpt-5",
                "agent_name": "SilverLake",
                "llm_mode": false,
                "inbox_limit": 1,
            }),
        )
        .await;
        assert_eq!(prepared["agent"]["name"], "SilverLake");
        assert!(prepared["agent"]["registration_token"].is_null());
        let token = stored_registration_token(&ctx, &project_key, "SilverLake").await;
        assert!(!prepared.to_string().contains(&token));
    });
}

#[test]
fn thread_macro_returns_only_new_tokens_on_explicit_opt_in() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/transcript-thread-token-{}", unique_suffix());
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure project");
        let router: Router = fastmcp_server::ServerBuilder::new("thread-token-test", "0")
            .tool(MacroPrepareThread)
            .build()
            .into_router();
        let arguments = json!({
            "project_key": &project_key,
            "thread_id": "transcript-safe-thread",
            "program": "codex-cli",
            "model": "gpt-5",
            "agent_name": "AmberRiver",
            "llm_mode": false,
            "inbox_limit": 1,
            "return_registration_token": true,
        });
        let created = dispatch_tool(&router, &cx, "macro_prepare_thread", arguments.clone()).await;
        assert_eq!(created["agent"]["name"], "AmberRiver");
        let token = created["agent"]["registration_token"]
            .as_str()
            .expect("explicit opt-in returns a new credential");
        assert_eq!(stored_registration_token(&ctx, &project_key, "AmberRiver").await, token);

        let prepared = dispatch_tool(&router, &cx, "macro_prepare_thread", arguments).await;
        assert_eq!(prepared["agent"]["id"], created["agent"]["id"]);
        assert!(prepared["agent"]["registration_token"].is_null());
        assert!(!prepared.to_string().contains(token));
        assert_eq!(stored_registration_token(&ctx, &project_key, "AmberRiver").await, token);
    });
}

/// Default behavior is unchanged: the token is echoed and no opt-out marker
/// appears in the response.
#[test]
fn token_echoed_by_default() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx, 1);
        let project_key = "/tmp/transcript-safe-default".to_string();
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let created = parse(
            &create_agent_identity(
                &ctx,
                project_key,
                "claude-code".to_string(),
                "opus-4.5".to_string(),
                None,
                Some("transcript-safety default".to_string()),
                None,
                None,
                None,
                None,
                None, // return_registration_token omitted => default true
            )
            .await
            .expect("create_agent_identity"),
        );

        let token = created["registration_token"]
            .as_str()
            .expect("default response must include registration_token");
        assert!(!token.is_empty(), "echoed token must be non-empty");
        assert!(
            created.get("registration_token_returned").is_none(),
            "opt-out marker must not appear on the default path"
        );
    });
}

/// `return_registration_token=false` omits the token, marks the omission, and
/// still persists the token server-side.
#[test]
fn token_omitted_on_opt_out_but_persisted() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx, 1);
        let project_key = "/tmp/transcript-safe-optout".to_string();
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let created = parse(
            &create_agent_identity(
                &ctx,
                project_key.clone(),
                "codex-cli".to_string(),
                "gpt-5".to_string(),
                None,
                Some("transcript-safety opt-out".to_string()),
                None,
                None,
                None,
                None,
                Some(false),
            )
            .await
            .expect("create_agent_identity with return_registration_token=false"),
        );

        assert!(
            created.get("registration_token").is_none(),
            "opt-out response must not contain a registration token, got: {created}"
        );
        assert_eq!(
            created["registration_token_returned"],
            Value::Bool(false),
            "opt-out response must carry registration_token_returned=false"
        );

        // The token must still exist server-side: the identity is functional,
        // the caller has merely opted out of the transcript echo.
        let name = created["name"].as_str().expect("agent name").to_string();
        let pool = get_db_pool().expect("db pool");
        let project =
            mcp_agent_mail_db::queries::get_project_by_human_key(ctx.cx(), &pool, &project_key)
                .await
                .into_result()
                .expect("project exists");
        let agent = mcp_agent_mail_db::queries::get_agent(
            ctx.cx(),
            &pool,
            project.id.expect("project id"),
            &name,
        )
        .await
        .into_result()
        .expect("agent exists");
        let stored = agent.registration_token.unwrap_or_default();
        assert!(
            !stored.is_empty(),
            "registration token must be persisted even when not echoed"
        );
    });
}

/// The dispatch layer must accept `return_registration_token: false` as JSON
/// and produce the transcript-safe response shape end-to-end.
#[test]
fn dispatch_accepts_return_registration_token_false() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = "/tmp/transcript-safe-dispatch".to_string();
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let router: Router = fastmcp_server::ServerBuilder::new("transcript-safe-test", "0")
            .tool(CreateAgentIdentity)
            .build()
            .into_router();
        let params = CallToolParams {
            name: "create_agent_identity".to_string(),
            arguments: Some(json!({
                "project_key": project_key,
                "program": "codex-cli",
                "model": "gpt-5",
                "return_registration_token": false,
            })),
            meta: None,
        };
        let request_ctx = McpContext::new(cx, 2);
        let result = router
            .handle_tools_call(&request_ctx, params, SessionState::new(), None, None)
            .await;
        let call_result = result.expect("create_agent_identity dispatch must not error");
        let text = tool_result_text(&call_result.content);
        let payload = parse(&text);
        assert!(
            payload.get("registration_token").is_none(),
            "dispatch opt-out response must not contain the token: {payload}"
        );
        assert_eq!(payload["registration_token_returned"], Value::Bool(false));
    });
}

/// Omitted `auto_contact_if_blocked` and an explicit JSON `null` both succeed
/// through the same server-default path (Python parity, fastmcp >= 0.7.1
/// nullable `Option<T>` schemas — see module docs).
#[test]
#[allow(clippy::too_many_lines)]
fn send_message_null_auto_contact_contract() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = "/tmp/transcript-safe-null-send".to_string();
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");
        for name in ["GreenCastle", "BlueLake"] {
            register_agent(
                &ctx,
                project_key.clone(),
                "claude-code".to_string(),
                "opus-4.5".to_string(),
                Some(name.to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("register_agent");
        }

        let router: Router = fastmcp_server::ServerBuilder::new("transcript-safe-test", "0")
            .tool(SendMessage)
            .tool(EnsureProject)
            .build()
            .into_router();
        let params = CallToolParams {
            name: "send_message".to_string(),
            arguments: Some(json!({
                "project_key": project_key.clone(),
                "sender_name": "GreenCastle",
                "to": ["BlueLake"],
                "subject": "null auto_contact_if_blocked",
                "body_md": "explicit null must behave like an omitted field",
                "auto_contact_if_blocked": null,
            })),
            meta: None,
        };
        let request_ctx = McpContext::new(cx.clone(), 2);
        let call_result = router
            .handle_tools_call(&request_ctx, params, SessionState::new(), None, None)
            .await
            .expect(
                "explicit null auto_contact_if_blocked must be accepted: fastmcp \
                 0.7.1 publishes nullable Option<T> schemas and treats null as \
                 omitted (Python parity)",
            );
        let text = tool_result_text(&call_result.content);
        let payload = parse(&text);
        assert_eq!(
            payload["count"].as_i64(),
            Some(1),
            "explicit null must take the server-default path and deliver: {payload}"
        );

        // Omitting the field entirely is the supported Python-parity spelling
        // and must succeed through the same dispatch path.
        let params_omitted = CallToolParams {
            name: "send_message".to_string(),
            arguments: Some(json!({
                "project_key": project_key.clone(),
                "sender_name": "GreenCastle",
                "to": ["BlueLake"],
                "subject": "omitted auto_contact_if_blocked",
                "body_md": "omitted field must take the server-default path",
            })),
            meta: None,
        };
        let request_ctx2 = McpContext::new(cx.clone(), 3);
        let call_result = router
            .handle_tools_call(
                &request_ctx2,
                params_omitted,
                SessionState::new(),
                None,
                None,
            )
            .await
            .expect("send_message with omitted auto_contact_if_blocked must succeed");
        let text = tool_result_text(&call_result.content);
        let payload = parse(&text);
        assert_eq!(
            payload["count"].as_i64(),
            Some(1),
            "send must deliver to the one recipient: {payload}"
        );

        // Direct-call equivalence: None (the deserialized form of null/omitted)
        // takes the server-default path.
        let direct = send_message(
            &ctx,
            "/tmp/transcript-safe-null-send".to_string(),
            "GreenCastle".to_string(),
            vec!["BlueLake".to_string()],
            "direct none".to_string(),
            "body".to_string(),
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
            None,
        )
        .await
        .expect("direct send with None auto_contact_if_blocked");
        let direct_payload = parse(&direct);
        assert_eq!(direct_payload["count"].as_i64(), Some(1));
    });
}

/// Extract the concatenated text content from a `tools/call` result.
fn tool_result_text(content: &[fastmcp::legacy_2024::LegacyContent]) -> String {
    use fastmcp::legacy_2024::LegacyContent;
    content
        .iter()
        .filter_map(|item| match item {
            LegacyContent::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}
