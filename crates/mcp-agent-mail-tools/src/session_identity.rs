//! Opt-in MCP session-bound agent identity (GH#279).
//!
//! With `MESSAGING_SESSION_IDENTITY` on, an MCP session remembers the agent
//! identities it established, so it can act as them without a registration
//! token in the model-visible transcript. The session is one stdio process, or
//! one Streamable HTTP session named by `Mcp-Session-Id` (the server keeps its
//! state across requests and connections; see the server's `http_sessions`).
//!
//! # Contract
//!
//! - **Binding.** `create_agent_identity` binds the identity it creates.
//!   `register_agent` binds an agent it created, or one this session already
//!   holds. Naming an existing agent never binds it: an arbitrary name,
//!   project key, pane id or header establishes nothing.
//! - **Verified sender.** A bound agent sends and replies with
//!   `verified_sender: true` and `sender_verification: "session"`, without a
//!   `sender_token`. Under `MESSAGING_FAIL_CLOSED_SEND_PROFILE` the binding is
//!   accepted as proof in place of the token.
//! - **No borrowed names.** A session that holds an identity in a project may
//!   only act as the agents it holds there: sending, replying, reading and
//!   acknowledging mail, managing contacts and contact policy, and reserving
//!   or releasing files as any other agent of that project is refused with
//!   `SESSION_IDENTITY_MISMATCH` unless the call presents that agent's
//!   registration token. `register_agent` of
//!   another existing agent is refused too, so its profile and token are not
//!   rewritten. A session that holds no identity in the project keeps the
//!   trusted-local behavior, unless its last binding there was evicted by the
//!   session's capacity limit. Automatic eviction withdraws proof, never the
//!   project's identity restriction.
//! - **Contact actors.** Requests act as the requester; approvals and blocks
//!   act as the recipient. An auto-accepting handshake preflights both actors
//!   before creating or refreshing a link. A bound session must explicitly
//!   create or register additional requester identities before using them;
//!   implicit contact registration never establishes a session binding.
//! - **Lifetime.** Bindings live in session memory: ending the session,
//!   letting it expire or restarting the server ends them; message history is
//!   untouched. Retiring or deregistering an agent drops it from the calling
//!   session.
//! - **Capacity.** Keep at most 32 bindings and 32 evicted-project restrictions.
//!   If eviction history exceeds that bound, disable trusted-local fallback
//!   for the session instead of forgetting restrictions. Held identities and
//!   verified registration tokens still work; a fresh session resets history.
//!
//! This is application identity context inside an already-authorized
//! connection. It does not authenticate the model, the provider or the OS
//! process, and it cannot isolate hostile clients that share one bearer
//! credential and can read each other's session ids.

use fastmcp::prelude::*;
use mcp_agent_mail_core::Config;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::tool_util::legacy_tool_error;

/// One atomic session-state value holds both proofs and eviction restrictions.
/// This state is process-local: server restart already ends every session.
const SESSION_IDENTITY_STATE_KEY: &str = "mcp_agent_mail.session_identity.v2";

/// Most identities one session may hold; the oldest binding is dropped first.
const MAX_SESSION_BINDINGS: usize = 32;
const MAX_EVICTED_PROJECTS: usize = 32;

// FastMCP locks individual get/set operations, but exposes no atomic update.
// Serialize this key's two writers across their bounded read/modify/write;
// otherwise parallel requests can lose a project's last binding or restore a
// retired identity. A single lock avoids a second session-lifetime registry.
// Authorization reads, database work, awaits, and logging never hold this lock.
static BINDINGS_UPDATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One agent identity held by the session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionBinding {
    pub project_id: i64,
    pub agent_id: i64,
    pub agent_name: String,
    pub bound_at_us: i64,
}

impl SessionBinding {
    /// Compare the resolved canonical row, not a caller alias or only its id.
    /// Recovery can reuse an id for a different name; that is not the identity
    /// this session established. This does not identify database generations
    /// whose project, id and canonical name are all unchanged.
    fn matches_agent(&self, agent: &mcp_agent_mail_db::AgentRow) -> bool {
        self.project_id > 0
            && self.agent_id > 0
            && !self.agent_name.is_empty()
            && agent.project_id == self.project_id
            && agent.id == Some(self.agent_id)
            && agent.name == self.agent_name
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct SessionIdentityState {
    held: Vec<SessionBinding>,
    // Only projects whose final proof was automatically evicted need a marker.
    // Explicit unbind keeps its existing semantics, but cannot erase a marker.
    evicted_projects: Vec<i64>,
    eviction_history_saturated: bool,
}

impl SessionIdentityState {
    fn restrict_evicted_project(&mut self, project_id: i64) {
        if self.eviction_history_saturated || self.evicted_projects.contains(&project_id) {
            return;
        }
        if self.evicted_projects.len() >= MAX_EVICTED_PROJECTS {
            // Finite memory must not turn an old constrained project into an
            // unconstrained one. This bit is monotonic for the session lifetime.
            self.eviction_history_saturated = true;
            self.evicted_projects.clear();
        } else {
            self.evicted_projects.push(project_id);
        }
    }

    fn enforce_capacity(&mut self) {
        while self.held.len() > MAX_SESSION_BINDINGS {
            let evicted = self.held.remove(0);
            if !self
                .held
                .iter()
                .any(|binding| binding.project_id == evicted.project_id)
            {
                self.restrict_evicted_project(evicted.project_id);
            }
        }
    }

    fn eviction_restricts(&self, project_id: i64) -> bool {
        self.eviction_history_saturated || self.evicted_projects.contains(&project_id)
    }

    fn permits_unresolved_actor(&self, project_id: Option<i64>) -> bool {
        !self.eviction_history_saturated
            && !self
                .evicted_projects
                .iter()
                .any(|project| project_id.is_none_or(|id| *project == id))
            && !self
                .held
                .iter()
                .any(|binding| project_id.is_none_or(|id| binding.project_id == id))
    }
}

/// Whether session-bound identity is enabled on this server.
#[must_use]
pub fn enabled() -> bool {
    Config::get().messaging_session_identity
}

fn identity_state(ctx: &McpContext) -> SessionIdentityState {
    ctx.get_state::<SessionIdentityState>(SESSION_IDENTITY_STATE_KEY)
        .unwrap_or_default()
}

fn update_bindings(
    ctx: &McpContext,
    update: impl FnOnce(&mut SessionIdentityState) -> bool,
) -> bool {
    #[cfg(test)]
    if matches!(
        BINDINGS_UPDATE_LOCK.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ) {
        tests::update_checkpoint(tests::UpdateCheckpoint::Contended);
    }
    let _guard = BINDINGS_UPDATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = identity_state(ctx);
    if !update(&mut state) {
        return true;
    }
    #[cfg(test)]
    tests::update_checkpoint(tests::UpdateCheckpoint::BeforeWrite);
    ctx.set_state(SESSION_IDENTITY_STATE_KEY, state)
}

/// The identities this session holds, or none when the feature is off.
#[must_use]
pub fn session_bindings(ctx: &McpContext) -> Vec<SessionBinding> {
    if enabled() {
        identity_state(ctx).held
    } else {
        Vec::new()
    }
}

/// Bind `agent_id` to this session. A no-op when the feature is off or the
/// call has no session state.
pub fn bind(ctx: &McpContext, project_id: i64, agent_id: i64, agent_name: &str) {
    if !enabled() || !ctx.has_session_state() || agent_id <= 0 {
        return;
    }
    let stored = update_bindings(ctx, |state| {
        state.held.retain(|binding| binding.agent_id != agent_id);
        state.held.push(SessionBinding {
            project_id,
            agent_id,
            agent_name: agent_name.to_string(),
            bound_at_us: mcp_agent_mail_db::now_micros(),
        });
        state.enforce_capacity();
        true
    });
    if !stored {
        tracing::warn!(agent_id, "could not record the session identity binding");
    }
}

/// Drop `agent_id` from this session (retire / deregister).
pub fn unbind(ctx: &McpContext, agent_id: i64) {
    if !enabled() || !ctx.has_session_state() {
        return;
    }
    update_bindings(ctx, |state| {
        let before = state.held.len();
        state.held.retain(|binding| binding.agent_id != agent_id);
        state.held.len() != before
    });
}

/// Whether the live session's binding inventory contains `agent_id`.
/// This id-only lookup does not establish a canonical-row match.
#[must_use]
pub fn holds(ctx: &McpContext, agent_id: Option<i64>) -> bool {
    let Ok(snapshot) = ActorAuthorizationSnapshot::capture(ctx) else {
        return false;
    };
    let held = agent_id.is_some_and(|agent_id| {
        snapshot
            .state
            .held
            .iter()
            .any(|binding| binding.agent_id == agent_id)
    });
    held && ctx.ensure_live().is_ok()
}

/// Keep deferred mutation admission and its eventual actor check on the same
/// identity snapshot. An outage must not turn an unresolved constrained actor
/// into a trusted-local durable intent that can run after this session ends.
pub(crate) struct ActorAuthorizationSnapshot {
    state: SessionIdentityState,
}

impl ActorAuthorizationSnapshot {
    pub(crate) fn capture(ctx: &McpContext) -> McpResult<Self> {
        ctx.ensure_live()?;
        let state = if enabled() {
            identity_state(ctx)
        } else {
            SessionIdentityState::default()
        };
        // An expired request scope makes get_state return None. Never mistake
        // that unavailable state for an intentionally unbound caller.
        ctx.ensure_live()?;
        Ok(Self { state })
    }

    /// Before resolving the project, any binding or eviction could constrain
    /// the call. Unknown actors never receive proof from eviction history.
    pub(crate) fn permits_unresolved_actor(&self, project_id: Option<i64>) -> bool {
        self.state.permits_unresolved_actor(project_id)
    }

    pub(crate) fn authorize(
        &self,
        ctx: &McpContext,
        agent: &mcp_agent_mail_db::AgentRow,
        action: &str,
    ) -> McpResult<()> {
        ctx.ensure_live()?;
        authorize_actor_from_state(&self.state, agent, action)
    }
}

/// Refuse acting as `agent` when this session holds another identity there.
///
/// "There" is the agent's project. `token_verified` is true when the call
/// presented the agent's own registration token, which authorizes acting as
/// it only while the request is live.
pub fn authorize_actor(
    ctx: &McpContext,
    agent: &mcp_agent_mail_db::AgentRow,
    token_verified: bool,
    action: &str,
) -> McpResult<()> {
    ctx.ensure_live()?;
    if token_verified {
        return Ok(());
    }
    // Use the same fail-closed snapshot boundary as deferred mutations.
    // get_state may return None after cancellation; that is not evidence of
    // an unbound, trusted-local session, even when this feature is disabled.
    ActorAuthorizationSnapshot::capture(ctx)?.authorize(ctx, agent, action)
}

fn authorize_actor_from_state(
    state: &SessionIdentityState,
    agent: &mcp_agent_mail_db::AgentRow,
    action: &str,
) -> McpResult<()> {
    let held: Vec<&SessionBinding> = state
        .held
        .iter()
        .filter(|binding| binding.project_id == agent.project_id)
        .collect();
    if (held.is_empty() && !state.eviction_restricts(agent.project_id))
        || held
            .iter()
            .any(|binding| binding.matches_agent(agent))
    {
        return Ok(());
    }
    let names: Vec<&str> = held
        .iter()
        .map(|binding| binding.agent_name.as_str())
        .collect();
    let restriction = if state.eviction_history_saturated {
        "eviction_history_saturated"
    } else if names.is_empty() {
        "binding_evicted"
    } else {
        "bound_to_other_agent"
    };
    let explanation = if names.is_empty() {
        "This MCP session lost identity proof through capacity eviction; its project restrictions remain"
            .to_string()
    } else {
        format!(
            "This MCP session holds the identity of {} in this project",
            names.join(", ")
        )
    };
    Err(legacy_tool_error(
        "SESSION_IDENTITY_MISMATCH",
        format!(
            "{explanation}, so it cannot {action} \
             as '{}'. Act as your own agent, present '{}''s registration_token where the tool \
             accepts one, or use a separate session for that agent.",
            agent.name, agent.name
        ),
        false,
        json!({
            "agent": agent.name,
            "session_agents": names,
            "action": action,
            "restriction": restriction,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum UpdateCheckpoint {
        Contended,
        BeforeWrite,
    }

    type UpdateHook = Box<dyn FnMut(UpdateCheckpoint)>;

    thread_local! {
        static UPDATE_HOOK: RefCell<Option<UpdateHook>> = const { RefCell::new(None) };
    }

    pub(super) fn update_checkpoint(checkpoint: UpdateCheckpoint) {
        UPDATE_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(checkpoint);
            }
        });
    }

    // Stop one actual writer immediately before publication, then require a
    // second actual writer to contend before releasing the first. Channels
    // select the interleaving; timeouts only bound a broken test's deadlock.
    fn interleave_updates(
        state: fastmcp_core::SessionState,
        first: impl FnOnce(&McpContext) + Send,
        second: impl FnOnce(&McpContext) + Send,
    ) {
        std::thread::scope(|scope| {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel::<()>();
            let first_state = state.clone();
            let first_writer = scope.spawn(move || {
                UPDATE_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |checkpoint| {
                        if checkpoint == UpdateCheckpoint::BeforeWrite {
                            ready_tx.send(()).expect("announce first writer");
                            let _ = resume_rx.recv();
                        }
                    }));
                });
                let ctx = McpContext::with_state(asupersync::Cx::for_testing(), 1, first_state);
                first(&ctx);
            });
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("first writer reached publication");

            let (checkpoint_tx, checkpoint_rx) = mpsc::channel();
            let second_writer = scope.spawn(move || {
                UPDATE_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |checkpoint| {
                        checkpoint_tx
                            .send(checkpoint)
                            .expect("announce second writer");
                    }));
                });
                let ctx = McpContext::with_state(asupersync::Cx::for_testing(), 2, state);
                second(&ctx);
            });
            let checkpoint = checkpoint_rx.recv_timeout(Duration::from_secs(5));
            // Release even on a missing or incorrect checkpoint, so a failing
            // assertion cannot strand either scoped writer.
            drop(resume_tx);
            first_writer.join().expect("first writer completed");
            second_writer.join().expect("second writer completed");
            assert_eq!(
                checkpoint.expect("second writer reached the update"),
                UpdateCheckpoint::Contended,
                "the second writer must not read a stale binding vector"
            );
        });
    }

    fn agent(id: i64, project_id: i64, name: &str) -> mcp_agent_mail_db::AgentRow {
        mcp_agent_mail_db::AgentRow {
            id: Some(id),
            project_id,
            name: name.to_string(),
            ..mcp_agent_mail_db::AgentRow::default()
        }
    }

    fn with_feature<T>(enabled: bool, f: impl FnOnce() -> T) -> T {
        let value = if enabled { "true" } else { "false" };
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("MESSAGING_SESSION_IDENTITY", value)],
            || {
                Config::reset_cached();
                let out = f();
                Config::reset_cached();
                out
            },
        )
    }

    fn session_ctx() -> McpContext {
        McpContext::with_state(
            asupersync::Cx::for_testing(),
            1,
            fastmcp_core::SessionState::new(),
        )
    }

    #[test]
    fn canonical_binding_match_requires_every_identity_component() {
        let binding = SessionBinding {
            project_id: 10,
            agent_id: 1,
            agent_name: "BlueLake".to_string(),
            bound_at_us: 100,
        };
        let own = agent(1, 10, "BlueLake");
        assert!(binding.matches_agent(&own));
        for other in [
            agent(1, 10, "RedStone"),
            agent(1, 10, "bluelake"),
            agent(1, 20, "BlueLake"),
            agent(2, 10, "BlueLake"),
            agent(0, 10, "BlueLake"),
            mcp_agent_mail_db::AgentRow {
                id: None,
                ..own.clone()
            },
        ] {
            assert!(!binding.matches_agent(&other), "accepted {other:?}");
        }
        for invalid in [
            SessionBinding {
                project_id: 0,
                ..binding.clone()
            },
            SessionBinding {
                agent_id: 0,
                ..binding.clone()
            },
            SessionBinding {
                agent_name: String::new(),
                ..binding.clone()
            },
        ] {
            let row = agent(invalid.agent_id, invalid.project_id, &invalid.agent_name);
            assert!(!invalid.matches_agent(&row), "invalid tuple granted proof");
        }
    }

    #[test]
    fn reused_id_is_refused_by_live_and_captured_actor_authorization() {
        with_feature(true, || {
            let ctx = session_ctx();
            let own = agent(1, 10, "BlueLake");
            bind(&ctx, 10, 1, &own.name);
            let snapshot = ActorAuthorizationSnapshot::capture(&ctx).unwrap();
            for other in [agent(1, 10, "RedStone"), agent(1, 10, "bluelake")] {
                let error = authorize_actor(&ctx, &other, false, "read inbox")
                    .expect_err("a reused id must not borrow the bound identity");
                assert_eq!(
                    crate::tool_error_code(&error),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
                let error = snapshot
                    .authorize(&ctx, &other, "acknowledge mail")
                    .expect_err("deferred actor admission needs the same canonical match");
                assert_eq!(
                    crate::tool_error_code(&error),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
                assert!(authorize_actor(&ctx, &other, true, "read inbox").is_ok());
            }
            assert!(authorize_actor(&ctx, &own, false, "read inbox").is_ok());
            // This policy still permits genuinely unbound projects; permission
            // through that fallback is not a canonical binding match.
            assert!(authorize_actor(&ctx, &agent(2, 20, "RedStone"), false, "read").is_ok());
            unbind(&ctx, 1);
            assert!(
                snapshot
                    .authorize(&ctx, &agent(1, 10, "RedStone"), "ack")
                    .is_err()
            );
            assert!(snapshot.authorize(&ctx, &own, "ack").is_ok());
        });
    }

    #[test]
    fn binding_inventory_cannot_grant_proof_to_a_cancelled_request() {
        with_feature(true, || {
            let state = fastmcp_core::SessionState::new();
            let cancelled = McpContext::with_state(asupersync::Cx::for_testing(), 1, state.clone());
            let live = McpContext::with_state(asupersync::Cx::for_testing(), 2, state);
            bind(&live, 10, 1, "BlueLake");
            assert!(holds(&cancelled, Some(1)));
            cancelled.request_cancellation().cancel();
            assert!(!holds(&cancelled, Some(1)));
            assert!(
                holds(&live, Some(1)),
                "cancellation must not erase another request's proof"
            );
            assert!(!holds(&live, None));
        });
    }

    #[test]
    fn cancelled_actor_checks_never_downgrade_to_trusted_local() {
        for feature_enabled in [false, true] {
            with_feature(feature_enabled, || {
                let ctx = session_ctx();
                bind(&ctx, 10, 1, "BlueLake");
                ctx.request_cancellation().cancel();

                for target in [
                    agent(1, 10, "BlueLake"),
                    agent(2, 10, "RedStone"),
                    agent(3, 20, "GreenCastle"),
                ] {
                    for token_verified in [false, true] {
                        assert!(
                            authorize_actor(&ctx, &target, token_verified, "acknowledge mail")
                                .is_err(),
                            "cancelled request authorized agent {} (identity={feature_enabled}, token={token_verified})",
                            target.name
                        );
                    }
                }
            });
        }
    }

    #[test]
    fn cancelling_one_request_preserves_other_requests_session_authority() {
        with_feature(true, || {
            let state = fastmcp_core::SessionState::new();
            let cancelled = McpContext::with_state(asupersync::Cx::for_testing(), 1, state.clone());
            let live = McpContext::with_state(asupersync::Cx::for_testing(), 2, state);
            let own = agent(1, 10, "BlueLake");
            let other = agent(2, 10, "RedStone");
            bind(&cancelled, 10, 1, &own.name);
            cancelled.request_cancellation().cancel();

            assert!(authorize_actor(&cancelled, &own, false, "send messages").is_err());
            assert!(authorize_actor(&live, &own, false, "send messages").is_ok());
            let error = authorize_actor(&live, &other, false, "send messages")
                .expect_err("the live request must retain its project's restriction");
            assert_eq!(
                crate::tool_error_code(&error),
                Some("SESSION_IDENTITY_MISMATCH")
            );
            assert!(authorize_actor(&live, &other, true, "send messages").is_ok());
        });
    }

    #[test]
    fn deferred_authorization_keeps_its_binding_snapshot_and_checks_liveness() {
        with_feature(true, || {
            let ctx = session_ctx();
            let own = agent(1, 10, "BlueLake");
            let other = agent(2, 10, "RedStone");
            bind(&ctx, 10, 1, &own.name);
            let authorization = ActorAuthorizationSnapshot::capture(&ctx).expect("live snapshot");
            assert!(!authorization.permits_unresolved_actor(None));
            assert!(!authorization.permits_unresolved_actor(Some(10)));
            assert!(authorization.permits_unresolved_actor(Some(20)));
            unbind(&ctx, 1);
            assert_eq!(session_bindings(&ctx), Vec::<SessionBinding>::new());
            authorization
                .authorize(&ctx, &own, "acknowledge mail")
                .expect("original owner");
            let error = authorization
                .authorize(&ctx, &other, "acknowledge mail")
                .expect_err("concurrent unbind cannot downgrade an accepted request");
            assert_eq!(
                crate::tool_error_code(&error),
                Some("SESSION_IDENTITY_MISMATCH")
            );

            ctx.request_cancellation().cancel();
            assert!(
                authorization
                    .authorize(&ctx, &own, "acknowledge mail")
                    .is_err()
            );
            assert!(ActorAuthorizationSnapshot::capture(&ctx).is_err());
        });
    }

    #[test]
    fn bound_sessions_cannot_borrow_other_names_in_their_project() {
        with_feature(true, || {
            let ctx = session_ctx();
            let own = agent(1, 10, "BlueLake");
            let other = agent(2, 10, "RedStone");
            let elsewhere = agent(3, 20, "GreenCastle");

            // Unbound: trusted-local behavior.
            assert!(authorize_actor(&ctx, &other, false, "send messages").is_ok());

            bind(&ctx, 10, 1, "BlueLake");
            assert!(holds(&ctx, Some(1)));
            assert!(authorize_actor(&ctx, &own, false, "send messages").is_ok());
            let error =
                authorize_actor(&ctx, &other, false, "send messages").expect_err("borrowed name");
            assert_eq!(
                crate::tool_util::tool_error_code(&error),
                Some("SESSION_IDENTITY_MISMATCH")
            );
            assert!(
                authorize_actor(&ctx, &other, true, "send messages").is_ok(),
                "the agent's own token always authorizes"
            );
            assert!(
                authorize_actor(&ctx, &elsewhere, false, "send messages").is_ok(),
                "no identity held in that project"
            );

            unbind(&ctx, 1);
            assert!(!holds(&ctx, Some(1)));
            assert!(authorize_actor(&ctx, &other, false, "send messages").is_ok());
        });
    }

    #[test]
    fn disabled_feature_never_binds_or_refuses() {
        with_feature(false, || {
            let ctx = session_ctx();
            bind(&ctx, 10, 1, "BlueLake");
            assert!(!holds(&ctx, Some(1)));
            assert_eq!(identity_state(&ctx), SessionIdentityState::default());
            assert!(authorize_actor(&ctx, &agent(2, 10, "RedStone"), false, "send").is_ok());
        });
    }

    #[test]
    fn bindings_are_bounded() {
        with_feature(true, || {
            let ctx = session_ctx();
            for id in 1..=i64::try_from(MAX_SESSION_BINDINGS + 3).expect("small") {
                bind(&ctx, 10, id, "BlueLake");
                assert!(holds(&ctx, Some(id)));
            }
            let held = session_bindings(&ctx);
            assert_eq!(held.len(), MAX_SESSION_BINDINGS);
            assert!(!holds(&ctx, Some(1)), "oldest binding dropped");
        });
    }

    fn evict_first_project(ctx: &McpContext) {
        bind(ctx, 10, 1, "BlueLake");
        for id in 2..=i64::try_from(MAX_SESSION_BINDINGS + 1).expect("small") {
            bind(ctx, 20, id, "RedStone");
        }
        assert!(
            !holds(ctx, Some(1)),
            "fixture must actually evict the proof"
        );
        assert_eq!(session_bindings(ctx).len(), MAX_SESSION_BINDINGS);
    }

    #[test]
    fn capacity_eviction_cannot_authorize_the_old_project_or_a_deferred_actor() {
        with_feature(true, || {
            let ctx = session_ctx();
            evict_first_project(&ctx);
            for target in [agent(1, 10, "BlueLake"), agent(999, 10, "GreenCastle")] {
                let error = authorize_actor(&ctx, &target, false, "send messages")
                    .expect_err("eviction must not grant trusted-local authority");
                assert_eq!(
                    crate::tool_error_code(&error),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
                assert!(authorize_actor(&ctx, &target, true, "send messages").is_ok());
            }
            assert!(authorize_actor(&ctx, &agent(2, 20, "RedStone"), false, "send").is_ok());
            assert!(authorize_actor(&ctx, &agent(999, 30, "GreenCastle"), false, "send").is_ok());

            let snapshot = ActorAuthorizationSnapshot::capture(&ctx).unwrap();
            assert!(!snapshot.permits_unresolved_actor(None));
            assert!(!snapshot.permits_unresolved_actor(Some(10)));
            assert!(!snapshot.permits_unresolved_actor(Some(20)));
            assert!(snapshot.permits_unresolved_actor(Some(30)));
            assert!(
                snapshot
                    .authorize(&ctx, &agent(1, 10, "BlueLake"), "ack")
                    .is_err()
            );
        });
    }

    #[test]
    fn unbinding_live_proofs_cannot_erase_an_evicted_projects_restriction() {
        with_feature(true, || {
            let ctx = session_ctx();
            evict_first_project(&ctx);
            for binding in session_bindings(&ctx) {
                unbind(&ctx, binding.agent_id);
            }
            unbind(&ctx, 1);
            assert_eq!(session_bindings(&ctx), Vec::new());
            assert_eq!(identity_state(&ctx).evicted_projects, [10]);
            let snapshot = ActorAuthorizationSnapshot::capture(&ctx).unwrap();
            assert!(!snapshot.permits_unresolved_actor(None));
            assert!(!snapshot.permits_unresolved_actor(Some(10)));
            assert!(snapshot.permits_unresolved_actor(Some(20)));
            assert!(authorize_actor(&ctx, &agent(999, 10, "GreenCastle"), false, "read").is_err());
            assert!(
                authorize_actor(
                    &session_ctx(),
                    &agent(999, 10, "GreenCastle"),
                    false,
                    "read"
                )
                .is_ok()
            );
        });
    }

    #[test]
    fn eviction_history_saturates_without_forgetting_authorization_restrictions() {
        with_feature(true, || {
            let ctx = session_ctx();
            let count =
                i64::try_from(MAX_SESSION_BINDINGS + MAX_EVICTED_PROJECTS + 1).expect("small");
            for id in 1..=count {
                bind(&ctx, id * 10, id, "BlueLake");
                let state = identity_state(&ctx);
                assert!(state.held.len() <= MAX_SESSION_BINDINGS);
                assert!(state.evicted_projects.len() <= MAX_EVICTED_PROJECTS);
                if id == count - 1 {
                    assert!(!state.eviction_history_saturated);
                    assert_eq!(state.evicted_projects.len(), MAX_EVICTED_PROJECTS);
                }
            }
            let state = identity_state(&ctx);
            assert!(state.eviction_history_saturated);
            assert_eq!(state.evicted_projects, Vec::<i64>::new());
            for project in [10, count * 10, 99_999] {
                assert!(
                    authorize_actor(&ctx, &agent(999, project, "GreenCastle"), false, "ack")
                        .is_err()
                );
                assert!(
                    authorize_actor(&ctx, &agent(999, project, "GreenCastle"), true, "ack").is_ok()
                );
            }
            for binding in &state.held {
                let own = agent(binding.agent_id, binding.project_id, &binding.agent_name);
                assert!(authorize_actor(&ctx, &own, false, "send").is_ok());
            }
            let snapshot = ActorAuthorizationSnapshot::capture(&ctx).unwrap();
            for project in [None, Some(10), Some(99_999)] {
                assert!(!snapshot.permits_unresolved_actor(project));
            }
            for binding in state.held {
                unbind(&ctx, binding.agent_id);
            }
            assert!(identity_state(&ctx).eviction_history_saturated);
            bind(&ctx, 99_999, 999, "GreenCastle");
            assert!(
                authorize_actor(&ctx, &agent(999, 99_999, "GreenCastle"), false, "send").is_ok()
            );
            ctx.request_cancellation().cancel();
            assert!(
                authorize_actor(&ctx, &agent(999, 99_999, "GreenCastle"), true, "send").is_err()
            );
        });
    }

    #[test]
    fn rebinding_cannot_remove_eviction_history_or_change_an_accepted_snapshot() {
        with_feature(true, || {
            let ctx = session_ctx();
            evict_first_project(&ctx);
            let snapshot = ActorAuthorizationSnapshot::capture(&ctx).unwrap();
            bind(&ctx, 10, 1, "BlueLake");
            assert!(holds(&ctx, Some(1)));
            assert!(authorize_actor(&ctx, &agent(1, 10, "BlueLake"), false, "send").is_ok());
            assert!(
                snapshot
                    .authorize(&ctx, &agent(1, 10, "BlueLake"), "ack")
                    .is_err()
            );
            unbind(&ctx, 1);
            assert_eq!(identity_state(&ctx).evicted_projects, [10]);
            assert!(authorize_actor(&ctx, &agent(999, 10, "GreenCastle"), false, "send").is_err());
        });
    }

    #[test]
    fn same_project_churn_does_not_invent_eviction_history_or_restrict_other_projects() {
        with_feature(true, || {
            let ctx = session_ctx();
            for id in 1..=100 {
                bind(&ctx, 10, id, "BlueLake");
            }
            assert_eq!(identity_state(&ctx).evicted_projects, Vec::<i64>::new());
            assert!(!identity_state(&ctx).eviction_history_saturated);
            assert!(authorize_actor(&ctx, &agent(999, 20, "RedStone"), false, "send").is_ok());
            for binding in session_bindings(&ctx) {
                unbind(&ctx, binding.agent_id);
            }
            assert!(authorize_actor(&ctx, &agent(999, 10, "RedStone"), false, "send").is_ok());
        });
    }

    #[test]
    fn concurrent_eviction_and_unbind_publish_proofs_and_restrictions_together() {
        with_feature(true, || {
            let state = fastmcp_core::SessionState::new();
            let ctx = McpContext::with_state(asupersync::Cx::for_testing(), 3, state.clone());
            bind(&ctx, 10, 1, "BlueLake");
            for id in 2..=i64::try_from(MAX_SESSION_BINDINGS).expect("small") {
                bind(&ctx, 20, id, "RedStone");
            }
            interleave_updates(
                state,
                |ctx| bind(ctx, 30, 999, "GreenCastle"),
                |ctx| unbind(ctx, 999),
            );
            assert!(!holds(&ctx, Some(1)));
            assert!(!holds(&ctx, Some(999)));
            assert_eq!(identity_state(&ctx).evicted_projects, [10]);
            assert!(authorize_actor(&ctx, &agent(1000, 10, "GreenCastle"), false, "send").is_err());
            assert!(
                !ActorAuthorizationSnapshot::capture(&ctx)
                    .unwrap()
                    .permits_unresolved_actor(Some(10))
            );
        });
    }

    #[test]
    fn concurrent_bindings_preserve_each_projects_identity_restriction() {
        with_feature(true, || {
            let state = fastmcp_core::SessionState::new();
            let ctx = McpContext::with_state(asupersync::Cx::for_testing(), 3, state.clone());
            interleave_updates(
                state,
                |ctx| bind(ctx, 10, 1, "BlueLake"),
                |ctx| bind(ctx, 20, 2, "RedStone"),
            );
            assert_eq!(session_bindings(&ctx).len(), 2);
            assert!(holds(&ctx, Some(1)));
            assert!(holds(&ctx, Some(2)));
            for project_id in [10, 20] {
                let error = authorize_actor(
                    &ctx,
                    &agent(3, project_id, "GreenCastle"),
                    false,
                    "send messages",
                )
                .expect_err("neither project may revert to trusted-local behavior");
                assert_eq!(
                    crate::tool_util::tool_error_code(&error),
                    Some("SESSION_IDENTITY_MISMATCH")
                );
            }
        });
    }

    #[test]
    fn concurrent_bind_and_unbind_preserve_the_new_identity_without_resurrection() {
        with_feature(true, || {
            for unbind_first in [false, true] {
                let state = fastmcp_core::SessionState::new();
                let ctx = McpContext::with_state(asupersync::Cx::for_testing(), 3, state.clone());
                bind(&ctx, 10, 1, "BlueLake");
                interleave_updates(
                    state,
                    move |ctx| {
                        if unbind_first {
                            unbind(ctx, 1);
                        } else {
                            bind(ctx, 20, 2, "RedStone");
                        }
                    },
                    move |ctx| {
                        if unbind_first {
                            bind(ctx, 20, 2, "RedStone");
                        } else {
                            unbind(ctx, 1);
                        }
                    },
                );
                let held = session_bindings(&ctx);
                assert_eq!(held.len(), 1, "unbind_first={unbind_first}");
                assert_eq!((held[0].project_id, held[0].agent_id), (20, 2));
                assert!(!holds(&ctx, Some(1)), "retired identity must stay unbound");
                assert!(
                    authorize_actor(&ctx, &agent(3, 20, "GreenCastle"), false, "send").is_err(),
                    "the newly bound project must keep its identity restriction"
                );
            }
        });
    }

    #[test]
    fn concurrent_unbinds_do_not_restore_either_identity() {
        with_feature(true, || {
            let state = fastmcp_core::SessionState::new();
            let ctx = McpContext::with_state(asupersync::Cx::for_testing(), 3, state.clone());
            bind(&ctx, 10, 1, "BlueLake");
            bind(&ctx, 20, 2, "RedStone");
            interleave_updates(state, |ctx| unbind(ctx, 1), |ctx| unbind(ctx, 2));
            assert_eq!(session_bindings(&ctx), Vec::new());
        });
    }
}
