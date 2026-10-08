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
//!   trusted-local behavior.
//! - **Contact actors.** Requests act as the requester; approvals and blocks
//!   act as the recipient. An auto-accepting handshake preflights both actors
//!   before creating or refreshing a link. A bound session must explicitly
//!   create or register additional requester identities before using them;
//!   implicit contact registration never establishes a session binding.
//! - **Lifetime.** Bindings live in session memory: ending the session,
//!   letting it expire or restarting the server ends them; message history is
//!   untouched. Retiring or deregistering an agent drops it from the calling
//!   session.
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

/// Session-state key holding this session's bindings.
const SESSION_IDENTITY_STATE_KEY: &str = "mcp_agent_mail.session_identity.v1";

/// Most identities one session may hold; the oldest binding is dropped first.
const MAX_SESSION_BINDINGS: usize = 32;

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

/// Whether session-bound identity is enabled on this server.
#[must_use]
pub fn enabled() -> bool {
    Config::get().messaging_session_identity
}

fn bindings(ctx: &McpContext) -> Vec<SessionBinding> {
    ctx.get_state::<Vec<SessionBinding>>(SESSION_IDENTITY_STATE_KEY)
        .unwrap_or_default()
}

fn update_bindings(
    ctx: &McpContext,
    update: impl FnOnce(&mut Vec<SessionBinding>) -> bool,
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
    let mut held = bindings(ctx);
    if !update(&mut held) {
        return true;
    }
    #[cfg(test)]
    tests::update_checkpoint(tests::UpdateCheckpoint::BeforeWrite);
    ctx.set_state(SESSION_IDENTITY_STATE_KEY, held)
}

/// The identities this session holds, or none when the feature is off.
#[must_use]
pub fn session_bindings(ctx: &McpContext) -> Vec<SessionBinding> {
    if enabled() { bindings(ctx) } else { Vec::new() }
}

/// Bind `agent_id` to this session. A no-op when the feature is off or the
/// call has no session state.
pub fn bind(ctx: &McpContext, project_id: i64, agent_id: i64, agent_name: &str) {
    if !enabled() || !ctx.has_session_state() || agent_id <= 0 {
        return;
    }
    let stored = update_bindings(ctx, |held| {
        held.retain(|binding| binding.agent_id != agent_id);
        held.push(SessionBinding {
            project_id,
            agent_id,
            agent_name: agent_name.to_string(),
            bound_at_us: mcp_agent_mail_db::now_micros(),
        });
        if held.len() > MAX_SESSION_BINDINGS {
            let excess = held.len() - MAX_SESSION_BINDINGS;
            held.drain(..excess);
        }
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
    update_bindings(ctx, |held| {
        let before = held.len();
        held.retain(|binding| binding.agent_id != agent_id);
        held.len() != before
    });
}

/// Whether this session holds `agent_id`.
#[must_use]
pub fn holds(ctx: &McpContext, agent_id: Option<i64>) -> bool {
    agent_id.is_some_and(|agent_id| {
        session_bindings(ctx)
            .iter()
            .any(|binding| binding.agent_id == agent_id)
    })
}

/// Refuse acting as `agent` when this session holds another identity there.
///
/// "There" is the agent's project. `token_verified` is true when the call
/// presented the agent's own registration token, which always authorizes
/// acting as it.
pub fn authorize_actor(
    ctx: &McpContext,
    agent: &mcp_agent_mail_db::AgentRow,
    token_verified: bool,
    action: &str,
) -> McpResult<()> {
    if token_verified {
        return Ok(());
    }
    let held: Vec<SessionBinding> = session_bindings(ctx)
        .into_iter()
        .filter(|binding| binding.project_id == agent.project_id)
        .collect();
    if held.is_empty()
        || held
            .iter()
            .any(|binding| Some(binding.agent_id) == agent.id)
    {
        return Ok(());
    }
    let names: Vec<&str> = held
        .iter()
        .map(|binding| binding.agent_name.as_str())
        .collect();
    Err(legacy_tool_error(
        "SESSION_IDENTITY_MISMATCH",
        format!(
            "This MCP session holds the identity of {} in this project, so it cannot {action} \
             as '{}'. Act as your own agent, present '{}''s registration_token where the tool \
             accepts one, or use a separate session for that agent.",
            names.join(", "),
            agent.name,
            agent.name
        ),
        false,
        json!({
            "agent": agent.name,
            "session_agents": names,
            "action": action,
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
            assert!(bindings(&ctx).is_empty(), "nothing was stored");
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
