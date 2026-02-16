//! Step 3: Scrub snapshot — per-preset redaction of secrets, ack state, etc.
//!
//! Three presets: `standard`, `strict`, `archive`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlmodel_core::Value as SqlValue;

use crate::{ScrubPreset, ShareError};

/// Connection type for offline snapshot manipulation (C-backed SQLite).
type Conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection;

/// Keys to remove from attachment metadata dicts during scrubbing.
const ATTACHMENT_REDACT_KEYS: &[&str] = &[
    "download_url",
    "headers",
    "authorization",
    "signed_url",
    "bearer_token",
];

/// Compiled secret-detection regexes (built once, reused).
static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // GitHub tokens
        Regex::new(r"(?i)ghp_[A-Za-z0-9]{36,}").unwrap(),
        Regex::new(r"(?i)github_pat_[A-Za-z0-9_]{20,}").unwrap(),
        // Slack tokens
        Regex::new(r"(?i)xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap(),
        // OpenAI / generic sk- keys
        Regex::new(r"(?i)sk-[A-Za-z0-9]{20,}").unwrap(),
        // Bearer tokens
        Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\./+=]{16,}").unwrap(),
        // URL-embedded basic auth credentials
        Regex::new(r"(?i)https?://[^/\s:@]+:[^@\s/]+@").unwrap(),
        // Environment-variable references likely to contain secrets
        Regex::new(r"(?i)\$[A-Z_][A-Z0-9_]*(?:SECRET|TOKEN|KEY|PASSWORD)[A-Z0-9_]*").unwrap(),
        // JWTs (three base64url segments)
        Regex::new(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+").unwrap(),
        // AWS access key IDs (always start with AKIA)
        Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
        // PEM private keys (multi-line block)
        Regex::new(r"(?s)-----BEGIN[A-Z ]* PRIVATE KEY-----.*?-----END[A-Z ]* PRIVATE KEY-----")
            .unwrap(),
        // Anthropic API keys
        Regex::new(r"(?i)sk-ant-[A-Za-z0-9\-]{20,}").unwrap(),
        // GitLab tokens
        Regex::new(r"glpat-[A-Za-z0-9\-_]{20,}").unwrap(),
    ]
});

fn normalize_redact_key(key: &str) -> String {
    key.chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '\0')
        .flat_map(char::to_lowercase)
        .collect()
}

static NORMALIZED_ATTACHMENT_REDACT_KEYS: LazyLock<HashSet<String>> = LazyLock::new(|| {
    ATTACHMENT_REDACT_KEYS
        .iter()
        .map(|k| normalize_redact_key(k))
        .collect()
});

/// Per-preset configuration flags.
struct ScrubConfig {
    redact_body: bool,
    body_placeholder: Option<&'static str>,
    drop_attachments: bool,
    scrub_secrets: bool,
    clear_ack_state: bool,
    clear_recipients: bool,
    clear_file_reservations: bool,
    clear_agent_links: bool,
}

fn preset_config(preset: ScrubPreset) -> ScrubConfig {
    match preset {
        ScrubPreset::Standard => ScrubConfig {
            redact_body: false,
            body_placeholder: None,
            drop_attachments: false,
            scrub_secrets: true,
            clear_ack_state: true,
            clear_recipients: true,
            clear_file_reservations: true,
            clear_agent_links: true,
        },
        ScrubPreset::Strict => ScrubConfig {
            redact_body: true,
            body_placeholder: Some("[Message body redacted]"),
            drop_attachments: true,
            scrub_secrets: true,
            clear_ack_state: true,
            clear_recipients: true,
            clear_file_reservations: true,
            clear_agent_links: true,
        },
        ScrubPreset::Archive => ScrubConfig {
            redact_body: false,
            body_placeholder: None,
            drop_attachments: false,
            scrub_secrets: false,
            clear_ack_state: false,
            clear_recipients: false,
            clear_file_reservations: false,
            clear_agent_links: false,
        },
    }
}

/// Summary of scrub operations performed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrubSummary {
    pub preset: String,
    pub pseudonym_salt: String,
    pub agents_total: i64,
    pub agents_pseudonymized: i64,
    pub ack_flags_cleared: i64,
    pub recipients_cleared: i64,
    pub file_reservations_removed: i64,
    pub agent_links_removed: i64,
    pub secrets_replaced: i64,
    pub attachments_sanitized: i64,
    pub bodies_redacted: i64,
    pub attachments_cleared: i64,
}

/// Apply scrub operations to a snapshot database according to the given preset.
///
/// Operates in-place on the provided snapshot file.
///
/// # Errors
///
/// - [`ShareError::Sqlite`] on any SQLite error.
pub fn scrub_snapshot(
    snapshot_path: &Path,
    preset: ScrubPreset,
) -> Result<ScrubSummary, ShareError> {
    let cfg = preset_config(preset);
    let path_str = snapshot_path.display().to_string();
    // Use SqliteConnection (C-backed) for offline snapshot manipulation.
    // FrankenConnection lacks sqlite_master support needed by table_exists().
    let conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(&path_str).map_err(
        |e| ShareError::Sqlite {
            message: format!("cannot open snapshot {path_str}: {e}"),
        },
    )?;

    conn.execute_raw("PRAGMA foreign_keys = ON")
        .map_err(|e| ShareError::Sqlite {
            message: format!("PRAGMA foreign_keys failed: {e}"),
        })?;

    conn.execute_raw("BEGIN IMMEDIATE")
        .map_err(|e| ShareError::Sqlite {
            message: format!("BEGIN transaction failed: {e}"),
        })?;

    let result = (|| {
        // Count agents
        let agents_total = count_scalar(&conn, "SELECT COUNT(*) AS cnt FROM agents")?;

        // Clear ack state
        let ack_flags_cleared = if cfg.clear_ack_state {
            exec_count(&conn, "UPDATE messages SET ack_required = 0", &[])?
        } else {
            0
        };

        // Clear recipient timestamps
        let recipients_cleared = if cfg.clear_recipients {
            exec_count(
                &conn,
                "UPDATE message_recipients SET read_ts = NULL, ack_ts = NULL",
                &[],
            )?
        } else {
            0
        };

        // Delete file reservations
        let file_reservations_removed = if cfg.clear_file_reservations {
            exec_count(&conn, "DELETE FROM file_reservations", &[])?
        } else {
            0
        };

        // Delete agent links
        let agent_links_removed = if cfg.clear_agent_links {
            if table_exists(&conn, "agent_links")? {
                exec_count(&conn, "DELETE FROM agent_links", &[])?
            } else {
                0
            }
        } else {
            0
        };

        // Iterate messages and scrub
        let mut secrets_replaced: i64 = 0;
        let mut attachments_sanitized: i64 = 0;
        let mut bodies_redacted: i64 = 0;
        let mut attachments_cleared: i64 = 0;

        let message_rows = conn
            .query_sync(
                "SELECT id, subject, body_md, attachments FROM messages",
                &[],
            )
            .map_err(|e| ShareError::Sqlite {
                message: format!("SELECT messages failed: {e}"),
            })?;

        // Collect messages to process (avoid borrowing conn during iteration)
        let messages: Vec<(i64, String, String, String)> = message_rows
            .iter()
            .map(|row| {
                let id: i64 = row.get_named("id").unwrap_or(0);
                let subject: String = row.get_named("subject").unwrap_or_default();
                let body_md: String = row.get_named("body_md").unwrap_or_default();
                let attachments: String = row.get_named("attachments").unwrap_or_default();
                (id, subject, body_md, attachments)
            })
            .collect();

        for (msg_id, subject_original, body_original, attachments_value) in &messages {
            let mut subject = subject_original.clone();
            let mut body = body_original.clone();
            let mut subj_replacements: i64 = 0;
            let mut body_replacements: i64 = 0;

            if cfg.scrub_secrets {
                let (s, sr) = scrub_text(&subject);
                subject = s;
                subj_replacements = sr;
                let (b, br) = scrub_text(&body);
                body = b;
                body_replacements = br;
            }
            secrets_replaced += subj_replacements + body_replacements;

            // Parse attachments JSON
            let mut attachments_data: Vec<Value> = parse_attachments_json(attachments_value);
            let mut attachments_updated = false;
            let mut attachment_replacements: i64 = 0;

            // Drop attachments if preset requires it
            if cfg.drop_attachments && !attachments_data.is_empty() {
                attachments_data = Vec::new();
                attachments_cleared += 1;
                attachments_updated = true;
            }

            // Scrub secrets in attachment structure
            if cfg.scrub_secrets && !attachments_data.is_empty() {
                let (sanitized, rep_count, keys_removed) =
                    scrub_structure(&Value::Array(attachments_data.clone()));
                attachment_replacements += rep_count;
                if let Value::Array(arr) = sanitized {
                    if arr != attachments_data {
                        attachments_data = arr;
                        attachments_updated = true;
                    }
                }
                if keys_removed > 0 {
                    attachments_updated = true;
                }
            }

            // Write back attachment changes
            if attachments_updated {
                let sanitized_json =
                    serde_json::to_string(&attachments_data).unwrap_or_else(|_| "[]".to_string());
                exec_count(
                    &conn,
                    "UPDATE messages SET attachments = ? WHERE id = ?",
                    &[SqlValue::Text(sanitized_json), SqlValue::BigInt(*msg_id)],
                )?;
            }

            // Write back subject changes
            if subject != *subject_original {
                exec_count(
                    &conn,
                    "UPDATE messages SET subject = ? WHERE id = ?",
                    &[SqlValue::Text(subject), SqlValue::BigInt(*msg_id)],
                )?;
            }

            // Redact body or write back secret-scrubbed body
            if cfg.redact_body {
                let placeholder = cfg
                    .body_placeholder
                    .unwrap_or("[Message body redacted]")
                    .to_string();
                if *body_original != placeholder {
                    bodies_redacted += 1;
                    exec_count(
                        &conn,
                        "UPDATE messages SET body_md = ? WHERE id = ?",
                        &[SqlValue::Text(placeholder), SqlValue::BigInt(*msg_id)],
                    )?;
                }
            } else if body != *body_original {
                exec_count(
                    &conn,
                    "UPDATE messages SET body_md = ? WHERE id = ?",
                    &[SqlValue::Text(body), SqlValue::BigInt(*msg_id)],
                )?;
            }

            secrets_replaced += attachment_replacements;
            if attachments_updated || attachment_replacements > 0 {
                attachments_sanitized += 1;
            }
        }

        Ok(ScrubSummary {
            preset: preset.as_str().to_string(),
            pseudonym_salt: preset.as_str().to_string(),
            agents_total,
            agents_pseudonymized: 0,
            ack_flags_cleared,
            recipients_cleared,
            file_reservations_removed,
            agent_links_removed,
            secrets_replaced,
            attachments_sanitized,
            bodies_redacted,
            attachments_cleared,
        })
    })();

    match result {
        Ok(summary) => {
            conn.execute_raw("COMMIT").map_err(|e| ShareError::Sqlite {
                message: format!("COMMIT failed: {e}"),
            })?;
            Ok(summary)
        }
        Err(err) => {
            let _ = conn.execute_raw("ROLLBACK");
            Err(err)
        }
    }
}

/// Scan text for secret patterns and replace with `[REDACTED]`.
///
/// Returns `(scrubbed_text, replacement_count)`.
///
/// This is the same scanner used by `scrub_snapshot` but exposed for
/// use by the static rendering pipeline as a defense-in-depth measure.
pub fn scan_for_secrets(input: &str) -> (String, i64) {
    scrub_text(input)
}

/// Replace secret patterns in text with `[REDACTED]`.
/// Returns `(scrubbed_text, replacement_count)`.
fn scrub_text(input: &str) -> (String, i64) {
    let mut result = input.to_string();
    let mut count: i64 = 0;
    for pattern in SECRET_PATTERNS.iter() {
        // Optimization: check count first to avoid unnecessary allocation/replacement
        // logic if the pattern is not present.
        let matches = pattern.find_iter(&result).count();
        if matches > 0 {
            count += matches as i64;
            result = pattern.replace_all(&result, "[REDACTED]").to_string();
        }
    }
    (result, count)
}

/// Parse attachments field as JSON array, handling string-encoded JSON.
fn parse_attachments_json(value: &str) -> Vec<Value> {
    if value.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(value) {
        Ok(Value::Array(arr)) => arr,
        Ok(Value::String(inner)) => match serde_json::from_str::<Value>(&inner) {
            Ok(Value::Array(arr)) => arr,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Recursively scrub secrets in a JSON structure.
/// Returns `(scrubbed_value, secret_replacement_count, keys_removed_count)`.
fn scrub_structure(value: &Value) -> (Value, i64, i64) {
    match value {
        Value::String(s) => {
            let (scrubbed, count) = scrub_text(s);
            (Value::String(scrubbed), count, 0)
        }
        Value::Array(arr) => {
            let mut total_reps: i64 = 0;
            let mut total_keys: i64 = 0;
            let new_arr: Vec<Value> = arr
                .iter()
                .map(|item| {
                    let (v, r, k) = scrub_structure(item);
                    total_reps += r;
                    total_keys += k;
                    v
                })
                .collect();
            (Value::Array(new_arr), total_reps, total_keys)
        }
        Value::Object(obj) => {
            let mut new_obj = serde_json::Map::new();
            let mut total_reps: i64 = 0;
            let mut total_keys: i64 = 0;
            for (key, val) in obj {
                if NORMALIZED_ATTACHMENT_REDACT_KEYS.contains(&normalize_redact_key(key)) {
                    // Only count as removed if value is non-empty
                    if !is_empty_value(val) {
                        total_keys += 1;
                    }
                    // Remove the key entirely (don't add to new_obj)
                    continue;
                }
                let (v, r, k) = scrub_structure(val);
                total_reps += r;
                total_keys += k;
                new_obj.insert(key.clone(), v);
            }
            (Value::Object(new_obj), total_reps, total_keys)
        }
        other => (other.clone(), 0, 0),
    }
}

/// Check if a JSON value is "empty" (null, empty string, empty array, empty object).
fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn count_scalar(conn: &Conn, sql: &str) -> Result<i64, ShareError> {
    let rows = conn.query_sync(sql, &[]).map_err(|e| ShareError::Sqlite {
        message: format!("scalar query failed: {e}"),
    })?;
    Ok(rows
        .first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .unwrap_or(0))
}

fn exec_count(conn: &Conn, sql: &str, params: &[SqlValue]) -> Result<i64, ShareError> {
    let n = conn
        .execute_sync(sql, params)
        .map_err(|e| ShareError::Sqlite {
            message: format!("exec failed: {e}"),
        })?;
    Ok(i64::try_from(n).unwrap_or(0))
}

fn table_exists(conn: &Conn, name: &str) -> Result<bool, ShareError> {
    let rows = conn
        .query_sync(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            &[SqlValue::Text(name.to_string())],
        )
        .map_err(|e| ShareError::Sqlite {
            message: format!("table_exists check failed: {e}"),
        })?;
    Ok(!rows.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_text_finds_github_pat() {
        let (result, count) = scrub_text("Token: ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        assert_eq!(result, "Token: [REDACTED]");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_finds_multiple_patterns() {
        let input = "Use sk-abcdef0123456789012345 and bearer MyToken1234567890123456.";
        let (result, count) = scrub_text(input);
        assert_eq!(result, "Use [REDACTED] and [REDACTED]");
        assert_eq!(count, 2);
    }

    #[test]
    fn scrub_text_jwt() {
        let input = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let (result, count) = scrub_text(input);
        assert_eq!(result, "[REDACTED]");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_base64_token() {
        let input = "Authorization: Bearer dGVzdF9rZXk6c2VjcmV0L3ZhbHVlKys9";
        let (result, count) = scrub_text(input);
        assert_eq!(result, "Authorization: [REDACTED]");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_multiline_pem_private_key() {
        let input =
            "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQ\n-----END PRIVATE KEY-----";
        let (result, count) = scrub_text(input);
        assert_eq!(result, "[REDACTED]");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_url_embedded_credentials() {
        let input = "Fetch https://alice:s3cr3t@example.com/path now";
        let (result, count) = scrub_text(input);
        assert_eq!(result, "Fetch [REDACTED]example.com/path now");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_environment_variable_references() {
        let input = "set $SECRET_KEY and $API_TOKEN before launch";
        let (result, count) = scrub_text(input);
        assert_eq!(result, "set [REDACTED] and [REDACTED] before launch");
        assert_eq!(count, 2);
    }

    #[test]
    fn scrub_text_is_idempotent() {
        let input = "Use sk-abcdef0123456789012345 immediately";
        let (once, count_once) = scrub_text(input);
        let (twice, count_twice) = scrub_text(&once);
        assert_eq!(once, "Use [REDACTED] immediately");
        assert_eq!(twice, once);
        assert_eq!(count_once, 1);
        assert_eq!(count_twice, 0);
    }

    #[test]
    fn scrub_text_binary_safe_for_nonsecrets() {
        let input = "\u{0}\u{1}\u{2}plain\u{7f}\u{8}";
        let (result, count) = scrub_text(input);
        assert_eq!(result, input);
        assert_eq!(count, 0);
    }

    #[test]
    fn scrub_structure_removes_redact_keys() {
        let input: Value = serde_json::from_str(
            r#"[{"type":"file","path":"data.json","download_url":"https://secret.example.com","authorization":"Bearer abc"}]"#,
        ).unwrap();
        let (result, _, keys_removed) = scrub_structure(&input);
        let arr = result.as_array().unwrap();
        let obj = arr[0].as_object().unwrap();
        assert!(!obj.contains_key("download_url"));
        assert!(!obj.contains_key("authorization"));
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("path"));
        assert_eq!(keys_removed, 2);
    }

    #[test]
    fn scrub_structure_nested_secret_recursion() {
        let input: Value = serde_json::json!([{
            "type": "file",
            "metadata": {
                "nested": {
                    "token": "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"
                },
                "events": [
                    {
                        "payload": "Bearer dGVzdF90b2tlbl9uZXN0ZWQxMjM0NTY3ODkw"
                    }
                ]
            }
        }]);
        let (result, replacements, keys_removed) = scrub_structure(&input);
        assert_eq!(replacements, 2);
        assert_eq!(keys_removed, 0);

        let arr = result.as_array().unwrap();
        let root = arr[0].as_object().unwrap();
        assert_eq!(
            root["metadata"]["nested"]["token"].as_str(),
            Some("[REDACTED]")
        );
        assert_eq!(
            root["metadata"]["events"][0]["payload"].as_str(),
            Some("[REDACTED]")
        );
    }

    #[test]
    fn scrub_structure_multiline_key_removal() {
        let input: Value = serde_json::json!([{
            "type": "file",
            "path": "data.json",
            "authorization\r\n": "Bearer abcdefghijklmnopqrstuvwxyz123456",
            " signed_url ": "https://secret.example.com",
            "headers\t": {"x-trace":"1"}
        }]);
        let (result, replacements, keys_removed) = scrub_structure(&input);
        let arr = result.as_array().unwrap();
        let obj = arr[0].as_object().unwrap();

        assert_eq!(replacements, 0);
        assert_eq!(keys_removed, 3);
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("path"));
        assert!(!obj.contains_key("authorization\r\n"));
        assert!(!obj.contains_key(" signed_url "));
        assert!(!obj.contains_key("headers\t"));
    }

    #[test]
    fn archive_preset_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let summary = scrub_snapshot(&db, ScrubPreset::Archive).unwrap();
        assert_eq!(summary.ack_flags_cleared, 0);
        assert_eq!(summary.recipients_cleared, 0);
        assert_eq!(summary.file_reservations_removed, 0);
        assert_eq!(summary.agent_links_removed, 0);
        assert_eq!(summary.secrets_replaced, 0);
        assert_eq!(summary.bodies_redacted, 0);
        assert_eq!(summary.attachments_cleared, 0);
    }

    #[test]
    fn scrub_presets_apply_distinct_redaction_levels() {
        let dir = tempfile::tempdir().unwrap();
        let source_db = create_fixture_db(dir.path());
        let source_conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(
            source_db.display().to_string(),
        )
        .unwrap();
        source_conn
            .execute_sync(
                "UPDATE messages SET ack_required = 1, body_md = ?, attachments = ? WHERE id = 1",
                &[
                    SqlValue::Text("body has sk-abcdef0123456789012345".to_string()),
                    SqlValue::Text(
                        r#"[{"type":"file","download_url":"https://secret.example.com","authorization":"Bearer abcdefghijklmnopqrstuvwxyz123456","path":"data.json"}]"#
                            .to_string(),
                    ),
                ],
            )
            .unwrap();

        let standard_db = dir.path().join("standard.sqlite3");
        let strict_db = dir.path().join("strict.sqlite3");
        let archive_db = dir.path().join("archive.sqlite3");
        crate::create_sqlite_snapshot(&source_db, &standard_db, false).unwrap();
        crate::create_sqlite_snapshot(&source_db, &strict_db, false).unwrap();
        crate::create_sqlite_snapshot(&source_db, &archive_db, false).unwrap();

        scrub_snapshot(&standard_db, ScrubPreset::Standard).unwrap();
        scrub_snapshot(&strict_db, ScrubPreset::Strict).unwrap();
        scrub_snapshot(&archive_db, ScrubPreset::Archive).unwrap();

        fn fetch_message_state(db_path: &std::path::Path) -> (i64, String, String) {
            let conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(
                db_path.display().to_string(),
            )
            .unwrap();
            let rows = conn
                .query_sync(
                    "SELECT ack_required, body_md, attachments FROM messages WHERE id = 1",
                    &[],
                )
                .unwrap();
            let row = rows.first().unwrap();
            let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
            let body_md: String = row.get_named("body_md").unwrap_or_default();
            let attachments: String = row.get_named("attachments").unwrap_or_default();
            (ack_required, body_md, attachments)
        }

        let (std_ack, std_body, std_attachments) = fetch_message_state(&standard_db);
        assert_eq!(std_ack, 0);
        assert_eq!(std_body, "body has [REDACTED]");
        let std_attachment_json: Value = serde_json::from_str(&std_attachments).unwrap();
        let std_obj = std_attachment_json.as_array().unwrap()[0]
            .as_object()
            .unwrap();
        assert!(!std_obj.contains_key("download_url"));
        assert!(!std_obj.contains_key("authorization"));
        assert_eq!(
            std_obj.get("path").and_then(Value::as_str),
            Some("data.json")
        );

        let (strict_ack, strict_body, strict_attachments) = fetch_message_state(&strict_db);
        assert_eq!(strict_ack, 0);
        assert_eq!(strict_body, "[Message body redacted]");
        assert_eq!(strict_attachments, "[]");

        let (archive_ack, archive_body, archive_attachments) = fetch_message_state(&archive_db);
        assert_eq!(archive_ack, 1);
        assert_eq!(archive_body, "body has sk-abcdef0123456789012345");
        assert!(archive_attachments.contains("download_url"));
        assert!(archive_attachments.contains("authorization"));
    }

    /// Conformance test against the Python fixture for all 3 presets.
    #[test]
    fn conformance_scrub_standard() {
        run_conformance_preset(ScrubPreset::Standard, "expected_standard.json");
    }

    #[test]
    fn conformance_scrub_strict() {
        run_conformance_preset(ScrubPreset::Strict, "expected_strict.json");
    }

    #[test]
    fn conformance_scrub_archive() {
        run_conformance_preset(ScrubPreset::Archive, "expected_archive.json");
    }

    fn run_conformance_preset(preset: ScrubPreset, expected_file: &str) {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mcp-agent-mail-conformance/tests/conformance/fixtures/share");

        let source = fixture_dir.join("needs_scrub.sqlite3");
        if !source.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let expected_path = fixture_dir.join(expected_file);
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(&expected_path).unwrap()).unwrap();

        // Create a snapshot copy so we don't modify the fixture
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("scrub_test.sqlite3");
        crate::create_sqlite_snapshot(&source, &snapshot, false).unwrap();

        let summary = scrub_snapshot(&snapshot, preset).unwrap();
        let summary_json = serde_json::to_value(&summary).unwrap();
        let expected_summary = &expected["summary"];

        // Compare summary fields
        for key in [
            "preset",
            "pseudonym_salt",
            "agents_total",
            "agents_pseudonymized",
            "ack_flags_cleared",
            "recipients_cleared",
            "file_reservations_removed",
            "agent_links_removed",
            "secrets_replaced",
            "attachments_sanitized",
            "bodies_redacted",
            "attachments_cleared",
        ] {
            assert_eq!(
                summary_json[key], expected_summary[key],
                "summary.{key} mismatch for {preset} preset"
            );
        }

        // Verify message content
        let conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(
            snapshot.display().to_string(),
        )
        .unwrap();
        let rows = conn
            .query_sync(
                "SELECT id, subject, body_md, ack_required, attachments FROM messages ORDER BY id",
                &[],
            )
            .unwrap();

        let expected_msgs = expected["messages_after"].as_array().unwrap();
        assert_eq!(rows.len(), expected_msgs.len(), "message count mismatch");

        for (row, exp) in rows.iter().zip(expected_msgs.iter()) {
            let id: i64 = row.get_named("id").unwrap();
            let subject: String = row.get_named("subject").unwrap_or_default();
            let body_md: String = row.get_named("body_md").unwrap_or_default();
            let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
            let attachments: String = row.get_named("attachments").unwrap_or_default();

            assert_eq!(id, exp["id"].as_i64().unwrap(), "id mismatch");
            assert_eq!(
                subject,
                exp["subject"].as_str().unwrap(),
                "subject mismatch for msg {id}"
            );
            assert_eq!(
                body_md,
                exp["body_md"].as_str().unwrap(),
                "body_md mismatch for msg {id}"
            );
            assert_eq!(
                ack_required,
                exp["ack_required"].as_i64().unwrap(),
                "ack_required mismatch for msg {id}"
            );
            let actual_attachments: serde_json::Value =
                serde_json::from_str(&attachments).expect("attachments JSON should be valid");
            let expected_attachments: serde_json::Value = serde_json::from_str(
                exp["attachments"]
                    .as_str()
                    .expect("fixture attachments should be a JSON string"),
            )
            .expect("fixture attachments JSON should be valid");
            assert_eq!(
                actual_attachments, expected_attachments,
                "attachments mismatch for msg {id}"
            );
        }
    }

    fn create_fixture_db(dir: &std::path::Path) -> std::path::PathBuf {
        let db_path = dir.join("test.sqlite3");
        let conn = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(
            db_path.display().to_string(),
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', released_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER, a_agent_id INTEGER, b_project_id INTEGER, b_agent_id INTEGER, status TEXT DEFAULT 'pending', reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', updated_ts TEXT DEFAULT '', expires_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'test', '/test', '')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'TestAgent', '', '', '', '', '', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO messages VALUES (1, 1, 1, NULL, 'Hi', 'Hello world', 'normal', 0, '', '[]')")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 1, 'to', NULL, NULL)")
            .unwrap();
        db_path
    }
}
