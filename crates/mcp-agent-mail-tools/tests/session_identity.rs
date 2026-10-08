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
use fastmcp::prelude::{McpContext, McpResult};
use fastmcp_core::SessionState;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    create_agent_identity, ensure_project, fetch_inbox, list_contacts, macro_contact_handshake,
    register_agent, request_contact, respond_contact, send_message, set_contact_policy,
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

fn run_with_session_identity<F, Fut, T>(enabled: bool, fail_closed: bool, f: F) -> T
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
        (
            "MESSAGING_SESSION_IDENTITY",
            if enabled { "true" } else { "false" },
        ),
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

async fn stored_agent(cx: &Cx, project: &str, agent: &str) -> mcp_agent_mail_db::AgentRow {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("db pool");
    let project = mcp_agent_mail_db::queries::get_project_by_human_key(cx, &pool, project)
        .await
        .into_result()
        .expect("project");
    let row =
        mcp_agent_mail_db::queries::get_agent(cx, &pool, project.id.expect("project id"), agent)
            .await
            .into_result()
            .expect("agent");
    mcp_agent_mail_db::queries::get_agent_by_id_fresh(cx, &pool, row.id.expect("agent id"))
        .await
        .into_result()
        .expect("persisted agent")
}

async fn stored_token(cx: &Cx, project: &str, agent: &str) -> Option<String> {
    stored_agent(cx, project, agent).await.registration_token
}

async fn contact_rows(cx: &Cx, project: &str, agent: &str) -> Value {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("db pool");
    let row = stored_agent(cx, project, agent).await;
    let links = mcp_agent_mail_db::queries::list_contacts(
        cx,
        &pool,
        row.project_id,
        row.id.expect("agent id"),
    )
    .await
    .into_result()
    .expect("contact rows");
    serde_json::to_value(links).expect("contact rows JSON")
}

async fn request(
    ctx: &McpContext,
    project: &str,
    requester: &str,
    target: &str,
    to_project: Option<&str>,
) -> McpResult<Value> {
    request_contact(
        ctx,
        project.to_string(),
        requester.to_string(),
        target.to_string(),
        to_project.map(str::to_string),
        Some("Session identity contact test".to_string()),
        Some(3600),
        Some(true),
        Some("codex-cli".to_string()),
        Some("gpt-5".to_string()),
        Some("Contact requester".to_string()),
    )
    .await
    .map(|json| serde_json::from_str(&json).expect("request JSON"))
}

async fn handshake(
    ctx: &McpContext,
    project: &str,
    requester: &str,
    target: &str,
    to_project: Option<&str>,
    auto_accept: bool,
) -> McpResult<Value> {
    macro_contact_handshake(
        ctx,
        project.to_string(),
        Some(requester.to_string()),
        Some(target.to_string()),
        None,
        None,
        to_project.map(str::to_string),
        Some("Session identity handshake test".to_string()),
        Some(auto_accept),
        Some(600),
        None,
        None,
        None,
        Some(true),
        Some("codex-cli".to_string()),
        Some("gpt-5".to_string()),
        Some("Handshake requester".to_string()),
        None,
    )
    .await
    .map(|json| serde_json::from_str(&json).expect("handshake JSON"))
}

#[test]
fn session_bound_identity_sends_without_a_token_and_cannot_be_borrowed() {
    run_with_session_identity(true, true, |cx| async move {
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
    run_with_session_identity(true, false, |cx| async move {
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

#[test]
fn contact_mutations_authorize_the_requester_recipient_and_policy_owner() {
    run_with_session_identity(true, false, |cx| async move {
        let session_a = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let session_b = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let project = format!("/tmp/session-contacts-{}", unique_suffix());
        ensure_project(&session_a, project.clone(), None)
            .await
            .expect("ensure project");
        let alice = create_identity(&session_a, &project).await;
        let bob = create_identity(&session_b, &project).await;

        let pending = request(&session_a, &project, &alice, &bob, None)
            .await
            .expect("own contact request");
        assert_eq!(pending["status"], "pending");
        let before_links = contact_rows(&cx, &project, &bob).await;
        let before_agent =
            serde_json::to_value(stored_agent(&cx, &project, &bob).await).expect("agent snapshot");

        // Case-normalized borrowed names must not create a reverse link or
        // rewrite profile/activity metadata, even with auto-registration on.
        let error = request(
            &session_a,
            &project,
            &bob.to_ascii_lowercase(),
            &alice,
            None,
        )
        .await
        .expect_err("borrowed requester");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        for accept in [true, false] {
            let error = respond_contact(
                &session_a,
                project.clone(),
                bob.clone(),
                alice.clone(),
                None,
                accept,
                Some(60),
            )
            .await
            .expect_err("only the recipient may decide this request");
            assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        }
        let error = set_contact_policy(
            &session_a,
            project.clone(),
            bob.to_ascii_lowercase(),
            "block_all".to_string(),
        )
        .await
        .expect_err("borrowed policy owner");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        assert_eq!(contact_rows(&cx, &project, &bob).await, before_links);
        assert_eq!(
            serde_json::to_value(stored_agent(&cx, &project, &bob).await).expect("agent snapshot"),
            before_agent,
            "refused contact actions leave policy, profile, token, and timestamps unchanged"
        );
        assert_eq!(
            inbox(&session_b, &project, &bob)
                .await
                .expect("inbox")
                .len(),
            1
        );
        assert_eq!(
            inbox(&session_a, &project, &alice).await.expect("inbox"),
            Vec::<Value>::new()
        );

        // Contact discovery stays read-only and usable across identities.
        let listed: Vec<Value> = serde_json::from_str(
            &list_contacts(&session_a, project.clone(), bob.clone())
                .await
                .expect("read contact metadata"),
        )
        .expect("contact JSON");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["status"], "pending");

        let approved: Value = serde_json::from_str(
            &respond_contact(
                &session_b,
                project.clone(),
                bob.clone(),
                alice,
                None,
                true,
                None,
            )
            .await
            .expect("recipient approves its own request"),
        )
        .expect("approval JSON");
        assert_eq!(approved["approved"], true);
        set_contact_policy(&session_b, project, bob, "contacts_only".to_string())
            .await
            .expect("own policy");
    });
}

#[test]
fn handshake_preflights_both_actors_before_creating_links_agents_or_projects() {
    run_with_session_identity(true, false, |cx| async move {
        let session_a = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let session_b = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let project = format!("/tmp/session-handshake-{}", unique_suffix());
        ensure_project(&session_a, project.clone(), None)
            .await
            .expect("ensure project");
        let alice = create_identity(&session_a, &project).await;
        let bob = create_identity(&session_b, &project).await;
        let before_links = contact_rows(&cx, &project, &bob).await;
        let missing_project = format!("/tmp/session-handshake-missing-{}", unique_suffix());

        let error = handshake(&session_a, &project, &alice, &bob, None, true)
            .await
            .expect_err("requester cannot auto-approve as another recipient");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        let error = handshake(
            &session_a,
            &project,
            &bob,
            &alice,
            Some(&missing_project),
            false,
        )
        .await
        .expect_err("borrowed requester refused before creating destination");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        let error = handshake(
            &session_a,
            &missing_project,
            "PurpleHill",
            &bob,
            Some(&project),
            true,
        )
        .await
        .expect_err("borrowed recipient refused before registering requester");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));

        assert_eq!(contact_rows(&cx, &project, &bob).await, before_links);
        assert_eq!(
            inbox(&session_b, &project, &bob).await.expect("inbox"),
            Vec::<Value>::new()
        );
        let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("db pool");
        let missing =
            mcp_agent_mail_db::queries::get_project_by_human_key(&cx, &pool, &missing_project)
                .await
                .into_result()
                .expect_err("failed handshake must not create the missing project");
        assert!(matches!(
            missing,
            asupersync::OutcomeError::Err(mcp_agent_mail_db::DbError::NotFound { .. })
        ));

        // A session that explicitly holds both identities can still use the
        // convenience macro and sends only the non-actionable approval notice.
        let owned_target = create_identity(&session_a, &project).await;
        let approved = handshake(&session_a, &project, &alice, &owned_target, None, true)
            .await
            .expect("both handshake actors held by this session");
        assert_eq!(approved["response"]["approved"], true);
        let notices = inbox(&session_a, &project, &owned_target)
            .await
            .expect("approval notice");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["ack_required"], false);
        assert_eq!(
            notices[0]["subject"],
            format!("Contact approved: {alice} -> {owned_target}")
        );
    });
}

#[test]
fn cross_project_contacts_check_the_recipient_binding_in_its_own_project() {
    run_with_session_identity(true, false, |cx| async move {
        let session_a = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let session_b = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let source = format!("/tmp/session-contact-source-{}", unique_suffix());
        let target = format!("/tmp/session-contact-target-{}", unique_suffix());
        ensure_project(&session_a, source.clone(), None)
            .await
            .expect("source project");
        let target_project: Value = serde_json::from_str(
            &ensure_project(&session_b, target.clone(), None)
                .await
                .expect("target project"),
        )
        .expect("target project JSON");
        let alice = create_identity(&session_a, &source).await;
        let bob = create_identity(&session_b, &target).await;
        let _other_source_identity = create_identity(&session_b, &source).await;

        request(&session_a, &source, &alice, &bob, Some(&target))
            .await
            .expect("cross-project request");
        respond_contact(
            &session_b,
            target.clone(),
            bob.clone(),
            alice.clone(),
            Some(source.clone()),
            true,
            None,
        )
        .await
        .expect("the bound recipient decides even with another source-project identity");

        // Existing per-project contract: no target-project binding leaves
        // that project's trusted-local authorization unchanged.
        let approved = handshake(&session_a, &source, &alice, &bob, Some(&target), true)
            .await
            .expect("unbound target project retains trusted-local behavior");
        assert_eq!(approved["response"]["approved"], true);

        let _other_target_identity = create_identity(&session_a, &target).await;
        let before_links = contact_rows(&cx, &target, &bob).await;
        let before_inbox = inbox(&session_b, &target, &bob)
            .await
            .expect("target inbox");
        let qualified_target = format!(
            "project:{}#{bob}",
            target_project["slug"].as_str().expect("target slug")
        );
        let error = handshake(&session_a, &source, &alice, &qualified_target, None, true)
            .await
            .expect_err("shorthand must not bypass the target project's binding");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        let error = respond_contact(
            &session_a,
            target.clone(),
            bob.clone(),
            alice.clone(),
            Some(source.clone()),
            false,
            None,
        )
        .await
        .expect_err("source ownership never authorizes a borrowed bound-project recipient");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        assert_eq!(contact_rows(&cx, &target, &bob).await, before_links);
        assert_eq!(
            inbox(&session_b, &target, &bob).await.expect("inbox"),
            before_inbox
        );

        // The requester may still request contact; only target-side actions
        // require its identity. Existing approvals survive the re-request.
        let repeated = handshake(&session_a, &source, &alice, &bob, Some(&target), false)
            .await
            .expect("cross-project request-only handshake");
        assert_eq!(repeated["request"]["status"], "approved");
        assert!(repeated["response"].is_null());
    });
}

#[test]
fn implicit_contact_registration_does_not_borrow_or_establish_session_identity() {
    run_with_session_identity(true, false, |cx| async move {
        let owner = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let unbound = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let project = format!("/tmp/session-implicit-contact-{}", unique_suffix());
        ensure_project(&owner, project.clone(), None)
            .await
            .expect("ensure project");
        let target = create_identity(&owner, &project).await;
        let implicit_requester = if target == "PurpleHill" {
            "BlueLake"
        } else {
            "PurpleHill"
        };
        let error = request(&owner, &project, implicit_requester, &target, None)
            .await
            .expect_err("a bound session must explicitly register another acting identity");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("db pool");
        let target_row = stored_agent(&cx, &project, &target).await;
        let missing = mcp_agent_mail_db::queries::get_agent(
            &cx,
            &pool,
            target_row.project_id,
            implicit_requester,
        )
        .await
        .into_result()
        .expect_err("refused implicit registration leaves no agent row");
        assert!(matches!(
            missing,
            asupersync::OutcomeError::Err(mcp_agent_mail_db::DbError::NotFound { .. })
        ));

        // The same first-use macro remains valid for a trusted-local caller:
        // implicit registration must not acquire a new binding mid-handshake.
        let approved = handshake(&unbound, &project, implicit_requester, &target, None, true)
            .await
            .expect("unbound implicit requester can complete its handshake");
        assert_eq!(approved["response"]["approved"], true);
        assert_eq!(
            mcp_agent_mail_tools::session_identity::session_bindings(&unbound),
            Vec::new()
        );
        let before = serde_json::to_value(stored_agent(&cx, &project, implicit_requester).await)
            .expect("implicit requester snapshot");
        let error = request(&owner, &project, implicit_requester, &target, None)
            .await
            .expect_err("naming an implicit requester still does not confer ownership");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        assert_eq!(
            serde_json::to_value(stored_agent(&cx, &project, implicit_requester).await)
                .expect("implicit requester snapshot"),
            before
        );
    });
}

#[test]
fn disabled_session_identity_preserves_trusted_local_contact_macros() {
    run_with_session_identity(false, false, |cx| async move {
        let session_a = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let session_b = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let project = format!("/tmp/session-contact-default-{}", unique_suffix());
        ensure_project(&session_a, project.clone(), None)
            .await
            .expect("ensure project");
        let alice = create_identity(&session_a, &project).await;
        let bob = create_identity(&session_b, &project).await;
        assert_eq!(
            mcp_agent_mail_tools::session_identity::session_bindings(&session_a),
            Vec::new()
        );

        let approved = handshake(&session_a, &project, &bob, &alice, None, true)
            .await
            .expect("trusted-local handshake may act as both named agents");
        assert_eq!(approved["response"]["approved"], true);
        respond_contact(
            &session_b,
            project.clone(),
            alice,
            bob.clone(),
            None,
            false,
            None,
        )
        .await
        .expect("trusted-local recipient response");
        set_contact_policy(
            &session_a,
            project.clone(),
            bob.clone(),
            "block_all".to_string(),
        )
        .await
        .expect("trusted-local policy update");
        assert_eq!(
            stored_agent(&cx, &project, &bob).await.contact_policy,
            "block_all"
        );
    });
}
