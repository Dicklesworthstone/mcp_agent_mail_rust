//! GH#279: opt-in MCP session-bound agent identity, through the real tool
//! entry points.
//!
//! With `MESSAGING_SESSION_IDENTITY` on, a session that created an identity
//! acts as it without a registration token in its transcript (also under the
//! fail-closed send profile), and cannot act as another agent of the project
//! by naming it. Two sessions are modeled as two contexts with independent
//! `SessionState`s, exactly as the stdio server and the HTTP session registry
//! supply them.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use fastmcp_core::SessionState;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    create_agent_identity, ensure_project, fetch_inbox, register_agent, send_message,
    tool_error_code,
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
    u64::try_from(micros)
        .unwrap_or(u64::MAX)
        .wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn run_with_session_identity<F, Fut, T>(fail_closed: bool, f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let suffix = unique_suffix();
    let db_path = format!("/tmp/session-identity-{suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/session-identity-storage-{suffix}");
    let env = [
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
        ("MESSAGING_SESSION_IDENTITY", "true"),
        (
            "MESSAGING_FAIL_CLOSED_SEND_PROFILE",
            if fail_closed { "1" } else { "0" },
        ),
    ];
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let out = rt.block_on(async {
            let cx = Cx::current().expect("runtime installs the tool test context");
            f(cx).await
        });
        Config::reset_cached();
        out
    })
}

async fn create_identity(ctx: &McpContext, project: &str) -> String {
    let created: Value = serde_json::from_str(
        &create_agent_identity(
            ctx,
            project.to_string(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(false),
        )
        .await
        .expect("create identity"),
    )
    .expect("identity JSON");
    assert!(created.get("registration_token").is_none());
    let name = created["name"].as_str().expect("agent name").to_string();
    mcp_agent_mail_tools::contacts::set_contact_policy(
        ctx,
        project.to_string(),
        name.clone(),
        "open".to_string(),
    )
    .await
    .expect("open contact policy");
    name
}

async fn send_as(ctx: &McpContext, project: &str, sender: &str, to: &str) -> Result<Value, String> {
    send_message(
        ctx,
        project.to_string(),
        sender.to_string(),
        vec![to.to_string()],
        "Session-bound hello".to_string(),
        "No token in this transcript.".to_string(),
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
    .map(|json| serde_json::from_str(&json).expect("send JSON"))
    .map_err(|error| tool_error_code(&error).unwrap_or("UNKNOWN").to_string())
}

async fn inbox(ctx: &McpContext, project: &str, agent: &str) -> Result<Vec<Value>, String> {
    fetch_inbox(
        ctx,
        project.to_string(),
        agent.to_string(),
        None,
        None,
        Some(50),
        None,
        None,
        None,
        None,
        Some(false),
    )
    .await
    .map(|json| serde_json::from_str(&json).expect("inbox JSON"))
    .map_err(|error| tool_error_code(&error).unwrap_or("UNKNOWN").to_string())
}

async fn stored_token(cx: &Cx, project: &str, agent: &str) -> Option<String> {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("db pool");
    let project = mcp_agent_mail_db::queries::get_project_by_human_key(cx, &pool, project)
        .await
        .into_result()
        .expect("project");
    mcp_agent_mail_db::queries::get_agent(cx, &pool, project.id.expect("project id"), agent)
        .await
        .into_result()
        .expect("agent")
        .registration_token
}

#[test]
fn session_bound_identity_sends_without_a_token_and_cannot_be_borrowed() {
    run_with_session_identity(true, |cx| async move {
        let session_a = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let session_b = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let stateless = McpContext::with_state(cx.clone(), 3, SessionState::new());
        let project = format!("/tmp/session-identity-{}", unique_suffix());
        ensure_project(&session_a, project.clone(), None)
            .await
            .expect("ensure project");
        let alice = create_identity(&session_a, &project).await;
        let bob = create_identity(&session_b, &project).await;

        // The fail-closed profile accepts the session binding as sender proof.
        let sent = send_as(&session_a, &project, &alice, &bob)
            .await
            .expect("session-bound send");
        assert_eq!(sent["verified_sender"], true);
        assert_eq!(sent["sender_verification"], "session");

        // Another session cannot send as alice by naming her, and an unbound
        // caller still needs a token under the fail-closed profile.
        assert!(send_as(&session_b, &project, &alice, &bob).await.is_err());
        assert_eq!(
            send_as(&stateless, &project, &alice, &bob)
                .await
                .expect_err("no proof"),
            "SENDER_TOKEN_REQUIRED"
        );

        // Bob's session reads its own inbox; alice's session cannot read it.
        let rows = inbox(&session_b, &project, &bob).await.expect("own inbox");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            inbox(&session_a, &project, &bob)
                .await
                .expect_err("borrowed inbox"),
            "SESSION_IDENTITY_MISMATCH"
        );

        // Re-registering alice's name from bob's session is refused and does
        // not rotate her token.
        let before = stored_token(&cx, &project, &alice).await;
        let error = register_agent(
            &session_b,
            project.clone(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some(alice.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("borrowed re-registration");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        assert_eq!(stored_token(&cx, &project, &alice).await, before);
    });
}

#[test]
fn unbound_sessions_keep_the_trusted_local_contract() {
    run_with_session_identity(false, |cx| async move {
        let session = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let other = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let project = format!("/tmp/session-identity-{}", unique_suffix());
        ensure_project(&session, project.clone(), None)
            .await
            .expect("ensure project");
        let alice = create_identity(&session, &project).await;
        let bob = create_identity(&other, &project).await;

        // A session holding no identity in the project may act as any agent,
        // unverified, exactly as without the feature.
        let stateless = McpContext::with_state(cx.clone(), 3, SessionState::new());
        let sent = send_as(&stateless, &project, &alice, &bob)
            .await
            .expect("trusted-local send");
        assert_eq!(sent["verified_sender"], false);
        assert!(sent.get("sender_verification").is_none());
        assert_eq!(
            inbox(&stateless, &project, &bob)
                .await
                .expect("inbox")
                .len(),
            1
        );

        // Registering a new agent by name binds it to that session.
        let fresh = McpContext::with_state(cx.clone(), 4, SessionState::new());
        let registered: Value = serde_json::from_str(
            &register_agent(
                &fresh,
                project.clone(),
                "codex-cli".to_string(),
                "gpt-5".to_string(),
                Some("PurpleHill".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("register new agent"),
        )
        .expect("register JSON");
        assert_eq!(registered["name"], "PurpleHill");
        let sent = send_as(&fresh, &project, "PurpleHill", &bob)
            .await
            .expect("bound send");
        assert_eq!(sent["sender_verification"], "session");
        assert_eq!(
            send_as(&fresh, &project, &alice, &bob)
                .await
                .expect_err("borrowed"),
            "SESSION_IDENTITY_MISMATCH"
        );
    });
}
