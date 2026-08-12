//! Client-supplied idempotency keys for mutating tool calls
//! (br-idempotency-keys-mutating-tools-h0x9k).
//!
//! # Why
//!
//! Field evidence (br-hpv61): MCP tool calls time out at the client's 30 s
//! JSON-RPC deadline while the underlying write *commits anyway* — the message
//! row lands, `send_message` writes land silently. The client cannot tell
//! failure from success, so a "safe" retry double-applies the write (a second
//! message, a second reservation). The only workaround today is to read raw
//! SQLite before retrying, which defeats the API.
//!
//! A client-supplied `idempotency_key` closes this: the key, a fingerprint of
//! the normalized arguments, and the serialized original result are recorded
//! **inside the same database transaction as the mutation itself** (see the
//! `*_idempotent` entry points in [`crate::queries`]). Because the key record
//! and the mutation commit atomically, a crash can never admit a duplicate —
//! either both landed (a retry replays the stored result) or neither did (a
//! retry applies cleanly).
//!
//! # Semantics
//!
//! - **Fresh key** → the mutation runs, the result + fingerprint are recorded,
//!   and [`IdempotentOutcome::Fresh`] is returned.
//! - **Key hit, matching fingerprint** → nothing is applied, the stored result
//!   is returned verbatim as [`IdempotentOutcome::Replayed`] (the caller marks
//!   the response replayed and must NOT re-dispatch archive side effects).
//! - **Key hit, different fingerprint** → nothing is applied and
//!   [`IdempotentOutcome::Conflict`] is returned so the tool layer can surface a
//!   typed, machine-readable conflict error. The write is never silently applied
//!   under either payload.
//!
//! Keys are scoped per `(project_id, tool)` so unrelated tools cannot collide;
//! omitting the key preserves today's behavior exactly (the `*_idempotent`
//! entry points are only taken when a key is supplied).
//!
//! # Retention
//!
//! Records are retained for a configurable window (default 24 h, ample against
//! any sane client retry policy over a 30 s deadline) via `expires_ts`. Expired
//! rows are pruned opportunistically inside the same transaction on every check,
//! so the table stays bounded without a background sweeper and an expired key is
//! correctly treated as fresh.

/// Default idempotency-key retention window, in seconds (24 hours).
///
/// Covers any sane client retry policy against a 30 s JSON-RPC deadline while
/// keeping the `idempotency_keys` table bounded.
pub const DEFAULT_IDEMPOTENCY_RETENTION_SECS: i64 = 24 * 60 * 60;

/// Environment override for the retention window, in seconds.
///
/// A non-positive or unparseable value falls back to
/// [`DEFAULT_IDEMPOTENCY_RETENTION_SECS`] so an override can never disable
/// retention (which would let a legitimate in-window retry double-apply).
pub const IDEMPOTENCY_RETENTION_ENV: &str = "AM_IDEMPOTENCY_RETENTION_SECS";

/// Resolve the effective idempotency-key retention window, in seconds.
///
/// Reads [`IDEMPOTENCY_RETENTION_ENV`]; non-positive or unparseable overrides
/// fall back to [`DEFAULT_IDEMPOTENCY_RETENTION_SECS`].
#[must_use]
pub fn idempotency_retention_secs() -> i64 {
    parse_retention_secs(std::env::var(IDEMPOTENCY_RETENTION_ENV).ok().as_deref())
}

/// Pure parse of a retention override value (the testable core, so unit tests
/// never mutate process env — `std::env::set_var` is `unsafe` under edition 2024
/// and this crate `#![forbid(unsafe_code)]`).
///
/// A non-positive or unparseable value falls back to
/// [`DEFAULT_IDEMPOTENCY_RETENTION_SECS`] so an override can never disable
/// retention (which would let a legitimate in-window retry double-apply).
#[must_use]
fn parse_retention_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_IDEMPOTENCY_RETENTION_SECS)
}

/// A client's idempotency claim for a single mutating tool call.
///
/// `project_id` + `tool` scope the key; `key` is the opaque client-supplied
/// token; `fingerprint` is an opaque hash of the normalized arguments computed
/// by the tool layer (the database layer only stores and compares it, never
/// interprets it).
#[derive(Debug, Clone, Copy)]
pub struct IdempotencyClaim<'a> {
    /// Project the call belongs to (scope component).
    pub project_id: i64,
    /// Canonical tool name, e.g. `"send_message"` (scope component).
    pub tool: &'a str,
    /// Opaque client-supplied idempotency key.
    pub key: &'a str,
    /// Opaque fingerprint of the normalized request arguments.
    pub fingerprint: &'a str,
}

/// Details of an idempotency-key conflict: the same key reused with a payload
/// whose fingerprint differs from the original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyConflict {
    /// Canonical tool name the key was scoped to.
    pub tool: String,
    /// The conflicting idempotency key.
    pub key: String,
    /// Fingerprint recorded when the key was first used.
    pub original_fingerprint: String,
    /// Fingerprint of the current (rejected) request.
    pub attempted_fingerprint: String,
    /// Microsecond timestamp the original request was recorded.
    pub original_created_ts: i64,
}

/// Outcome of a mutating call that carried an idempotency key.
///
/// `Conflict` is deliberately a *successful* outcome variant rather than an
/// error: it never flows through the MVCC-retry / corruption-breaker machinery
/// (a differing payload is a client mistake, not a database fault), and the
/// tool layer maps it to a typed, machine-readable JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotentOutcome<T> {
    /// The key had not been seen (within its window); the mutation was applied
    /// and its result recorded. `T` is the freshly produced result.
    Fresh(T),
    /// The key was already recorded with a matching fingerprint; nothing was
    /// applied. `T` is the stored original result, replayed verbatim.
    Replayed(T),
    /// The key was already recorded with a DIFFERENT fingerprint; nothing was
    /// applied.
    Conflict(IdempotencyConflict),
}

impl<T> IdempotentOutcome<T> {
    /// Whether this outcome replayed a previously stored result.
    #[must_use]
    pub const fn is_replayed(&self) -> bool {
        matches!(self, Self::Replayed(_))
    }

    /// Whether this outcome is an idempotency conflict.
    #[must_use]
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict(_))
    }

    /// The successful result (fresh or replayed), if any.
    #[must_use]
    pub fn result(&self) -> Option<&T> {
        match self {
            Self::Fresh(v) | Self::Replayed(v) => Some(v),
            Self::Conflict(_) => None,
        }
    }
}

/// Internal result of the in-transaction idempotency check.
///
/// Produced by the check step woven into a mutation transaction; drives whether
/// the mutation proceeds, short-circuits to a replay, or aborts as a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdempotencyCheck {
    /// Key absent (or expired) — proceed with the mutation and record the key.
    Proceed,
    /// Key present with a matching fingerprint — replay the stored result JSON.
    Replay(String),
    /// Key present with a different fingerprint — abort as a conflict.
    Conflict(IdempotencyConflict),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_parse_defaults_and_overrides() {
        // Test the pure parser directly — no process-env mutation, so this stays
        // within `#![forbid(unsafe_code)]` and never races parallel tests.
        assert_eq!(DEFAULT_IDEMPOTENCY_RETENTION_SECS, 86_400);
        // Missing override -> default.
        assert_eq!(parse_retention_secs(None), DEFAULT_IDEMPOTENCY_RETENTION_SECS);
        // A valid positive override is honored (trimmed).
        assert_eq!(parse_retention_secs(Some("3600")), 3600);
        assert_eq!(parse_retention_secs(Some("  7200  ")), 7200);
        // Non-positive / unparseable falls back to the default (never disables
        // retention, which would let a legitimate in-window retry double-apply).
        assert_eq!(
            parse_retention_secs(Some("0")),
            DEFAULT_IDEMPOTENCY_RETENTION_SECS
        );
        assert_eq!(
            parse_retention_secs(Some("-5")),
            DEFAULT_IDEMPOTENCY_RETENTION_SECS
        );
        assert_eq!(
            parse_retention_secs(Some("not-a-number")),
            DEFAULT_IDEMPOTENCY_RETENTION_SECS
        );
        assert_eq!(
            parse_retention_secs(Some("")),
            DEFAULT_IDEMPOTENCY_RETENTION_SECS
        );
    }

    #[test]
    fn outcome_accessors() {
        let fresh: IdempotentOutcome<i32> = IdempotentOutcome::Fresh(7);
        assert!(!fresh.is_replayed());
        assert!(!fresh.is_conflict());
        assert_eq!(fresh.result(), Some(&7));

        let replayed: IdempotentOutcome<i32> = IdempotentOutcome::Replayed(7);
        assert!(replayed.is_replayed());
        assert_eq!(replayed.result(), Some(&7));

        let conflict: IdempotentOutcome<i32> = IdempotentOutcome::Conflict(IdempotencyConflict {
            tool: "send_message".to_string(),
            key: "k1".to_string(),
            original_fingerprint: "aaa".to_string(),
            attempted_fingerprint: "bbb".to_string(),
            original_created_ts: 123,
        });
        assert!(conflict.is_conflict());
        assert!(!conflict.is_replayed());
        assert_eq!(conflict.result(), None);
    }
}
