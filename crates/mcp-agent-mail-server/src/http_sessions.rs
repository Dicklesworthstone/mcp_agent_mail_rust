//! Opt-in stateful MCP sessions over Streamable HTTP (GH#279).
//!
//! The HTTP transport is stateless by default: every request is dispatched
//! with a fresh `SessionState`. When `MESSAGING_SESSION_IDENTITY` is on, a
//! successful `initialize` mints an `Mcp-Session-Id` and the server keeps that
//! session's `SessionState` here, so a later request carrying the header —
//! over any HTTP connection — sees the same session state. Tools use it to
//! hold the agent identities the session established (see
//! `mcp_agent_mail_tools::session_identity`).
//!
//! This is application identity context inside an already-authorized
//! connection, never authentication by session id: every request is still
//! subject to bearer/JWT authorization before it reaches the registry, and a
//! session is bound to the authorization it was created under (its
//! *principal*). A request presenting a session id with a different principal,
//! an unknown id, or an expired id gets no session state (it behaves like a
//! stateless request). Sessions live in memory only, so a server restart ends
//! every binding.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fastmcp_core::SessionState;
use sha2::{Digest, Sha256};

/// HTTP header carrying the MCP session id (MCP Streamable HTTP transport).
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Upper bound on live sessions; the least recently used is evicted first.
const MAX_SESSIONS: usize = 4096;

/// A session unused for this long is forgotten.
const SESSION_IDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Longest session id accepted from a client.
const MAX_SESSION_ID_LEN: usize = 128;

struct HttpSession {
    state: SessionState,
    principal: [u8; 32],
    last_seen: Instant,
}

/// Live HTTP sessions, keyed by `Mcp-Session-Id`.
pub struct HttpSessionRegistry {
    sessions: Mutex<HashMap<String, HttpSession>>,
    max_sessions: usize,
    idle_ttl: Duration,
}

impl Default for HttpSessionRegistry {
    fn default() -> Self {
        Self::with_limits(MAX_SESSIONS, SESSION_IDLE_TTL)
    }
}

/// The principal a session is bound to: a digest of the request's
/// `Authorization` header (empty when the server runs without auth). The raw
/// credential is never stored.
#[must_use]
pub fn principal_digest(authorization: Option<&str>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"mcp-agent-mail http session principal v1\0");
    hasher.update(authorization.unwrap_or_default().trim().as_bytes());
    hasher.finalize().into()
}

impl HttpSessionRegistry {
    #[must_use]
    pub fn with_limits(max_sessions: usize, idle_ttl: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            max_sessions: max_sessions.max(1),
            idle_ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, HttpSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start a session for `principal`. Returns its id and state, or `None`
    /// when no random id could be generated (the request then proceeds
    /// statelessly).
    pub fn create(&self, principal: [u8; 32]) -> Option<(String, SessionState)> {
        let id = match mcp_agent_mail_core::setup::generate_registration_token() {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(error = %error, "cannot mint an MCP session id; continuing statelessly");
                return None;
            }
        };
        let now = Instant::now();
        let state = SessionState::new();
        let mut sessions = self.lock();
        sessions.retain(|_, session| now.duration_since(session.last_seen) < self.idle_ttl);
        while sessions.len() >= self.max_sessions {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, session)| session.last_seen)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            sessions.remove(&oldest);
        }
        sessions.insert(
            id.clone(),
            HttpSession {
                state: state.clone(),
                principal,
                last_seen: now,
            },
        );
        Some((id, state))
    }

    /// The live state of session `id` when it was created under `principal`.
    pub fn lookup(&self, id: &str, principal: [u8; 32]) -> Option<SessionState> {
        let id = id.trim();
        if id.is_empty() || id.len() > MAX_SESSION_ID_LEN {
            return None;
        }
        let now = Instant::now();
        let mut sessions = self.lock();
        let session = sessions.get_mut(id)?;
        if now.duration_since(session.last_seen) >= self.idle_ttl {
            sessions.remove(id);
            return None;
        }
        if session.principal != principal {
            return None;
        }
        session.last_seen = now;
        Some(session.state.clone())
    }

    /// End session `id` (HTTP `DELETE`). Only its own principal may end it.
    pub fn terminate(&self, id: &str, principal: [u8; 32]) -> bool {
        let mut sessions = self.lock();
        if sessions
            .get(id.trim())
            .is_some_and(|session| session.principal == principal)
        {
            sessions.remove(id.trim());
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_share_state_only_with_their_own_principal() {
        let registry = HttpSessionRegistry::default();
        let alice = principal_digest(Some("Bearer alice"));
        let bob = principal_digest(Some("Bearer bob"));
        let (id, state) = registry.create(alice).expect("session");
        state.set("k", 7_i64);

        let again = registry.lookup(&id, alice).expect("same principal");
        assert_eq!(again.get::<i64>("k"), Some(7));
        assert!(registry.lookup(&id, bob).is_none(), "other principal");
        assert!(registry.lookup("unknown", alice).is_none());
        assert!(registry.lookup("", alice).is_none());

        assert!(!registry.terminate(&id, bob), "only the owner may end it");
        assert!(registry.terminate(&id, alice));
        assert!(registry.lookup(&id, alice).is_none(), "terminated");
    }

    #[test]
    fn sessions_expire_and_are_bounded() {
        let principal = principal_digest(None);
        let expired = HttpSessionRegistry::with_limits(8, Duration::ZERO);
        let (id, _) = expired.create(principal).expect("session");
        assert!(expired.lookup(&id, principal).is_none(), "idle TTL elapsed");

        let bounded = HttpSessionRegistry::with_limits(2, Duration::from_secs(60));
        let (first, _) = bounded.create(principal).expect("first");
        let (second, _) = bounded.create(principal).expect("second");
        let (third, _) = bounded.create(principal).expect("third");
        assert_eq!(bounded.len(), 2);
        assert!(bounded.lookup(&first, principal).is_none(), "LRU evicted");
        assert!(bounded.lookup(&second, principal).is_some());
        assert!(bounded.lookup(&third, principal).is_some());
    }
}
