//! Transcript-safe identity contract tests (GH#255).
//!
//! Covers the two well-bounded contract deltas reported against the Python
//! server:
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

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use fastmcp::{CallToolParams, Router};
use fastmcp_core::SessionState;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    CreateAgentIdentity, EnsureProject, RegisterAgent, RevokeAgentToken, RotateAgentToken,
    SendMessage, create_agent_identity, ensure_project, register_agent, reply_message,
    retire_agent, revoke_agent_token, rotate_agent_token, send_message, tool_error_code,
    tool_util::get_db_pool,
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
    run_isolated_with_env(&[], f)
}

fn run_isolated_with_env<F, Fut, T>(overrides: &[(&str, &str)], f: F) -> T
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
    let mut env: Vec<(&str, &str)> = vec![
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
    ];
    env.extend_from_slice(overrides);
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

#[test]
fn register_dispatch_can_suppress_token_on_creation_and_update() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx, 1);
        let project = "/tmp/transcript-safe-register";
        ensure_project(&ctx, project.to_string(), None)
            .await
            .unwrap();
        let router = fastmcp_server::ServerBuilder::new("register-echo-test", "0")
            .tool(RegisterAgent)
            .build()
            .into_router();
        let mut previous = None;
        for echo in [false, false, true] {
            let result = router
                .handle_tools_call(
                    &ctx,
                    CallToolParams {
                        name: "register_agent".to_string(),
                        arguments: Some(json!({
                            "project_key": project, "name": "BlueLake",
                            "program": "codex-cli", "model": "gpt-5",
                            "return_registration_token": echo,
                        })),
                        meta: None,
                    },
                    SessionState::new(),
                    None,
                    None,
                )
                .await
                .expect("register dispatch");
            let text = tool_result_text(&result.content);
            let payload = parse(&text);
            let pool = get_db_pool().unwrap();
            let stored = mcp_agent_mail_db::queries::get_agent_by_id_fresh(
                ctx.cx(),
                &pool,
                payload["id"].as_i64().unwrap(),
            )
            .await
            .into_result()
            .unwrap();
            let token = stored.registration_token.expect("credential persists");
            assert!(!token.is_empty());
            assert_ne!(
                previous.as_ref(),
                Some(&token),
                "registration still rotates"
            );
            if echo {
                assert_eq!(payload["registration_token"].as_str(), Some(token.as_str()));
            } else {
                assert!(payload.get("registration_token").is_none());
                assert!(!text.contains(&token));
                assert_eq!(payload["registration_token_returned"], false);
            }
            previous = Some(token);
        }
    });
}

async fn register_credential_fixture(ctx: &McpContext, project: &str, name: &str) -> Value {
    parse(
        &register_agent(
            ctx,
            project.to_string(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some(name.to_string()),
            Some("preserve credential fixture metadata".to_string()),
            None,
            Some(true),
            None,
            None,
            None,
            Some(true),
        )
        .await
        .unwrap(),
    )
}

async fn credential_test_send(
    ctx: &McpContext,
    project: &str,
    token: Option<&str>,
    key: Option<&str>,
) -> Result<Value, String> {
    send_message(
        ctx,
        project.to_string(),
        "BlueLake".to_string(),
        vec!["GreenCastle".to_string()],
        "Credential lifecycle".to_string(),
        "Bearer proof must be current.".to_string(),
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
        key.map(str::to_string),
    )
    .await
    .map(|response| parse(&response))
    .map_err(|error| tool_error_code(&error).unwrap_or("UNKNOWN").to_string())
}

#[test]
#[allow(clippy::too_many_lines)]
fn bearer_rotation_and_revocation_reject_stale_cache_without_revoking_sessions() {
    run_isolated_with_env(
        &[
            ("MESSAGING_SESSION_IDENTITY", "true"),
            ("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "0"),
            ("CONTACT_ENFORCEMENT_ENABLED", "0"),
            ("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1"),
        ],
        |cx| async move {
            let owner = McpContext::with_state(cx.clone(), 1, SessionState::new());
            let plain = McpContext::new(cx, 2);
            let project = "/tmp/bearer-lifecycle";
            ensure_project(&plain, project.to_string(), None)
                .await
                .unwrap();
            let registered = register_credential_fixture(&owner, project, "BlueLake").await;
            let recipient = register_credential_fixture(&plain, project, "GreenCastle").await;
            let original_token = registered["registration_token"].as_str().unwrap();
            let pool = get_db_pool().unwrap();
            let original = mcp_agent_mail_db::queries::get_agent_by_id_fresh(
                owner.cx(),
                &pool,
                registered["id"].as_i64().unwrap(),
            )
            .await
            .into_result()
            .unwrap();

            // Rotation with no echo is useful without changing any public profile.
            let hidden = rotate_agent_token(
                &owner,
                project.to_string(),
                "BlueLake".to_string(),
                original_token.to_string(),
                None,
            )
            .await
            .unwrap();
            let hidden_payload = parse(&hidden);
            assert_eq!(hidden_payload["token_status"], "rotated");
            assert_eq!(hidden_payload["registration_token_returned"], false);
            assert!(hidden_payload.get("registration_token").is_none());
            let rotated = mcp_agent_mail_db::queries::get_agent_by_id_fresh(
                owner.cx(),
                &pool,
                original.id.unwrap(),
            )
            .await
            .into_result()
            .unwrap();
            let rotated_token = rotated.registration_token.as_deref().unwrap();
            assert_ne!(rotated_token, original_token);
            assert!(!hidden.contains(rotated_token));
            let mut expected = original.clone();
            expected.registration_token = rotated.registration_token.clone();
            assert_eq!(
                serde_json::to_value(&rotated).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );

            // Reinstalling an old cache entry must not restore bearer authority.
            mcp_agent_mail_db::cache::read_cache()
                .put_agent_scoped(&pool.sqlite_identity_key(), &original);
            assert_eq!(
                credential_test_send(&owner, project, Some(original_token), None).await,
                Err("SENDER_TOKEN_MISMATCH".to_string())
            );
            let explicit = parse(
                &rotate_agent_token(
                    &owner,
                    project.to_string(),
                    "BlueLake".to_string(),
                    rotated_token.to_string(),
                    Some(true),
                )
                .await
                .unwrap(),
            );
            assert_eq!(explicit["registration_token_returned"], true);
            let active_token = explicit["registration_token"].as_str().unwrap();
            assert_ne!(active_token, rotated_token);
            let sent = credential_test_send(
                &owner,
                project,
                Some(active_token),
                Some("credential-replay"),
            )
            .await
            .unwrap();
            assert_eq!(sent["verified_sender"], true);
            let message_id = sent["deliveries"][0]["payload"]["id"].as_i64().unwrap();
            let before_revoke = mcp_agent_mail_db::queries::get_agent_by_id_fresh(
                owner.cx(),
                &pool,
                original.id.unwrap(),
            )
            .await
            .into_result()
            .unwrap();

            let revoked_text = revoke_agent_token(
                &owner,
                project.to_string(),
                "BlueLake".to_string(),
                active_token.to_string(),
            )
            .await
            .unwrap();
            let revoked = parse(&revoked_text);
            assert_eq!(revoked["token_status"], "revoked");
            assert_eq!(revoked["registration_token_returned"], false);
            assert!(revoked.get("registration_token").is_none());
            assert!(!revoked_text.contains(active_token));
            mcp_agent_mail_db::cache::read_cache()
                .put_agent_scoped(&pool.sqlite_identity_key(), &before_revoke);

            for key in [None, Some("credential-replay")] {
                assert_eq!(
                    credential_test_send(&owner, project, Some(active_token), key).await,
                    Err("SENDER_TOKEN_UNAVAILABLE".to_string())
                );
            }
            let reply_error = reply_message(
                &owner,
                project.to_string(),
                message_id,
                "BlueLake".to_string(),
                "Revoked bearer must not reply.".to_string(),
                Some(vec!["GreenCastle".to_string()]),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(active_token.to_string()),
                None,
            )
            .await
            .expect_err("revoked bearer cannot reply despite session ownership");
            assert_eq!(
                tool_error_code(&reply_error),
                Some("SENDER_TOKEN_UNAVAILABLE")
            );
            let retire_error = retire_agent(
                &owner,
                project.to_string(),
                "BlueLake".to_string(),
                Some(active_token.to_string()),
                None,
                None,
                Some("http".to_string()),
            )
            .await
            .expect_err("revoked bearer cannot retire through a stale cache");
            assert_eq!(
                tool_error_code(&retire_error),
                Some("AUTHENTICATION_REQUIRED")
            );
            let rotate_error = rotate_agent_token(
                &owner,
                project.to_string(),
                "BlueLake".to_string(),
                active_token.to_string(),
                Some(true),
            )
            .await
            .expect_err("revoked bearer cannot mint another token");
            assert_eq!(
                tool_error_code(&rotate_error),
                Some("AUTHENTICATION_REQUIRED")
            );
            assert!(!format!("{rotate_error:?}").contains(active_token));
            let inbox = mcp_agent_mail_db::queries::fetch_inbox(
                owner.cx(),
                &pool,
                original.project_id,
                recipient["id"].as_i64().unwrap(),
                false,
                None,
                50,
            )
            .await
            .into_result()
            .unwrap();
            assert_eq!(inbox.len(), 1, "refused sends and replies must not persist");

            let session_send = credential_test_send(&owner, project, None, None)
                .await
                .unwrap();
            assert_eq!(session_send["verified_sender"], true);
            assert_eq!(session_send["sender_verification"], "session");
            let trusted_local = credential_test_send(&plain, project, None, None)
                .await
                .unwrap();
            assert_eq!(trusted_local["verified_sender"], false);
            let durable = mcp_agent_mail_db::queries::get_agent_by_id_fresh(
                owner.cx(),
                &pool,
                original.id.unwrap(),
            )
            .await
            .into_result()
            .unwrap();
            assert!(durable.registration_token.is_none());
            assert!(durable.retired_at.is_none());
        },
    );
}

#[test]
fn credential_tools_dispatch_require_current_token_and_hide_replacements() {
    run_isolated(|cx| async move {
        let ctx = McpContext::new(cx, 1);
        let project = "/tmp/credential-tool-dispatch";
        ensure_project(&ctx, project.to_string(), None)
            .await
            .unwrap();
        let registered = register_credential_fixture(&ctx, project, "BlueLake").await;
        let token = registered["registration_token"].as_str().unwrap();
        let router = fastmcp_server::ServerBuilder::new("credential-tool-test", "0")
            .tool(RotateAgentToken)
            .tool(RevokeAgentToken)
            .build()
            .into_router();
        let result = router.handle_tools_call(
            &ctx,
            CallToolParams { name: "rotate_agent_token".to_string(), arguments: Some(json!({
                "project_key": project, "agent_name": "BlueLake", "registration_token": token,
            })), meta: None }, SessionState::new(), None, None,
        ).await.unwrap();
        let text = tool_result_text(&result.content);
        let result = parse(&text);
        assert_eq!(result["token_status"], "rotated");
        assert!(result.get("registration_token").is_none());
        assert!(!text.contains(token));
        let pool = get_db_pool().unwrap();
        let fresh = mcp_agent_mail_db::queries::get_agent_by_id_fresh(
            ctx.cx(),
            &pool,
            registered["id"].as_i64().unwrap(),
        )
        .await
        .into_result()
        .unwrap();
        let replacement = fresh.registration_token.unwrap();
        assert!(!text.contains(&replacement));
        for invalid in ["", "wrong-token", token] {
            let error = revoke_agent_token(
                &ctx,
                project.to_string(),
                "BlueLake".to_string(),
                invalid.to_string(),
            )
            .await
            .expect_err("only the current credential authorizes");
            assert_eq!(tool_error_code(&error), Some("AUTHENTICATION_REQUIRED"));
        }
        let result = router.handle_tools_call(
            &ctx,
            CallToolParams { name: "revoke_agent_token".to_string(), arguments: Some(json!({
                "project_key": project, "agent_name": "BlueLake", "registration_token": replacement,
            })), meta: None }, SessionState::new(), None, None,
        ).await.unwrap();
        let result = parse(&tool_result_text(&result.content));
        assert_eq!(result["token_status"], "revoked");
        assert!(result.get("registration_token").is_none());
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
