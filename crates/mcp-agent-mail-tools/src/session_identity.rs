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
//!   acknowledging mail, and reserving or releasing files as any other agent
//!   of that project is refused with `SESSION_IDENTITY_MISMATCH` unless the
//!   call presents that agent's registration token. `register_agent` of
//!   another existing agent is refused too, so its profile and token are not
//!   rewritten. A session that holds no identity in the project keeps the
//!   trusted-local behavior.
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
    let mut held = bindings(ctx);
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
    if !ctx.set_state(SESSION_IDENTITY_STATE_KEY, held) {
        tracing::warn!(agent_id, "could not record the session identity binding");
    }
}

/// Drop `agent_id` from this session (retire / deregister).
pub fn unbind(ctx: &McpContext, agent_id: i64) {
    if !enabled() || !ctx.has_session_state() {
        return;
    }
    let mut held = bindings(ctx);
    let before = held.len();
    held.retain(|binding| binding.agent_id != agent_id);
    if held.len() != before {
        ctx.set_state(SESSION_IDENTITY_STATE_KEY, held);
    }
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
    use super::*;

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
}
