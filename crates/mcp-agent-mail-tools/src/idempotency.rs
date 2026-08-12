//! Tool-layer helpers for client-supplied idempotency keys on mutating tools
//! (br-idempotency-keys-mutating-tools-h0x9k).
//!
//! The durable key-recording layer lives in `mcp_agent_mail_db` (the
//! `*_idempotent` query entry points record the key + a payload fingerprint +
//! the serialized result inside the mutation's own transaction). This module is
//! the thin tool-side glue: it validates the client's key argument, derives the
//! canonical payload fingerprint the DB layer compares, renders the typed
//! conflict error, and marks a replayed response.
//!
//! Contract summary (documented in each tool's description):
//!   - Omitting `idempotency_key` preserves today's behavior exactly.
//!   - A retry with the same key + byte-identical payload replays the original
//!     result (same ids) and sets `"idempotent_replay": true`; no second write
//!     and no second archive bundle are produced.
//!   - The same key with a DIFFERENT payload is rejected with the
//!     `IDEMPOTENCY_KEY_CONFLICT` error.
//!   - Keys are retained for a configurable window (default 24 h) and scoped per
//!     (project, tool).

use crate::tool_util::legacy_tool_error;
use fastmcp::prelude::*;
use mcp_agent_mail_db::IdempotencyConflict;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Maximum accepted idempotency-key length. Keys are opaque client tokens
/// (UUID/ULID/hash sized); the bound keeps the `idempotency_keys` table from
/// storing pathologically large keys.
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// Canonical documentation blurb appended to each mutating tool's description so
/// the retention window + semantics are discoverable by clients.
pub const IDEMPOTENCY_DOC: &str = "\n\nIdempotency\n-----------\nidempotency_key : Optional[str]\n    Client-supplied key that makes this call safe to retry after a timeout. If a\n    previous call with the SAME key and byte-identical arguments already committed\n    (e.g. the write landed but the 30s JSON-RPC deadline elapsed first), the retry\n    replays the original result verbatim (same ids) with \"idempotent_replay\": true\n    and applies nothing — no duplicate message/reservation and no second archive\n    write. Reusing the same key with DIFFERENT arguments is rejected with error\n    type IDEMPOTENCY_KEY_CONFLICT. Keys are scoped per (project, tool) and retained\n    for a configurable window (default 24h; AM_IDEMPOTENCY_RETENTION_SECS). Omit to\n    preserve default behavior.";

/// Normalize a client-supplied idempotency-key argument.
///
/// `None`, or a blank/whitespace-only value, means "no key" — today's behavior
/// is preserved exactly. A non-blank value is trimmed; over-long keys are
/// rejected with a typed `INVALID_ARGUMENT` error.
///
/// # Errors
/// Returns an `INVALID_ARGUMENT` [`McpError`] when the key exceeds
/// [`MAX_IDEMPOTENCY_KEY_LEN`].
pub fn normalize_idempotency_key(raw: Option<&str>) -> Result<Option<String>, McpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(legacy_tool_error(
            "INVALID_ARGUMENT",
            format!(
                "idempotency_key is too long ({} chars; max {MAX_IDEMPOTENCY_KEY_LEN}). \
                 Use a compact token such as a UUID.",
                trimmed.chars().count()
            ),
            false,
            json!({ "field": "idempotency_key", "max_len": MAX_IDEMPOTENCY_KEY_LEN }),
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Compute a stable fingerprint of the normalized arguments of a tool call.
///
/// `fields` is a set of `(name, canonical-value)` pairs. They are sorted by name
/// and hashed with SHA-256, so the fingerprint is deterministic and independent
/// of the order the caller lists fields. Two calls with the same logical payload
/// produce the same fingerprint; any change flips it (driving conflict
/// detection). Multi-valued fields (recipient lists, path sets) must be
/// pre-canonicalized by the caller (e.g. sorted and joined) before being passed.
#[must_use]
pub fn compute_fingerprint(tool: &str, fields: &[(&str, String)]) -> String {
    let mut sorted: Vec<&(&str, String)> = fields.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));

    let mut hasher = Sha256::new();
    hasher.update(tool.as_bytes());
    hasher.update([0u8]);
    for (name, value) in sorted {
        // Length-prefix each part so field boundaries are unambiguous and no
        // pair of distinct field sets can collide by concatenation.
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Build the typed, machine-readable error for an idempotency-key conflict: the
/// same key reused with a payload whose fingerprint differs from the original.
///
/// The error carries `error.type = "IDEMPOTENCY_KEY_CONFLICT"` and both
/// fingerprints so a client can tell it apart from a transient failure and never
/// retry a changed payload under a spent key.
#[must_use]
pub fn idempotency_conflict_error(conflict: &IdempotencyConflict) -> McpError {
    legacy_tool_error(
        "IDEMPOTENCY_KEY_CONFLICT",
        format!(
            "idempotency_key '{}' for tool '{}' was already used with a different payload. \
             The original request stands; do not retry a changed payload under the same key. \
             Use a fresh idempotency_key for a genuinely new request.",
            conflict.key, conflict.tool
        ),
        false,
        json!({
            "tool": conflict.tool,
            "idempotency_key": conflict.key,
            "original_fingerprint": conflict.original_fingerprint,
            "attempted_fingerprint": conflict.attempted_fingerprint,
        }),
    )
}

/// If `replayed`, inject `"idempotent_replay": true` into the top-level object of
/// a serialized tool response so a client can distinguish a replay from a fresh
/// apply. A fresh (or keyless) response is returned unchanged, keeping default
/// output byte-identical to the conformance fixtures.
#[must_use]
pub fn with_replay_marker(response_json: String, replayed: bool) -> String {
    if !replayed {
        return response_json;
    }
    match serde_json::from_str::<Value>(&response_json) {
        Ok(Value::Object(mut map)) => {
            map.insert("idempotent_replay".to_string(), Value::Bool(true));
            serde_json::to_string(&Value::Object(map)).unwrap_or(response_json)
        }
        _ => response_json,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_blank_and_missing_are_none() {
        assert_eq!(normalize_idempotency_key(None).unwrap(), None);
        assert_eq!(normalize_idempotency_key(Some("")).unwrap(), None);
        assert_eq!(normalize_idempotency_key(Some("   ")).unwrap(), None);
    }

    #[test]
    fn normalize_trims_and_keeps_value() {
        assert_eq!(
            normalize_idempotency_key(Some("  abc-123  ")).unwrap(),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn normalize_rejects_overlong_key() {
        let long = "x".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1);
        let err = normalize_idempotency_key(Some(&long)).unwrap_err();
        let data = err.data.as_ref().expect("error data");
        assert_eq!(
            data.get("error")
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str),
            Some("INVALID_ARGUMENT")
        );
    }

    #[test]
    fn fingerprint_is_order_independent_and_sensitive() {
        let a = compute_fingerprint(
            "send_message",
            &[("subject", "hi".into()), ("body", "world".into())],
        );
        let b = compute_fingerprint(
            "send_message",
            &[("body", "world".into()), ("subject", "hi".into())],
        );
        assert_eq!(a, b, "field order must not change the fingerprint");

        let c = compute_fingerprint(
            "send_message",
            &[("subject", "hi".into()), ("body", "worlds".into())],
        );
        assert_ne!(a, c, "a changed value must change the fingerprint");

        // Same fields under a different tool must differ (per-tool scoping is in
        // the key too, but the fingerprint should also reflect the tool).
        let d = compute_fingerprint(
            "reply_message",
            &[("subject", "hi".into()), ("body", "world".into())],
        );
        assert_ne!(a, d);
    }

    #[test]
    fn fingerprint_no_boundary_collision() {
        // ("ab", "c") vs ("a", "bc") must not collide thanks to length-prefixing.
        let a = compute_fingerprint("t", &[("ab", "c".into())]);
        let b = compute_fingerprint("t", &[("a", "bc".into())]);
        assert_ne!(a, b);
    }

    #[test]
    fn replay_marker_only_on_replay() {
        let base = r#"{"count":1}"#.to_string();
        assert_eq!(with_replay_marker(base.clone(), false), base);
        let marked = with_replay_marker(base, true);
        let v: Value = serde_json::from_str(&marked).unwrap();
        assert_eq!(v.get("idempotent_replay"), Some(&Value::Bool(true)));
        assert_eq!(v.get("count"), Some(&Value::from(1)));
    }
}
