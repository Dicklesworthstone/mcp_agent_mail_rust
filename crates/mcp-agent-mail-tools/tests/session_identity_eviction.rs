//! Capacity eviction must not become a trusted-local identity reset (GH#279).
//!
//! These tests create every identity through the real tools and use the normal
//! database/archive fixture. No direct binding injection, actor-check stub, or
//! mocked database is used to force eviction. This is in-process tool coverage,
//! not HTTP authentication or multi-process transport coverage.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use fastmcp_core::SessionState;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    create_agent_identity, ensure_project, fetch_inbox, register_agent, send_message,
    session_identity, set_contact_policy, tool_error_code,
};
use serde_json::Value;
use std::path::PathBuf;

struct ResetConfigCache;

impl Drop for ResetConfigCache {
    fn drop(&mut self) {
        Config::reset_cached();
    }
}

fn run_case<F, Fut>(case: F)
where
    F: FnOnce(Cx, PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let temp = tempfile::tempdir().expect("isolated mailbox");
    let root = temp.path().to_path_buf();
    let database = format!("sqlite://{}", root.join("mail.sqlite3").display());
    let archive = root.join("archive").to_string_lossy().into_owned();
    with_process_env_overrides_for_test(
        &[
            ("DATABASE_URL", database.as_str()),
            ("STORAGE_ROOT", archive.as_str()),
            ("WORKTREES_ENABLED", "0"),
            ("MESSAGING_SESSION_IDENTITY", "true"),
            // A token-profile refusal must not mask the eviction regression.
            ("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "0"),
            // The projects live under a tempfile `.tmpXXXXXX` directory, which
            // the ephemeral-root guard refuses by default.
            ("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1"),
        ],
        || {
            Config::reset_cached();
            let _reset = ResetConfigCache;
            let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
            runtime.block_on(async {
                let cx = Cx::current().expect("runtime context");
                case(cx, root).await;
            });
        },
    );
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
        .expect("create identity through the tool"),
    )
    .expect("identity JSON");
    assert!(created.get("registration_token").is_none());
    let name = created["name"].as_str().expect("agent name").to_string();
    set_contact_policy(ctx, project.to_string(), name.clone(), "open".to_string())
        .await
        .expect("owner sets its contact policy");
    name
}

async fn send_as(ctx: &McpContext, project: &str, sender: &str, to: &str) -> Result<Value, String> {
    send_message(
        ctx,
        project.to_string(),
        sender.to_string(),
        vec![to.to_string()],
        "Eviction identity test".to_string(),
        "Capacity must not broaden identity authority.".to_string(),
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

async fn stored_agent(cx: &Cx, project: &str, name: &str) -> mcp_agent_mail_db::AgentRow {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("live pool");
    let project = mcp_agent_mail_db::queries::get_project_by_human_key(cx, &pool, project)
        .await
        .into_result()
        .expect("stored project");
    let row =
        mcp_agent_mail_db::queries::get_agent(cx, &pool, project.id.expect("project id"), name)
            .await
            .into_result()
            .expect("stored identity");
    mcp_agent_mail_db::queries::get_agent_by_id_fresh(cx, &pool, row.id.expect("agent id"))
        .await
        .into_result()
        .expect("fresh stored identity")
}

struct EvictedFixture {
    actor: McpContext,
    actor_state: SessionState,
    peer: McpContext,
    first_project: String,
    second_project: String,
    original: String,
    victim: String,
    retained: String,
    second_peer: String,
}

async fn evicted_fixture(cx: &Cx, root: &std::path::Path) -> EvictedFixture {
    let actor_state = SessionState::new();
    let actor = McpContext::with_state(cx.clone(), 1, actor_state.clone());
    let peer = McpContext::with_state(cx.clone(), 2, SessionState::new());
    let first_project = root.join("first-project").to_string_lossy().into_owned();
    let second_project = root.join("second-project").to_string_lossy().into_owned();
    for project in [&first_project, &second_project] {
        ensure_project(&actor, project.clone(), None)
            .await
            .expect("ensure project");
    }
    let original = create_identity(&actor, &first_project).await;
    let victim = create_identity(&peer, &first_project).await;
    let second_peer = create_identity(&peer, &second_project).await;
    let sent = send_as(&actor, &first_project, &original, &victim)
        .await
        .expect("initial owned send");
    assert_eq!(sent["sender_verification"], "session");
    let original_id = stored_agent(cx, &first_project, &original).await.id;

    // Exercise the shipped 32-proof boundary through actual registrations.
    // Assert eviction rather than allowing a changed limit to hide coverage.
    let mut retained = String::new();
    for _ in 0..32 {
        retained = create_identity(&actor, &second_project).await;
    }
    assert_eq!(session_identity::session_bindings(&actor).len(), 32);
    assert!(!session_identity::holds(&actor, original_id));
    EvictedFixture {
        actor,
        actor_state,
        peer,
        first_project,
        second_project,
        original,
        victim,
        retained,
        second_peer,
    }
}

#[test]
fn real_tools_do_not_borrow_names_or_mutate_mail_and_profiles_after_eviction() {
    run_case(|cx, root| async move {
        let fixture = evicted_fixture(&cx, &root).await;
        let before = stored_agent(&cx, &fixture.first_project, &fixture.victim).await;
        for sender in [&fixture.original, &fixture.victim] {
            let recipient = if sender == &fixture.original {
                &fixture.victim
            } else {
                &fixture.original
            };
            assert_eq!(
                send_as(&fixture.actor, &fixture.first_project, sender, recipient)
                    .await
                    .expect_err("no sender proof after eviction"),
                "SESSION_IDENTITY_MISMATCH"
            );
        }
        assert_eq!(
            inbox(&fixture.actor, &fixture.first_project, &fixture.victim)
                .await
                .expect_err("eviction does not grant another inbox"),
            "SESSION_IDENTITY_MISMATCH"
        );
        let error = register_agent(
            &fixture.actor,
            fixture.first_project.clone(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some(fixture.victim.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("eviction does not authorize a profile rewrite");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        let error = set_contact_policy(
            &fixture.actor,
            fixture.first_project.clone(),
            fixture.victim.clone(),
            "block_all".to_string(),
        )
        .await
        .expect_err("eviction does not authorize contact policy changes");
        assert_eq!(tool_error_code(&error), Some("SESSION_IDENTITY_MISMATCH"));
        let after = stored_agent(&cx, &fixture.first_project, &fixture.victim).await;
        assert_eq!(after.registration_token, before.registration_token);
        assert_eq!(after.contact_policy, before.contact_policy);
        assert_eq!(after.program, before.program);
        assert_eq!(after.model, before.model);
        assert_eq!(
            inbox(&fixture.peer, &fixture.first_project, &fixture.victim)
                .await
                .expect("recipient reads its own mailbox")
                .len(),
            1,
            "refused sends must not append mail"
        );
        let sent = send_as(
            &fixture.actor,
            &fixture.second_project,
            &fixture.retained,
            &fixture.second_peer,
        )
        .await
        .expect("retained identity still sends normally");
        assert_eq!(sent["verified_sender"], true);
        assert_eq!(sent["sender_verification"], "session");
    });
}

#[test]
fn shared_session_with_no_remaining_proofs_keeps_its_evicted_project_restricted() {
    run_case(|cx, root| async move {
        let fixture = evicted_fixture(&cx, &root).await;
        // Explicitly drop all remaining proofs. This is the public session
        // lifecycle helper used by retirement, not a private-state overwrite.
        for binding in session_identity::session_bindings(&fixture.actor) {
            session_identity::unbind(&fixture.actor, binding.agent_id);
        }
        let followup = McpContext::with_state(cx.clone(), 3, fixture.actor_state.clone());
        assert_eq!(session_identity::session_bindings(&followup), Vec::new());
        assert_eq!(
            send_as(
                &followup,
                &fixture.first_project,
                &fixture.victim,
                &fixture.original
            )
            .await
            .expect_err("an empty proof list is not an identity reset"),
            "SESSION_IDENTITY_MISMATCH"
        );
        assert_eq!(
            inbox(&followup, &fixture.first_project, &fixture.victim)
                .await
                .expect_err("shared context retains eviction restrictions"),
            "SESSION_IDENTITY_MISMATCH"
        );
        let fresh = McpContext::with_state(cx, 4, SessionState::new());
        assert_eq!(
            inbox(&fresh, &fixture.first_project, &fixture.victim)
                .await
                .expect("a genuinely fresh session keeps trusted-local behavior")
                .len(),
            1
        );
        let new_owner = create_identity(&followup, &fixture.first_project).await;
        let sent = send_as(
            &followup,
            &fixture.first_project,
            &new_owner,
            &fixture.victim,
        )
        .await
        .expect("explicitly established new identity authorizes normally");
        assert_eq!(sent["sender_verification"], "session");
    });
}

/// Read the actual persistent surfaces that a refused macro used to mutate.
/// The source pool and connection drop before calling the read-lane preflight.
async fn handshake_counts(cx: &Cx) -> [i64; 5] {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("live pool");
    let conn = pool.acquire(cx).await.into_result().expect("checkout");
    let mut counts = [0; 5];
    for (count, sql) in counts.iter_mut().zip([
        "SELECT COUNT(*) AS n FROM projects",
        "SELECT COUNT(*) AS n FROM agents",
        "SELECT COUNT(*) AS n FROM agent_links",
        "SELECT COUNT(*) AS n FROM messages",
        "SELECT COUNT(*) AS n FROM message_recipients",
    ]) {
        let rows = conn.query_sync(sql, &[]).expect("read persisted count");
        *count = rows[0].get_named::<i64>("n").expect("integer count");
    }
    counts
}

async fn handshake(
    ctx: &McpContext,
    source: &str,
    requester: &str,
    target_project: &str,
    target: &str,
    register_if_missing: bool,
) -> Result<Value, String> {
    mcp_agent_mail_tools::macro_contact_handshake(
        ctx,
        source.to_string(),
        Some(requester.to_string()),
        Some(target.to_string()),
        None,
        None,
        Some(target_project.to_string()),
        Some("eviction preflight regression".to_string()),
        Some(true),
        Some(3600),
        Some("Welcome after authorization".to_string()),
        Some("Both contact actors must be admitted before this mail is sent.".to_string()),
        None,
        Some(register_if_missing),
        Some("codex-cli".to_string()),
        Some("gpt-5".to_string()),
        None,
        None,
    )
    .await
    .map(|json| serde_json::from_str(&json).expect("handshake JSON"))
    .map_err(|error| tool_error_code(&error).unwrap_or("UNKNOWN").to_string())
}

#[test]
fn auto_accept_preflights_evicted_target_before_any_handshake_writes() {
    run_case(|cx, root| async move {
        let fixture = evicted_fixture(&cx, &root).await;
        for binding in session_identity::session_bindings(&fixture.actor) {
            session_identity::unbind(&fixture.actor, binding.agent_id);
        }
        // A later request on the SAME session inherits restrictions despite
        // having no live proofs. The source project is deliberately unbound;
        // only preflighting the evicted target prevents partial side effects.
        let followup = McpContext::with_state(cx.clone(), 3, fixture.actor_state.clone());
        assert!(session_identity::session_bindings(&followup).is_empty());
        let before = handshake_counts(&cx).await;
        assert_eq!(before[2], 0, "the fixture has no contact links");
        let absent_source = root.join("new-source").to_string_lossy().into_owned();
        for (source, requester, register) in [
            (
                fixture.second_project.as_str(),
                fixture.retained.as_str(),
                false,
            ),
            (absent_source.as_str(), "BlueLake", true),
        ] {
            let error = handshake(
                &followup,
                source,
                requester,
                &fixture.first_project,
                &fixture.victim,
                register,
            )
            .await
            .expect_err("eviction must refuse the target before the request is committed");
            assert_eq!(error, "SESSION_IDENTITY_MISMATCH");
            assert_eq!(
                handshake_counts(&cx).await,
                before,
                "refusal must not create a project, requester, link, intro, welcome or delivery"
            );
        }
        // Positive control: no global disablement of contact workflows, and
        // no accidental restriction on a genuinely fresh trusted-local session.
        let fresh = McpContext::with_state(cx.clone(), 4, SessionState::new());
        let result = handshake(
            &fresh,
            &fixture.second_project,
            &fixture.retained,
            &fixture.first_project,
            &fixture.victim,
            false,
        )
        .await
        .expect("fresh unbound session can complete the same real handshake");
        assert_eq!(result["response"]["approved"], true);
        assert!(result["welcome_message"].is_object());
        assert_eq!(handshake_counts(&cx).await[2], before[2] + 1);
    });
}

#[test]
fn saturated_eviction_history_refuses_new_source_before_creating_its_project() {
    run_case(|cx, root| async move {
        let actor = McpContext::with_state(cx.clone(), 1, SessionState::new());
        let peer = McpContext::with_state(cx.clone(), 2, SessionState::new());
        let target_project = root.join("target").to_string_lossy().into_owned();
        ensure_project(&peer, target_project.clone(), None)
            .await
            .unwrap();
        let target = create_identity(&peer, &target_project).await;

        // 32 held proofs plus 33 distinct evicted projects exhaust the shipped
        // 32-project history through real registrations, not a state injection.
        for ordinal in 0..65 {
            let project = root.join(format!("history-{ordinal}"));
            let project = project.to_string_lossy().into_owned();
            ensure_project(&actor, project.clone(), None).await.unwrap();
            create_identity(&actor, &project).await;
        }
        assert_eq!(session_identity::session_bindings(&actor).len(), 32);
        for binding in session_identity::session_bindings(&actor) {
            session_identity::unbind(&actor, binding.agent_id);
        }
        assert!(session_identity::session_bindings(&actor).is_empty());
        let unrelated = set_contact_policy(
            &actor,
            target_project.clone(),
            target.clone(),
            "open".to_string(),
        )
        .await
        .expect_err("saturation must constrain even a project never held by this session");
        assert_eq!(
            tool_error_code(&unrelated),
            Some("SESSION_IDENTITY_MISMATCH")
        );

        let before = handshake_counts(&cx).await;
        let source = root.join("never-created").to_string_lossy().into_owned();
        assert_eq!(
            handshake(&actor, &source, "BlueLake", &target_project, &target, true)
                .await
                .expect_err("new-project auto-registration cannot reset eviction history"),
            "SESSION_IDENTITY_MISMATCH"
        );
        assert_eq!(
            handshake_counts(&cx).await,
            before,
            "even the source project must not be created before saturated admission refuses"
        );
        let fresh = McpContext::with_state(cx.clone(), 3, SessionState::new());
        let result = handshake(&fresh, &source, "BlueLake", &target_project, &target, true)
            .await
            .expect("fresh session retains implicit registration on the same new-project path");
        assert_eq!(result["response"]["approved"], true);
        assert_eq!(handshake_counts(&cx).await[0], before[0] + 1);
    });
}
