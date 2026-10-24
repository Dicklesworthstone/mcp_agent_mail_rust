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
    acquire_build_slot, create_agent_identity, ensure_product, ensure_project, fetch_inbox,
    fetch_inbox_product, force_release_file_reservation, list_contacts, macro_contact_handshake,
    products_link, register_agent, release_build_slot, renew_build_slot, request_contact,
    respond_contact, send_message, set_contact_policy, tool_error_code,
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
    run_with_identity_options(enabled, fail_closed, false, f)
}

fn run_with_identity_options<F, Fut, T>(
    enabled: bool,
    fail_closed: bool,
    worktrees: bool,
    f: F,
) -> T
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
        ("WORKTREES_ENABLED", if worktrees { "1" } else { "0" }),
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

async fn register_named(ctx: &McpContext, project: &str, name: &str) {
    register_agent(
        ctx,
        project.to_string(),
        "codex-cli".to_string(),
        "gpt-5".to_string(),
        Some(name.to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("register named identity");
    set_contact_policy(
        ctx,
        project.to_string(),
        name.to_string(),
        "open".to_string(),
    )
    .await
    .expect("open named identity contacts");
}

async fn product_inbox(
    ctx: &McpContext,
    product: &str,
    agent: &str,
    with_bodies: bool,
    limit: i32,
) -> McpResult<Value> {
    fetch_inbox_product(
        ctx,
        product.to_string(),
        agent.to_string(),
        Some(limit),
        None,
        Some(with_bodies),
        None,
    )
    .await
    .map(|json| serde_json::from_str(&json).expect("product inbox JSON"))
}

#[test]
fn product_inbox_authorizes_every_project_before_returning_body_or_metadata() {
    for enabled in [true, false] {
        run_with_identity_options(enabled, false, true, |cx| async move {
            let owner = McpContext::with_state(cx.clone(), 1, SessionState::new());
            let other = McpContext::with_state(cx.clone(), 2, SessionState::new());
            let unbound = McpContext::with_state(cx.clone(), 3, SessionState::new());
            let suffix = unique_suffix();
            let first = format!("/tmp/session-product-first-{suffix}");
            let second = format!("/tmp/session-product-second-{suffix}");
            let product = format!("session-product-{suffix}");
            ensure_product(&owner, None, Some(product.clone()))
                .await
                .expect("ensure product");
            for project in [&first, &second] {
                ensure_project(&owner, project.clone(), None)
                    .await
                    .expect("ensure linked project");
                products_link(&owner, product.clone(), project.clone())
                    .await
                    .expect("link product project");
            }
            register_named(&owner, &first, "BlueLake").await;
            register_named(&other, &second, "BlueLake").await;
            let sender_second = create_identity(&other, &second).await;
            let sender_first = create_identity(&other, &first).await;
            send_as(&other, &second, &sender_second, "BlueLake")
                .await
                .expect("second project message");
            send_as(&other, &first, &sender_first, "BlueLake")
                .await
                .expect("first project message");

            // Ownership in the first project does not constrain the second
            // project's trusted-local behavior until this session binds there.
            for bodies in [false, true] {
                let rows = product_inbox(&owner, &product, "bluelake", bodies, 50)
                    .await
                    .expect("own and unbound-project inboxes");
                assert_eq!(rows.as_array().expect("inbox array").len(), 2);
                assert_eq!(rows[0].get("body_md").is_some(), bodies);
            }
            let second_before = serde_json::to_value(stored_agent(&cx, &second, "BlueLake").await)
                .expect("second viewer snapshot");
            let _different_second_identity = create_identity(&owner, &second).await;
            for bodies in [false, true] {
                let result = product_inbox(&owner, &product, "bluelake", bodies, 1).await;
                if enabled {
                    let error = result.expect_err("every viewer is checked even below the limit");
                    assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
                } else {
                    assert_eq!(
                        result
                            .expect("default-off borrowed inbox")
                            .as_array()
                            .unwrap()
                            .len(),
                        1
                    );
                }
            }
            assert_eq!(
                serde_json::to_value(stored_agent(&cx, &second, "BlueLake").await)
                    .expect("second viewer snapshot"),
                second_before,
                "product inbox authorization never updates agent activity or profile"
            );
            let rows = product_inbox(&unbound, &product, "BlueLake", true, 50)
                .await
                .expect("unbound session retains product access");
            assert_eq!(rows.as_array().unwrap().len(), 2);
            assert!(
                rows.as_array()
                    .unwrap()
                    .iter()
                    .all(|row| row["read_ts"].is_null())
            );
        });
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn product_inbox_scope_excludes_later_links_and_legacy_name_aliases() {
    run_with_identity_options(true, false, true, |cx| async move {
        use mcp_agent_mail_db::queries;
        use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

        let owner = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let sender = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let suffix = unique_suffix();
        let first = format!("/tmp/session-scope-first-{suffix}");
        let second = format!("/tmp/session-scope-second-{suffix}");
        let foreign = format!("/tmp/session-scope-foreign-{suffix}");
        let product = format!("session-scope-{suffix}");
        let product_json: Value = serde_json::from_str(
            &ensure_product(&owner, None, Some(product.clone()))
                .await
                .expect("product"),
        )
        .unwrap();
        let product_id = product_json["id"].as_i64().unwrap();
        for project in [&first, &second, &foreign] {
            ensure_project(&owner, project.clone(), None)
                .await
                .expect("project");
            register_named(&owner, project, "BlueLake").await;
        }
        products_link(&owner, product.clone(), first.clone())
            .await
            .expect("initial link");
        let sender_name = create_identity(&sender, &first).await;
        send_as(&sender, &first, &sender_name, "BlueLake")
            .await
            .expect("canonical direct mail");
        let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("pool");
        let viewers = queries::product_inbox_agents(&cx, &pool, product_id, "bluelake")
            .await
            .into_result()
            .expect("preflight scope");
        assert_eq!(viewers.len(), 1);
        let viewer_ids = [viewers[0].id.unwrap()];
        let first_id = viewers[0].project_id;

        // Deterministically mutate both lookup inputs after preflight. The
        // preflighted identity set remains the scope of the subsequent read.
        products_link(&owner, product.clone(), second.clone())
            .await
            .expect("later link");
        let second_sender = create_identity(&sender, &second).await;
        send_as(&sender, &second, &second_sender, "BlueLake")
            .await
            .expect("later-project mail");
        let foreign_sender = create_identity(&sender, &foreign).await;
        send_as(&sender, &foreign, &foreign_sender, "BlueLake")
            .await
            .expect("foreign-product mail");
        let conn = pool.acquire(&cx).await.into_result().expect("connection");
        // A legacy mailbox can predate v10b's case-insensitive unique index
        // (idx_agents_project_name_nocase) and so hold a case-variant alias.
        // Represent that schema. The DDL runs in an explicit transaction
        // because FrankenSQLite refuses autocommit DDL on a pooled connection
        // that has seen later commits (br-1a63i).
        for statement in [
            "BEGIN IMMEDIATE",
            "DROP INDEX IF EXISTS idx_agents_project_name_nocase",
            "COMMIT",
        ] {
            conn.execute_raw(statement)
                .expect("represent pre-guard legacy schema");
        }
        conn.execute_sync(
            "INSERT INTO agents(project_id, name, program, model, inception_ts, last_active_ts) \
             VALUES (?, 'bluelake', 'test', 'test', 1, 1)",
            &[SqlValue::BigInt(first_id)],
        )
        .expect("legacy case-variant registration");
        let aliases = conn
            .query_sync(
                "SELECT id FROM agents WHERE project_id = ? AND name = 'bluelake' COLLATE BINARY",
                &[SqlValue::BigInt(first_id)],
            )
            .expect("alias id");
        let alias_id = aliases[0].get_named::<i64>("id").unwrap();
        drop(conn);
        let sender_row = stored_agent(&cx, &first, &sender_name).await;
        queries::create_message_with_recipients(
            &cx,
            &pool,
            first_id,
            sender_row.id.unwrap(),
            "Legacy alias only",
            "Alias body",
            None,
            "normal",
            false,
            "[]",
            &[(alias_id, "to")],
        )
        .await
        .into_result()
        .expect("alias-only mail");
        let first_project = queries::get_project_by_human_key(&cx, &pool, &first)
            .await
            .into_result()
            .expect("first project");
        send_as(
            &sender,
            &first,
            &sender_name,
            &format!("project:{}", first_project.slug),
        )
        .await
        .expect("shared mailbox mail");

        for bodies in [false, true] {
            let rows = queries::fetch_inbox_for_product_agent_scoped(
                &cx,
                &pool,
                product_id,
                "bluelake",
                &viewer_ids,
                false,
                None,
                50,
                bodies,
            )
            .await
            .into_result()
            .expect("pinned scope read");
            assert_eq!(rows.len(), 2, "only canonical direct and shared deliveries");
            assert!(rows.iter().all(|row| row.message.project_id == first_id));
            assert!(
                rows.iter()
                    .all(|row| row.message.subject != "Legacy alias only")
            );
            assert_eq!(rows.iter().filter(|row| row.kind == "project").count(), 1);
            assert!(
                rows.iter()
                    .all(|row| row.message.body_md.is_empty() != bodies)
            );
        }
        let refreshed = queries::product_inbox_agents(&cx, &pool, product_id, "bluelake")
            .await
            .into_result()
            .expect("canonical refreshed viewers");
        assert_eq!(refreshed.len(), 2);
        assert_eq!(
            refreshed
                .iter()
                .find(|viewer| viewer.project_id == first_id)
                .unwrap()
                .id,
            Some(viewer_ids[0])
        );
        let foreign_id = stored_agent(&cx, &foreign, "BlueLake").await.id.unwrap();
        let foreign_rows = queries::fetch_inbox_for_product_agent_scoped(
            &cx,
            &pool,
            product_id,
            "BlueLake",
            &[foreign_id],
            false,
            None,
            50,
            true,
        )
        .await
        .into_result()
        .expect("foreign scope read");
        assert!(
            foreign_rows.is_empty(),
            "IDs never bypass product membership"
        );
        let empty = queries::fetch_inbox_for_product_agent_scoped(
            &cx,
            &pool,
            product_id,
            "BlueLake",
            &[],
            false,
            None,
            50,
            true,
        )
        .await
        .into_result()
        .expect("empty scope");
        assert!(empty.is_empty());
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn build_slots_authorize_before_creating_or_changing_lease_files() {
    for enabled in [true, false] {
        run_with_identity_options(enabled, false, true, |cx| async move {
            let owner = McpContext::with_state(cx.clone(), 1, SessionState::new());
            let other = McpContext::with_state(cx.clone(), 2, SessionState::new());
            let unbound = McpContext::with_state(cx.clone(), 3, SessionState::new());
            let project = format!("/tmp/session-build-slots-{}", unique_suffix());
            let project_json: Value = serde_json::from_str(
                &ensure_project(&owner, project.clone(), None)
                    .await
                    .expect("project"),
            )
            .expect("project JSON");
            register_named(&owner, &project, "BlueLake").await;
            register_named(&other, &project, "PurpleHill").await;
            let slots_root = Config::get()
                .storage_root
                .join("projects")
                .join(project_json["slug"].as_str().expect("project slug"))
                .join("build_slots");
            let denied = acquire_build_slot(
                &owner,
                project.clone(),
                "purplehill".to_string(),
                "new-slot".to_string(),
                None,
                None,
            )
            .await;
            if enabled {
                assert_eq!(
                    tool_error_code(&denied.expect_err("borrowed acquire")),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
                assert!(
                    !slots_root.join("new-slot").exists(),
                    "authorization precedes directory and lock creation"
                );
            } else {
                denied.expect("default-off borrowed acquire");
            }

            acquire_build_slot(
                &other,
                project.clone(),
                "PurpleHill".to_string(),
                "existing".to_string(),
                None,
                None,
            )
            .await
            .expect("holder acquires lease");
            let lease_path = std::fs::read_dir(slots_root.join("existing"))
                .expect("slot files")
                .map(|entry| entry.expect("slot entry").path())
                .find(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "json")
                })
                .expect("persisted lease");
            let before = std::fs::read(&lease_path).expect("lease bytes");
            let borrowed = [
                acquire_build_slot(
                    &owner,
                    project.clone(),
                    "purplehill".to_string(),
                    "existing".to_string(),
                    Some(7200),
                    None,
                )
                .await,
                renew_build_slot(
                    &owner,
                    project.clone(),
                    "purplehill".to_string(),
                    "existing".to_string(),
                    Some(7200),
                )
                .await,
                release_build_slot(
                    &owner,
                    project.clone(),
                    "purplehill".to_string(),
                    "existing".to_string(),
                )
                .await,
            ];
            for result in borrowed {
                if enabled {
                    assert_eq!(
                        tool_error_code(&result.expect_err("borrowed lease mutation")),
                        Some("SESSION_IDENTITY_MISMATCH")
                    );
                } else {
                    result.expect("default-off borrowed lease mutation");
                }
            }
            if enabled {
                assert_eq!(
                    std::fs::read(&lease_path).expect("unchanged lease bytes"),
                    before
                );
            }

            acquire_build_slot(
                &owner,
                project.clone(),
                "bluelake".to_string(),
                "owned".to_string(),
                None,
                None,
            )
            .await
            .expect("own canonical-name acquire");
            let renewed: Value = serde_json::from_str(
                &renew_build_slot(
                    &owner,
                    project.clone(),
                    "bluelake".to_string(),
                    "owned".to_string(),
                    None,
                )
                .await
                .expect("own renew"),
            )
            .unwrap();
            assert_eq!(renewed["renewed"], true);
            let released: Value = serde_json::from_str(
                &release_build_slot(
                    &owner,
                    project.clone(),
                    "bluelake".to_string(),
                    "owned".to_string(),
                )
                .await
                .expect("own release"),
            )
            .unwrap();
            assert_eq!(released["released"], true);

            let missing = acquire_build_slot(
                &owner,
                project.clone(),
                "UnregisteredBuilder".to_string(),
                "unknown".to_string(),
                None,
                None,
            )
            .await;
            if enabled {
                assert_eq!(
                    tool_error_code(&missing.expect_err("bound unknown actor")),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
                assert!(!slots_root.join("unknown").exists());
            } else {
                missing.expect("default-off unregistered actor");
            }
            acquire_build_slot(
                &unbound,
                project,
                "UnregisteredBuilder".to_string(),
                "trusted".to_string(),
                None,
                None,
            )
            .await
            .expect("unbound unregistered actor remains supported");
        });
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn force_release_authorizes_the_requester_and_preserves_operator_override() {
    for enabled in [true, false] {
        run_with_session_identity(enabled, false, |cx| async move {
            let owner = McpContext::with_state(cx.clone(), 1, SessionState::new());
            let other = McpContext::with_state(cx.clone(), 2, SessionState::new());
            let project = format!("/tmp/session-force-release-{}", unique_suffix());
            ensure_project(&owner, project.clone(), None)
                .await
                .expect("project");
            let alice = create_identity(&owner, &project).await;
            let bob = create_identity(&other, &project).await;
            let holder = create_identity(&other, &project).await;
            let holder_row = stored_agent(&cx, &project, &holder).await;
            let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("pool");
            let rows = mcp_agent_mail_db::queries::create_file_reservations(
                &cx,
                &pool,
                holder_row.project_id,
                holder_row.id.unwrap(),
                &["src/stale.rs"],
                3600,
                true,
                "identity force-release test",
            )
            .await
            .into_result()
            .expect("reservation fixture");
            let reservation_id = rows[0].id.unwrap();
            let conn = pool.acquire(&cx).await.into_result().expect("connection");
            conn.execute_sync(
                "UPDATE file_reservations SET expires_ts = 1 WHERE id = ?",
                &[mcp_agent_mail_db::sqlmodel::Value::BigInt(reservation_id)],
            )
            .expect("expire lease for legitimate operator release");
            drop(conn);
            let before = serde_json::to_value(
                mcp_agent_mail_db::queries::get_reservations_by_ids(&cx, &pool, &[reservation_id])
                    .await
                    .into_result()
                    .expect("reservation snapshot"),
            )
            .unwrap();
            let refused = force_release_file_reservation(
                &owner,
                project.clone(),
                bob.to_ascii_lowercase(),
                reservation_id,
                Some("borrowed requester".to_string()),
                Some(true),
            )
            .await;
            if enabled {
                assert_eq!(
                    tool_error_code(&refused.expect_err("borrowed operator name")),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
                assert_eq!(
                    serde_json::to_value(
                        mcp_agent_mail_db::queries::get_reservations_by_ids(
                            &cx,
                            &pool,
                            &[reservation_id]
                        )
                        .await
                        .into_result()
                        .expect("unchanged reservation")
                    )
                    .unwrap(),
                    before
                );
                assert_eq!(
                    inbox(&other, &project, &holder)
                        .await
                        .expect("holder inbox"),
                    Vec::<Value>::new()
                );
                let released: Value = serde_json::from_str(
                    &force_release_file_reservation(
                        &owner,
                        project.clone(),
                        alice.clone(),
                        reservation_id,
                        Some("actual requester".to_string()),
                        Some(true),
                    )
                    .await
                    .expect("bound operator may release another holder's expired lease"),
                )
                .unwrap();
                assert_eq!(released["released"], 1);
            } else {
                let released: Value =
                    serde_json::from_str(&refused.expect("default-off borrowed operator")).unwrap();
                assert_eq!(released["released"], 1);
            }
            let notices = inbox(&other, &project, &holder)
                .await
                .expect("holder notice");
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0]["from"], if enabled { alice } else { bob });
        });
    }
}
