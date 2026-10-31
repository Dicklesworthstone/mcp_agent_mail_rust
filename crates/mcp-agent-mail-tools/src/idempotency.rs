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

/// Read committed receipts without entering write admission or initializing a
/// mailbox. A missing or virgin database has no read-side schema yet; callers
/// must enter their ordinary guarded write path and recheck there. Errors in
/// existing history are never evidence that a key is absent.
pub(crate) async fn replay_read_pool(
    ctx: &McpContext,
) -> McpResult<Option<mcp_agent_mail_db::DbPool>> {
    use crate::tool_util::{
        db_error_to_mcp_error, db_outcome_to_mcp_result, ensure_request_live,
        get_coalescer_bypass_read_db_pool,
    };

    ensure_request_live(ctx)?;
    let config = mcp_agent_mail_db::DbPoolConfig::from_env();
    let resolved = mcp_agent_mail_db::resolve_mailbox_sqlite_path(&config.database_url)
        .map_err(db_error_to_mcp_error)?;
    if resolved.canonical_path == ":memory:" {
        // The generic read helper initializes an in-memory pool when absent.
        // Reuse only already-initialized memory; cold calls need write admission.
        return Ok(mcp_agent_mail_db::get_cached_pool(&config)
            .filter(mcp_agent_mail_db::DbPool::is_initialized));
    }
    match std::fs::metadata(&resolved.canonical_path) {
        Ok(metadata) if metadata.is_file() && metadata.len() == 0 => return Ok(None),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(McpError::internal_error(format!(
                "Cannot inspect database for committed idempotency receipts: {error}"
            )));
        }
    }

    let pool = get_coalescer_bypass_read_db_pool()?;
    let conn = db_outcome_to_mcp_result(pool.acquire(ctx.cx()).await)?;
    let schema = conn
        .query_sync("SELECT name FROM sqlite_master LIMIT 1", &[])
        .map_err(|error| {
            db_error_to_mcp_error(mcp_agent_mail_db::DbError::Sqlite(error.to_string()))
        })?;
    drop(conn);
    // Do not treat a missing idempotency table in an otherwise initialized
    // mailbox as an empty history. Its lookup must report the schema error.
    Ok((!schema.is_empty()).then_some(pool))
}

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

/// Mark a serialized tool response as an idempotent replay when `replayed`.
///
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
    fn replay_read_pool_only_reuses_initialized_memory() {
        use asupersync::Cx;
        use asupersync::runtime::RuntimeBuilder;
        use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};

        let root = tempfile::tempdir().expect("isolated in-memory fixture");
        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", "sqlite:///:memory:"),
                ("STORAGE_ROOT", root.path().to_str().unwrap()),
            ],
            || {
                Config::reset_cached();
                let runtime = RuntimeBuilder::current_thread().build().unwrap();
                runtime.block_on(async {
                    let ctx = McpContext::new(Cx::current().expect("fixture context"), 1);
                    let config = mcp_agent_mail_db::DbPoolConfig::from_env();
                    assert!(replay_read_pool(&ctx).await.unwrap().is_none());
                    assert!(mcp_agent_mail_db::get_cached_pool(&config).is_none());

                    let writer = crate::tool_util::get_db_pool_for_request(&ctx)
                        .expect("ordinary admitted memory bootstrap");
                    let project = mcp_agent_mail_db::queries::ensure_project(
                        ctx.cx(),
                        &writer,
                        "/memory-replay",
                    )
                    .await
                    .into_result()
                    .expect("seed original memory state");
                    let engine = writer.engine_identity_key();
                    let _server_pool = mcp_agent_mail_db::get_cached_pool(&config)
                        .expect("retain the initialized server pool");
                    drop(writer);

                    let reader = replay_read_pool(&ctx)
                        .await
                        .unwrap()
                        .expect("reuse initialized memory");
                    assert_eq!(reader.engine_identity_key(), engine);
                    let observed = mcp_agent_mail_db::queries::get_project_by_human_key(
                        ctx.cx(),
                        &reader,
                        "/memory-replay",
                    )
                    .await
                    .into_result()
                    .expect("read the existing memory state");
                    assert_eq!(observed.id, project.id);
                    assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
                });
                Config::reset_cached();
            },
        );
    }

    #[test]
    fn replay_read_pool_preserves_cold_bootstrap_and_unavailable_history() {
        use asupersync::Cx;
        use asupersync::runtime::RuntimeBuilder;
        use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};

        for fixture in [
            "missing",
            "zero_bytes",
            "empty_schema",
            "partial_schema",
            "corrupt",
        ] {
            let root = tempfile::tempdir().expect("isolated replay fixture");
            let database = root.path().join("mail.sqlite3");
            match fixture {
                "zero_bytes" => std::fs::write(&database, []).unwrap(),
                "empty_schema" | "partial_schema" => {
                    let conn = mcp_agent_mail_db::DbConn::open_file(database.to_str().unwrap())
                        .expect("seed SQLite fixture");
                    conn.execute_raw("PRAGMA user_version = 1").unwrap();
                    if fixture == "partial_schema" {
                        conn.execute_raw("CREATE TABLE preserved_data (id INTEGER PRIMARY KEY)")
                            .unwrap();
                    }
                }
                "corrupt" => std::fs::write(&database, b"not a SQLite database").unwrap(),
                _ => {}
            }
            let database_url = format!("sqlite://{}", database.display());
            let storage_root = root.path().join("archive");
            with_process_env_overrides_for_test(
                &[
                    ("DATABASE_URL", database_url.as_str()),
                    ("STORAGE_ROOT", storage_root.to_str().unwrap()),
                ],
                || {
                    Config::reset_cached();
                    let runtime = RuntimeBuilder::current_thread().build().unwrap();
                    runtime.block_on(async {
                        let ctx = McpContext::new(Cx::current().expect("fixture context"), 1);
                        let result = replay_read_pool(&ctx).await;
                        match fixture {
                            "corrupt" => assert!(
                                result.is_err(),
                                "unreadable history must not become an idempotency miss"
                            ),
                            "partial_schema" => {
                                let pool = result.unwrap().expect("existing schema is not virgin");
                                let claim = mcp_agent_mail_db::IdempotencyClaim {
                                    project_id: 1,
                                    tool: "send_message",
                                    key: "existing-key",
                                    fingerprint: "payload",
                                };
                                let lookup =
                                    mcp_agent_mail_db::queries::lookup_idempotency_result::<Value>(
                                        ctx.cx(),
                                        &pool,
                                        claim,
                                    )
                                    .await;
                                assert!(
                                    matches!(lookup, asupersync::Outcome::Err(_)),
                                    "a missing receipt table must fail, not return a key miss"
                                );
                            }
                            _ => assert!(result.unwrap().is_none(), "cold fixture: {fixture}"),
                        }
                        assert!(
                            mcp_agent_mail_db::get_cached_pool(
                                &mcp_agent_mail_db::DbPoolConfig::from_env()
                            )
                            .is_none(),
                            "receipt probing must not bootstrap an authoritative writer"
                        );
                    });
                    Config::reset_cached();
                },
            );
            assert!(
                !storage_root.exists(),
                "receipt probing must not create archives"
            );
            if fixture == "missing" {
                assert!(
                    !database.exists(),
                    "receipt probing must not create a database"
                );
            } else if fixture == "zero_bytes" {
                assert_eq!(std::fs::metadata(&database).unwrap().len(), 0);
            }
        }
    }

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
