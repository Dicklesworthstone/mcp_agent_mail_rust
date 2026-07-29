//! Step 3: Scrub snapshot — per-preset redaction of secrets, ack state, etc.
//!
//! Three presets: `standard`, `strict`, `archive`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use mcp_agent_mail_db::DbConn;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlmodel_core::Value as SqlValue;

use crate::{ScrubPreset, ShareError};

type Conn = DbConn;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectPathRedaction {
    original: String,
    replacement: String,
}

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
        Regex::new(r"(?i)ghp_[A-Za-z0-9]{36,}").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"(?i)github_pat_[A-Za-z0-9_]{20,}").unwrap_or_else(|_| unreachable!()),
        // Slack tokens
        Regex::new(r"(?i)xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap_or_else(|_| unreachable!()),
        // Anthropic API keys (must precede generic sk- pattern)
        Regex::new(r"(?i)sk-ant-[A-Za-z0-9\-]{20,}").unwrap_or_else(|_| unreachable!()),
        // Stripe API keys
        Regex::new(r"(?i)(?:sk|pk|rk)_(?:live|test)_[A-Za-z0-9]{10,}")
            .unwrap_or_else(|_| unreachable!()),
        // OpenAI / generic sk- keys
        Regex::new(r"(?i)sk-[A-Za-z0-9]{20,}").unwrap_or_else(|_| unreachable!()),
        // Bearer tokens
        Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\./+=]{16,}").unwrap_or_else(|_| unreachable!()),
        // URL-embedded basic auth credentials (broader URI support)
        Regex::new(r"(?i)[a-z][a-z0-9+.-]*://[^/\s@]+:[^@\s/]+@")
            .unwrap_or_else(|_| unreachable!()),
        // Environment-variable references likely to contain secrets
        Regex::new(r"(?i)\$[A-Z_][A-Z0-9_]*(?:SECRET|TOKEN|KEY|PASSWORD)[A-Z0-9_]*")
            .unwrap_or_else(|_| unreachable!()),
        // JWTs (three base64url segments)
        Regex::new(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+")
            .unwrap_or_else(|_| unreachable!()),
        // AWS access key IDs (always start with AKIA)
        Regex::new(r"AKIA[0-9A-Z]{16}").unwrap_or_else(|_| unreachable!()),
        // Azure connection strings
        Regex::new(r"(?i)(?:AccountKey|SharedAccessKey)=[A-Za-z0-9+/=]{20,}")
            .unwrap_or_else(|_| unreachable!()),
        // GCP service-account private key IDs
        Regex::new(r#""private_key_id"\s*:\s*"[a-f0-9]{40}""#).unwrap_or_else(|_| unreachable!()),
        // Google API keys
        Regex::new(r"AIza[0-9A-Za-z\-_]{35}").unwrap_or_else(|_| unreachable!()),
        // npm tokens
        Regex::new(r"(?i)npm_[A-Za-z0-9]{36,}").unwrap_or_else(|_| unreachable!()),
        // PEM private keys (multi-line block)
        Regex::new(r"(?s)-----BEGIN[A-Z ]* PRIVATE KEY-----.*?-----END[A-Z ]* PRIVATE KEY-----")
            .unwrap_or_else(|_| unreachable!()),
        // GitLab tokens
        Regex::new(r"glpat-[A-Za-z0-9\-_]{20,}").unwrap_or_else(|_| unreachable!()),
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
    redact_project_paths: bool,
    clear_ack_state: bool,
    clear_recipient_state: bool,
    drop_recipient_metadata: bool,
    clear_file_reservations: bool,
    clear_agent_links: bool,
    /// Drop the global `tool_metrics_snapshots` telemetry. This table is not
    /// keyed by project, so it survives project scoping and would otherwise ship
    /// cross-project tool usage (names, capabilities, latencies, call/error
    /// counts) in a shared bundle. Cleared for sharing presets; kept for the
    /// full-fidelity archive preset.
    clear_tool_metrics: bool,
}

fn preset_config(preset: ScrubPreset) -> ScrubConfig {
    match preset {
        ScrubPreset::Standard => ScrubConfig {
            redact_body: false,
            body_placeholder: None,
            drop_attachments: false,
            scrub_secrets: true,
            redact_project_paths: true,
            clear_ack_state: true,
            clear_recipient_state: true,
            drop_recipient_metadata: false,
            clear_file_reservations: true,
            clear_agent_links: true,
            clear_tool_metrics: true,
        },
        ScrubPreset::Strict => ScrubConfig {
            redact_body: true,
            body_placeholder: Some("[Message body redacted]"),
            drop_attachments: true,
            scrub_secrets: true,
            redact_project_paths: true,
            clear_ack_state: true,
            clear_recipient_state: false,
            drop_recipient_metadata: true,
            clear_file_reservations: true,
            clear_agent_links: true,
            clear_tool_metrics: true,
        },
        ScrubPreset::Archive => ScrubConfig {
            redact_body: false,
            body_placeholder: None,
            drop_attachments: false,
            scrub_secrets: false,
            redact_project_paths: false,
            clear_ack_state: false,
            clear_recipient_state: false,
            drop_recipient_metadata: false,
            clear_file_reservations: false,
            clear_agent_links: false,
            clear_tool_metrics: false,
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

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DerivedExportArtifacts {
    has_search_index: bool,
    has_materialized_views: bool,
    has_fts_overview: bool,
    has_shadow_columns: bool,
}

impl DerivedExportArtifacts {
    pub(crate) const fn any(self) -> bool {
        self.has_search_index
            || self.has_materialized_views
            || self.has_fts_overview
            || self.has_shadow_columns
    }
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
    let snapshot_path = crate::require_real_share_sqlite_path(snapshot_path)?;
    let path_str = snapshot_path.display().to_string();
    let conn = Conn::open_file(&path_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot open snapshot {path_str}: {e}"),
    })?;
    let derived_artifacts = detect_derived_export_artifacts(&conn)?;

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

        // Standard exports preserve recipient identities but clear per-recipient
        // read/ack state. Strict exports must remove recipient metadata entirely
        // from the bundled database, not merely hide it in rendered HTML.
        let recipients_cleared = if cfg.drop_recipient_metadata {
            let removed = exec_count(&conn, "DELETE FROM message_recipients", &[])?;
            if column_exists(&conn, "messages", "recipients_json")? {
                exec_count(
                    &conn,
                    "UPDATE messages SET recipients_json = ?",
                    &[SqlValue::Text(empty_recipients_json())],
                )?;
            }
            if table_exists(&conn, "inbox_stats")? {
                // These aggregates are computed from recipient rows, so keeping
                // them after strict recipient scrubbing would leak recipient
                // activity despite the row-level metadata being removed.
                exec_count(&conn, "DELETE FROM inbox_stats", &[])?;
            }
            removed
        } else if cfg.clear_recipient_state {
            exec_count(
                &conn,
                "UPDATE message_recipients SET read_ts = NULL, ack_ts = NULL",
                &[],
            )?
        } else {
            0
        };

        // Delete file reservations (child table first to satisfy FK constraint)
        let file_reservations_removed = if cfg.clear_file_reservations {
            if table_exists(&conn, "file_reservation_releases")? {
                exec_count(&conn, "DELETE FROM file_reservation_releases", &[])?;
            }
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

        // Drop global tool telemetry. `tool_metrics_snapshots` is not keyed by
        // project, so project scoping never filters it; left in place it would
        // ship cross-project tool usage (names, capabilities, latencies, call /
        // error counts) inside a shared bundle. Cleared for sharing presets.
        if cfg.clear_tool_metrics && table_exists(&conn, "tool_metrics_snapshots")? {
            exec_count(&conn, "DELETE FROM tool_metrics_snapshots", &[])?;
        }

        let project_path_redactions = if cfg.redact_project_paths {
            redact_project_paths(&conn)?
        } else {
            Vec::new()
        };
        if !project_path_redactions.is_empty() {
            redact_project_paths_in_text_column(
                &conn,
                RedactTextColumn::AgentTaskDescription,
                &project_path_redactions,
            )?;
            redact_project_paths_in_text_column(
                &conn,
                RedactTextColumn::SiblingSuggestionRationale,
                &project_path_redactions,
            )?;
        }

        // Iterate messages and scrub in chunks to avoid OOM
        let mut secrets_replaced: i64 = 0;
        let mut attachments_sanitized: i64 = 0;
        let mut bodies_redacted: i64 = 0;
        let mut attachments_cleared: i64 = 0;

        let mut last_id = 0i64;
        loop {
            let message_rows = conn
                .query_sync(
                    "SELECT id, subject, body_md, attachments FROM messages WHERE id > ? ORDER BY id ASC LIMIT 500",
                    &[SqlValue::BigInt(last_id)],
                )
                .map_err(|e| ShareError::Sqlite {
                    message: format!("SELECT messages failed: {e}"),
                })?;

            if message_rows.is_empty() {
                break;
            }

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
                last_id = *msg_id;
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
                if !project_path_redactions.is_empty() {
                    subject = redact_project_paths_in_text(&subject, &project_path_redactions);
                    body = redact_project_paths_in_text(&body, &project_path_redactions);
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
                        scrub_structure(&Value::Array(attachments_data.clone()), 0);
                    attachment_replacements += rep_count;
                    if let Value::Array(arr) = sanitized
                        && arr != attachments_data
                    {
                        attachments_data = arr;
                        attachments_updated = true;
                    }
                    if keys_removed > 0 {
                        attachments_updated = true;
                    }
                }
                if !project_path_redactions.is_empty() && !attachments_data.is_empty() {
                    let sanitized = redact_project_paths_in_value(
                        &Value::Array(attachments_data.clone()),
                        0,
                        &project_path_redactions,
                    );
                    if let Value::Array(arr) = sanitized
                        && arr != attachments_data
                    {
                        attachments_data = arr;
                        attachments_updated = true;
                    }
                }

                // Write back attachment changes
                if attachments_updated {
                    let sanitized_json = crate::encode_json(
                        &attachments_data,
                        "scrubbed attachments serialization failed",
                    )?;
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
        }

        // Secret-scrub the free-text columns that aren't message bodies. Agent
        // task descriptions and sibling-suggestion rationales can carry tokens /
        // keys just like a message body (e.g. "deploying with ghp_… to prod"),
        // and the in-browser viewer loads the raw DB, so the static-HTML defense
        // doesn't protect them. Path redaction already ran on these columns
        // above; this applies the same secret scanning messages get.
        if cfg.scrub_secrets {
            secrets_replaced +=
                scrub_secrets_in_text_column(&conn, RedactTextColumn::AgentTaskDescription)?;
            secrets_replaced +=
                scrub_secrets_in_text_column(&conn, RedactTextColumn::SiblingSuggestionRationale)?;
            if column_exists(&conn, "agents", "registration_token")? {
                // Registration tokens are credentials by definition. Sharing
                // presets must remove them even when their encoding does not
                // match a generic secret detector; the full-fidelity archive
                // preset keeps them so disaster recovery remains lossless.
                secrets_replaced += exec_count(
                    &conn,
                    "UPDATE agents SET registration_token = NULL \
                     WHERE registration_token IS NOT NULL",
                    &[],
                )?;
            }
        }

        // Generate a stable salt for pseudonymization reproducibility (matches Python).
        let pseudonym_salt = preset.as_str().to_string();
        Ok(ScrubSummary {
            preset: preset.as_str().to_string(),
            pseudonym_salt,
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
            if scrub_mutates_export_artifacts(&cfg) && derived_artifacts.any() {
                refresh_derived_export_artifacts(&snapshot_path, derived_artifacts)?;
            }
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
fn scrub_structure(value: &Value, depth: usize) -> (Value, i64, i64) {
    // Cap recursion at a high hard limit to avoid stack blow-ups on malicious
    // payloads while still scrubbing realistically deep JSON structures.
    // Return the value as-is rather than Null to avoid data corruption.
    if depth > 256 {
        return (value.clone(), 0, 0);
    }
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
                    let (v, r, k) = scrub_structure(item, depth + 1);
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
                let (v, r, k) = scrub_structure(val, depth + 1);
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

/// Check if a table exists. Uses a direct SELECT probe because
/// FrankenConnection does not support sqlite_master queries.
fn table_exists(conn: &Conn, name: &str) -> Result<bool, ShareError> {
    let probe = format!("SELECT 1 FROM \"{name}\" LIMIT 0");
    match conn.query_sync(&probe, &[]) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn column_exists(conn: &Conn, table: &str, column: &str) -> Result<bool, ShareError> {
    let probe = format!("SELECT \"{column}\" FROM \"{table}\" LIMIT 0");
    match conn.query_sync(&probe, &[]) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

pub(crate) fn detect_derived_export_artifacts(
    conn: &Conn,
) -> Result<DerivedExportArtifacts, ShareError> {
    Ok(DerivedExportArtifacts {
        has_search_index: table_exists(conn, "fts_messages")?,
        has_materialized_views: table_exists(conn, "message_overview_mv")?
            || table_exists(conn, "attachments_by_message_mv")?,
        has_fts_overview: table_exists(conn, "fts_search_overview_mv")?,
        has_shadow_columns: column_exists(conn, "messages", "subject_lower")?
            || column_exists(conn, "messages", "sender_lower")?,
    })
}

const fn scrub_mutates_export_artifacts(cfg: &ScrubConfig) -> bool {
    cfg.redact_body
        || cfg.drop_attachments
        || cfg.scrub_secrets
        || cfg.redact_project_paths
        || cfg.clear_ack_state
        || cfg.clear_recipient_state
        || cfg.drop_recipient_metadata
}

pub(crate) fn refresh_derived_export_artifacts(
    snapshot_path: &Path,
    artifacts: DerivedExportArtifacts,
) -> Result<(), ShareError> {
    let path_str = snapshot_path.display().to_string();
    let conn = Conn::open_file(&path_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot reopen snapshot {path_str} for artifact refresh: {e}"),
    })?;

    if artifacts.has_materialized_views {
        conn.execute_raw("DROP TABLE IF EXISTS message_overview_mv")
            .map_err(|e| ShareError::Sqlite {
                message: format!("drop stale message_overview_mv failed: {e}"),
            })?;
        conn.execute_raw("DROP TABLE IF EXISTS attachments_by_message_mv")
            .map_err(|e| ShareError::Sqlite {
                message: format!("drop stale attachments_by_message_mv failed: {e}"),
            })?;
    }
    if artifacts.has_fts_overview {
        conn.execute_raw("DROP TABLE IF EXISTS fts_search_overview_mv")
            .map_err(|e| ShareError::Sqlite {
                message: format!("drop stale fts_search_overview_mv failed: {e}"),
            })?;
    }
    if artifacts.has_search_index {
        conn.execute_raw("DROP TABLE IF EXISTS fts_messages")
            .map_err(|e| ShareError::Sqlite {
                message: format!("drop stale fts_messages failed: {e}"),
            })?;
    }

    let fts_enabled = if artifacts.has_search_index || artifacts.has_fts_overview {
        crate::build_search_indexes(snapshot_path)?
    } else {
        false
    };

    if artifacts.has_materialized_views || artifacts.has_fts_overview {
        crate::build_materialized_views(snapshot_path, fts_enabled)?;
    }
    if artifacts.has_shadow_columns {
        crate::create_performance_indexes(snapshot_path)?;
    }

    Ok(())
}

fn empty_recipients_json() -> String {
    serde_json::json!({
        "to": Vec::<String>::new(),
        "cc": Vec::<String>::new(),
        "bcc": Vec::<String>::new(),
    })
    .to_string()
}

pub(crate) fn redacted_project_human_key(slug: &str) -> String {
    let slug = slug.trim();
    if slug.is_empty() {
        "[project path redacted]".to_string()
    } else {
        format!("[project path redacted: {slug}]")
    }
}

pub(crate) fn redact_scope_project_human_keys(scope: &mut crate::scope::ProjectScopeResult) {
    for project in &mut scope.projects {
        project.human_key = redacted_project_human_key(&project.slug);
    }
}

pub(crate) fn redact_manifest_project_identifier(identifier: &str) -> String {
    let trimmed = identifier.trim();
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.starts_with('~') {
        "[project identifier redacted]".to_string()
    } else {
        identifier.to_string()
    }
}

fn project_path_redaction_for(slug: &str, human_key: &str) -> Option<ProjectPathRedaction> {
    let original = human_key.trim();
    if original.is_empty()
        || original == "/"
        || original == "\\"
        || !(original.contains('/') || original.contains('\\') || original.starts_with('~'))
    {
        return None;
    }

    let replacement = redacted_project_human_key(slug);
    if original == replacement {
        return None;
    }

    Some(ProjectPathRedaction {
        original: original.to_string(),
        replacement,
    })
}

fn redact_project_paths_in_text(input: &str, redactions: &[ProjectPathRedaction]) -> String {
    let mut out = input.to_string();
    for redaction in redactions {
        out = out.replace(&redaction.original, &redaction.replacement);
    }
    out
}

fn redact_project_paths_in_value(
    value: &Value,
    depth: usize,
    redactions: &[ProjectPathRedaction],
) -> Value {
    if depth > 256 {
        return value.clone();
    }
    match value {
        Value::String(s) => Value::String(redact_project_paths_in_text(s, redactions)),
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|item| redact_project_paths_in_value(item, depth + 1, redactions))
                .collect(),
        ),
        Value::Object(obj) => {
            let mut redacted = serde_json::Map::new();
            for (key, val) in obj {
                redacted.insert(
                    key.clone(),
                    redact_project_paths_in_value(val, depth + 1, redactions),
                );
            }
            Value::Object(redacted)
        }
        other => other.clone(),
    }
}

#[derive(Debug, Clone, Copy)]
enum RedactTextColumn {
    AgentTaskDescription,
    SiblingSuggestionRationale,
}

impl RedactTextColumn {
    const fn table(self) -> &'static str {
        match self {
            Self::AgentTaskDescription => "agents",
            Self::SiblingSuggestionRationale => "project_sibling_suggestions",
        }
    }

    const fn column(self) -> &'static str {
        match self {
            Self::AgentTaskDescription => "task_description",
            Self::SiblingSuggestionRationale => "rationale",
        }
    }

    const fn select_sql(self) -> &'static str {
        match self {
            Self::AgentTaskDescription => "SELECT id, task_description FROM agents ORDER BY id ASC",
            Self::SiblingSuggestionRationale => {
                "SELECT id, rationale FROM project_sibling_suggestions ORDER BY id ASC"
            }
        }
    }

    const fn update_sql(self) -> &'static str {
        match self {
            Self::AgentTaskDescription => "UPDATE agents SET task_description = ? WHERE id = ?",
            Self::SiblingSuggestionRationale => {
                "UPDATE project_sibling_suggestions SET rationale = ? WHERE id = ?"
            }
        }
    }
}

fn redact_project_paths_in_text_column(
    conn: &Conn,
    target: RedactTextColumn,
    redactions: &[ProjectPathRedaction],
) -> Result<(), ShareError> {
    let table = target.table();
    let column = target.column();
    if redactions.is_empty() || !column_exists(conn, table, column)? {
        return Ok(());
    }

    let rows = conn
        .query_sync(target.select_sql(), &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("SELECT {table}.{column} failed: {e}"),
        })?;

    let values: Vec<(i64, String)> = rows
        .iter()
        .map(|row| {
            let id: i64 = row.get_named("id").unwrap_or(0);
            let value: String = row.get_named(column).unwrap_or_default();
            (id, value)
        })
        .collect();

    for (id, original) in values {
        let redacted = redact_project_paths_in_text(&original, redactions);
        if redacted != original {
            exec_count(
                conn,
                target.update_sql(),
                &[SqlValue::Text(redacted), SqlValue::BigInt(id)],
            )?;
        }
    }

    Ok(())
}

/// Secret-scrub a free-text column (agent `task_description`, sibling-suggestion
/// `rationale`) the same way message subjects/bodies are scrubbed. Returns the
/// number of secret replacements applied so the caller can fold it into the
/// `secrets_replaced` total.
fn scrub_secrets_in_text_column(conn: &Conn, target: RedactTextColumn) -> Result<i64, ShareError> {
    let table = target.table();
    let column = target.column();
    if !column_exists(conn, table, column)? {
        return Ok(0);
    }

    let rows = conn
        .query_sync(target.select_sql(), &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("SELECT {table}.{column} failed: {e}"),
        })?;

    let values: Vec<(i64, String)> = rows
        .iter()
        .map(|row| {
            let id: i64 = row.get_named("id").unwrap_or(0);
            let value: String = row.get_named(column).unwrap_or_default();
            (id, value)
        })
        .collect();

    let mut replaced = 0i64;
    for (id, original) in values {
        let (scrubbed, count) = scrub_text(&original);
        if count > 0 {
            exec_count(
                conn,
                target.update_sql(),
                &[SqlValue::Text(scrubbed), SqlValue::BigInt(id)],
            )?;
            replaced += count;
        }
    }

    Ok(replaced)
}

fn redact_project_paths(conn: &Conn) -> Result<Vec<ProjectPathRedaction>, ShareError> {
    if !column_exists(conn, "projects", "human_key")? {
        return Ok(Vec::new());
    }

    let has_slug = column_exists(conn, "projects", "slug")?;
    let select_sql = if has_slug {
        "SELECT id, slug, human_key FROM projects ORDER BY id ASC"
    } else {
        "SELECT id, human_key FROM projects ORDER BY id ASC"
    };

    let rows = conn
        .query_sync(select_sql, &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("SELECT projects failed: {e}"),
        })?;

    let projects: Vec<(i64, String, String)> = rows
        .iter()
        .map(|row| {
            let id: i64 = row.get_named("id").unwrap_or(0);
            let slug = if has_slug {
                row.get_named("slug").unwrap_or_default()
            } else {
                format!("project-{id}")
            };
            let human_key: String = row.get_named("human_key").unwrap_or_default();
            (id, slug, human_key)
        })
        .collect();

    let mut redactions = Vec::new();
    for (id, slug, human_key) in projects {
        let redacted = redacted_project_human_key(&slug);
        if let Some(redaction) = project_path_redaction_for(&slug, &human_key) {
            redactions.push(redaction);
        }
        if human_key != redacted {
            exec_count(
                conn,
                "UPDATE projects SET human_key = ? WHERE id = ?",
                &[SqlValue::Text(redacted), SqlValue::BigInt(id)],
            )?;
        }
    }
    redactions.sort_by_key(|redaction| std::cmp::Reverse(redaction.original.len()));

    Ok(redactions)
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
        let (result, _, keys_removed) = scrub_structure(&input, 0);
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
        let (result, replacements, keys_removed) = scrub_structure(&input, 0);
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
    fn scrub_structure_scrubs_secrets_deeper_than_20_levels() {
        let mut nested = serde_json::json!("ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        for _ in 0..25 {
            nested = serde_json::json!({ "nested": nested });
        }
        let input: Value = serde_json::json!([{ "metadata": nested }]);

        let (result, replacements, keys_removed) = scrub_structure(&input, 0);
        assert_eq!(replacements, 1);
        assert_eq!(keys_removed, 0);

        let mut cursor = &result[0]["metadata"];
        for _ in 0..25 {
            cursor = &cursor["nested"];
        }
        assert_eq!(cursor.as_str(), Some("[REDACTED]"));
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
        let (result, replacements, keys_removed) = scrub_structure(&input, 0);
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
    fn sharing_presets_remove_registration_tokens_but_archive_preserves_them() {
        for (preset, expected_token) in [
            (ScrubPreset::Standard, None),
            (ScrubPreset::Strict, None),
            (ScrubPreset::Archive, Some("registration-secret")),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = create_fixture_db(dir.path());
            let conn = Conn::open_file(db.display().to_string()).unwrap();
            conn.execute_raw("ALTER TABLE agents ADD COLUMN registration_token TEXT")
                .unwrap();
            conn.execute_raw(
                "UPDATE agents SET registration_token = 'registration-secret' WHERE id = 1",
            )
            .unwrap();
            drop(conn);

            scrub_snapshot(&db, preset).unwrap();

            let conn = Conn::open_file(db.display().to_string()).unwrap();
            let rows = conn
                .query_sync("SELECT registration_token FROM agents WHERE id = 1", &[])
                .unwrap();
            let token = rows[0]
                .get_named::<Option<String>>("registration_token")
                .unwrap();
            assert_eq!(token.as_deref(), expected_token, "preset={preset:?}");
        }
    }

    #[test]
    fn scrub_clears_tool_metrics_for_sharing_presets_but_keeps_for_archive() {
        // `tool_metrics_snapshots` is global, not project-keyed, so project
        // scoping never filters it. A sharing export must drop it (it would
        // otherwise leak cross-project tool telemetry); the full-fidelity
        // archive preset must keep it.
        fn seed_tool_metrics(db: &std::path::Path) {
            let conn = Conn::open_file(db.display().to_string()).unwrap();
            conn.execute_raw(
                "CREATE TABLE tool_metrics_snapshots (id INTEGER PRIMARY KEY, tool_name TEXT, capabilities_json TEXT DEFAULT '', calls INTEGER DEFAULT 0)",
            )
            .unwrap();
            conn.execute_raw(
                "INSERT INTO tool_metrics_snapshots (id, tool_name, capabilities_json, calls) VALUES (1, 'send_message', '[\"messaging\"]', 7)",
            )
            .unwrap();
        }
        fn tool_metrics_count(db: &std::path::Path) -> i64 {
            let conn = Conn::open_file(db.display().to_string()).unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS cnt FROM tool_metrics_snapshots", &[])
                .unwrap();
            rows[0].get_named("cnt").unwrap_or(-1)
        }

        for (preset, expected, label) in [
            (ScrubPreset::Standard, 0, "standard"),
            (ScrubPreset::Strict, 0, "strict"),
            (ScrubPreset::Archive, 1, "archive"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = create_fixture_db(dir.path());
            seed_tool_metrics(&db);
            scrub_snapshot(&db, preset).unwrap();
            assert_eq!(
                tool_metrics_count(&db),
                expected,
                "tool_metrics_snapshots row count after {label} scrub"
            );
        }
    }

    #[test]
    fn scrub_presets_apply_distinct_redaction_levels() {
        let dir = tempfile::tempdir().unwrap();
        let source_db = create_fixture_db(dir.path());
        let source_conn = Conn::open_file(source_db.display().to_string()).unwrap();
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

        fn fetch_message_state(db_path: &std::path::Path) -> (i64, String, String, String) {
            let conn = Conn::open_file(db_path.display().to_string()).unwrap();
            let rows = conn
                .query_sync(
                    "SELECT ack_required, body_md, attachments, recipients_json FROM messages WHERE id = 1",
                    &[],
                )
                .unwrap();
            let row = rows.first().unwrap();
            let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
            let body_md: String = row.get_named("body_md").unwrap_or_default();
            let attachments: String = row.get_named("attachments").unwrap_or_default();
            let recipients_json: String = row.get_named("recipients_json").unwrap_or_default();
            (ack_required, body_md, attachments, recipients_json)
        }

        fn recipient_row_count(db_path: &std::path::Path) -> i64 {
            let conn = Conn::open_file(db_path.display().to_string()).unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS cnt FROM message_recipients", &[])
                .unwrap();
            rows[0].get_named("cnt").unwrap_or(0)
        }

        let (std_ack, std_body, std_attachments, std_recipients_json) =
            fetch_message_state(&standard_db);
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
        assert_eq!(recipient_row_count(&standard_db), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&std_recipients_json).unwrap(),
            serde_json::json!({"to":["TestAgent"],"cc":[],"bcc":[]})
        );

        let (strict_ack, strict_body, strict_attachments, strict_recipients_json) =
            fetch_message_state(&strict_db);
        assert_eq!(strict_ack, 0);
        assert_eq!(strict_body, "[Message body redacted]");
        assert_eq!(strict_attachments, "[]");
        assert_eq!(recipient_row_count(&strict_db), 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&strict_recipients_json).unwrap(),
            serde_json::json!({"to":[],"cc":[],"bcc":[]})
        );

        let (archive_ack, archive_body, archive_attachments, archive_recipients_json) =
            fetch_message_state(&archive_db);
        assert_eq!(archive_ack, 1);
        assert_eq!(archive_body, "body has sk-abcdef0123456789012345");
        assert!(archive_attachments.contains("download_url"));
        assert!(archive_attachments.contains("authorization"));
        assert_eq!(recipient_row_count(&archive_db), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&archive_recipients_json).unwrap(),
            serde_json::json!({"to":["TestAgent"],"cc":[],"bcc":[]})
        );
    }

    #[test]
    fn strict_scrub_removes_recipient_metadata_from_finalized_views() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());

        scrub_snapshot(&db, ScrubPreset::Strict).unwrap();
        crate::build_materialized_views(&db, false).unwrap();

        let conn = Conn::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT recipients FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let recipients: String = rows[0].get_named("recipients").unwrap();
        assert!(
            recipients.is_empty(),
            "strict export must not leak recipients"
        );
    }

    #[test]
    fn strict_scrub_removes_recipient_derived_inbox_stats() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE inbox_stats (
                agent_id INTEGER PRIMARY KEY,
                total_count INTEGER NOT NULL DEFAULT 0,
                unread_count INTEGER NOT NULL DEFAULT 0,
                ack_pending_count INTEGER NOT NULL DEFAULT 0,
                last_message_ts INTEGER
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO inbox_stats (agent_id, total_count, unread_count, ack_pending_count, last_message_ts)
             VALUES (1, 4, 2, 1, 12345)",
        )
        .unwrap();

        scrub_snapshot(&db, ScrubPreset::Strict).unwrap();

        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM inbox_stats", &[])
            .unwrap();
        let remaining: i64 = rows[0].get_named("cnt").unwrap_or(0);
        assert_eq!(
            remaining, 0,
            "strict export must not leak recipient-derived inbox aggregates"
        );
    }

    #[test]
    fn standard_scrub_refreshes_existing_export_shadow_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_sync(
            "UPDATE messages SET subject = ?, body_md = ?, attachments = ? WHERE id = 1",
            &[
                SqlValue::Text("subject sk-abcdef0123456789012345".to_string()),
                SqlValue::Text("body has sk-abcdef0123456789012345".to_string()),
                SqlValue::Text(
                    r#"[{"type":"file","download_url":"https://secret.example.com","path":"data.json"}]"#
                        .to_string(),
                ),
            ],
        )
        .unwrap();

        crate::build_materialized_views(&db, false).unwrap();
        crate::create_performance_indexes(&db).unwrap();

        scrub_snapshot(&db, ScrubPreset::Standard).unwrap();

        let message_rows = conn
            .query_sync(
                "SELECT latest_snippet FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let latest_snippet: String = message_rows[0].get_named("latest_snippet").unwrap();
        assert_eq!(latest_snippet, "body has [REDACTED]");

        let lower_rows = conn
            .query_sync("SELECT subject_lower FROM messages WHERE id = 1", &[])
            .unwrap();
        let subject_lower: String = lower_rows[0].get_named("subject_lower").unwrap();
        assert_eq!(subject_lower, "subject [redacted]");
    }

    #[test]
    fn strict_scrub_refreshes_existing_finalized_views_without_manual_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();
        conn.execute_sync(
            "UPDATE messages SET attachments = ? WHERE id = 1",
            &[SqlValue::Text(
                r#"[{"type":"file","path":"data.json","bytes":42}]"#.to_string(),
            )],
        )
        .unwrap();

        crate::build_materialized_views(&db, false).unwrap();
        scrub_snapshot(&db, ScrubPreset::Strict).unwrap();

        let overview_rows = conn
            .query_sync(
                "SELECT recipients, attachment_count FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let recipients: String = overview_rows[0].get_named("recipients").unwrap();
        let attachment_count: i64 = overview_rows[0].get_named("attachment_count").unwrap();
        assert!(recipients.is_empty());
        assert_eq!(attachment_count, 0);

        let attachment_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM attachments_by_message_mv WHERE message_id = 1",
                &[],
            )
            .unwrap();
        let remaining: i64 = attachment_rows[0].get_named("cnt").unwrap_or(0);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn strict_privacy_corpus_redacts_paths_and_preserves_product_proof() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();

        conn.execute_raw(
            "UPDATE projects SET human_key = '/home/ubuntu/private/acme-client' WHERE id = 1",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (2, 1, 'PeerAgent', '', '', '', '', '', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "UPDATE messages \
             SET subject = 'Deploy sk-abcdef0123456789012345', \
                 body_md = 'Bearer dGVzdF90b2tlbl9uZXN0ZWQxMjM0NTY3ODkw from /home/ubuntu/private/acme-client', \
                 ack_required = 1, \
                 attachments = '[{\"type\":\"file\",\"path\":\"/home/ubuntu/private/acme-client/secrets.txt\",\"download_url\":\"https://signed.example.com\",\"authorization\":\"Bearer abcdefghijklmnopqrstuvwxyz123456\",\"metadata\":{\"token\":\"ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789\"}}]' \
             WHERE id = 1",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE inbox_stats (
                agent_id INTEGER PRIMARY KEY,
                total_count INTEGER NOT NULL DEFAULT 0,
                unread_count INTEGER NOT NULL DEFAULT 0,
                ack_pending_count INTEGER NOT NULL DEFAULT 0,
                last_message_ts INTEGER
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservation_releases (
                reservation_id INTEGER PRIMARY KEY,
                released_ts INTEGER NOT NULL
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE products (
                id INTEGER PRIMARY KEY,
                product_uid TEXT NOT NULL,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT ''
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE product_project_links (
                id INTEGER PRIMARY KEY,
                product_id INTEGER NOT NULL,
                project_id INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT ''
            )",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO inbox_stats VALUES (1, 9, 4, 2, 123456)")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO file_reservations VALUES (10, 1, 1, '/home/ubuntu/private/acme-client/src/*.rs', 1, 'contains ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789', '', '', NULL)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO file_reservation_releases VALUES (10, 777)")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agent_links VALUES (10, 1, 1, 1, 2, 'accepted', 'Contact for /home/ubuntu/private/acme-client', '', '', NULL)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO products VALUES (10, 'prod-public', 'Public Product', '')")
            .unwrap();
        conn.execute_raw("INSERT INTO product_project_links VALUES (10, 10, 1, '')")
            .unwrap();

        let summary = scrub_snapshot(&db, ScrubPreset::Strict).unwrap();
        assert_eq!(summary.file_reservations_removed, 1);
        assert_eq!(summary.agent_links_removed, 1);
        assert_eq!(summary.recipients_cleared, 1);
        assert_eq!(summary.bodies_redacted, 1);
        assert_eq!(summary.attachments_cleared, 1);

        crate::build_materialized_views(&db, false).unwrap();
        let rows = conn
            .query_sync(
                "SELECT subject, body_md, attachments, recipients_json, p.human_key \
                 FROM messages m JOIN projects p ON p.id = m.project_id \
                 WHERE m.id = 1",
                &[],
            )
            .unwrap();
        let subject: String = rows[0].get_named("subject").unwrap();
        let body_md: String = rows[0].get_named("body_md").unwrap();
        let attachments: String = rows[0].get_named("attachments").unwrap();
        let recipients_json: String = rows[0].get_named("recipients_json").unwrap();
        let human_key: String = rows[0].get_named("human_key").unwrap();
        assert_eq!(subject, "Deploy [REDACTED]");
        assert_eq!(body_md, "[Message body redacted]");
        assert_eq!(attachments, "[]");
        assert_eq!(
            serde_json::from_str::<Value>(&recipients_json).unwrap(),
            serde_json::json!({"to":[],"cc":[],"bcc":[]})
        );
        assert_eq!(human_key, "[project path redacted: test]");

        fn table_count(conn: &Conn, table: &str) -> i64 {
            let rows = conn
                .query_sync(&format!("SELECT COUNT(*) AS cnt FROM {table}"), &[])
                .unwrap();
            rows[0].get_named("cnt").unwrap_or(0)
        }

        assert_eq!(table_count(&conn, "message_recipients"), 0);
        assert_eq!(table_count(&conn, "inbox_stats"), 0);
        assert_eq!(table_count(&conn, "file_reservations"), 0);
        assert_eq!(table_count(&conn, "file_reservation_releases"), 0);
        assert_eq!(table_count(&conn, "agent_links"), 0);

        let product_rows = conn
            .query_sync(
                "SELECT p.product_uid, p.name, l.project_id \
                 FROM products p JOIN product_project_links l ON l.product_id = p.id",
                &[],
            )
            .unwrap();
        assert_eq!(product_rows.len(), 1);
        let product_uid: String = product_rows[0].get_named("product_uid").unwrap();
        let product_name: String = product_rows[0].get_named("name").unwrap();
        let product_project_id: i64 = product_rows[0].get_named("project_id").unwrap();
        assert_eq!(product_uid, "prod-public");
        assert_eq!(product_name, "Public Product");
        assert_eq!(product_project_id, 1);

        let output = dir.path().join("site");
        crate::render_static_site(
            &db,
            &output,
            &crate::StaticRenderConfig {
                redaction: crate::ExportRedactionPolicy::from_preset(ScrubPreset::Strict),
                ..crate::StaticRenderConfig::default()
            },
        )
        .unwrap();
        let rendered_paths = [
            "viewer/pages/projects.html",
            "viewer/pages/projects/test/index.html",
            "viewer/pages/messages/1.html",
            "viewer/data/navigation.json",
            "viewer/data/search_index.json",
        ];
        let rendered = rendered_paths
            .iter()
            .map(|path| std::fs::read_to_string(output.join(path)).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        for private in [
            "/home/ubuntu/private/acme-client",
            "sk-abcdef0123456789012345",
            "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
            "PeerAgent",
        ] {
            assert!(
                !rendered.contains(private),
                "strict static export leaked private token: {private}"
            );
        }
        assert!(rendered.contains("[project path redacted: test]"));
        assert!(rendered.contains("data-redaction-reason=\"body_redacted\""));
    }

    #[test]
    fn standard_scrub_removes_secrets_from_task_description_and_rationale() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();

        // A GitHub PAT (ghp_ + 36 chars) tucked into an agent's task_description
        // and a sibling-suggestion rationale — neither is a message field, so
        // before the fix these shipped verbatim in a shared bundle's DB.
        let secret_token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        conn.execute_raw(&format!(
            "UPDATE agents SET task_description = 'Deploy with {secret_token} to prod' WHERE id = 1"
        ))
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE project_sibling_suggestions (
                id INTEGER PRIMARY KEY,
                project_a_id INTEGER NOT NULL,
                project_b_id INTEGER NOT NULL,
                score REAL NOT NULL DEFAULT 0.0,
                status TEXT NOT NULL DEFAULT 'suggested',
                rationale TEXT NOT NULL DEFAULT '',
                created_ts TEXT NOT NULL DEFAULT '',
                evaluated_ts TEXT NOT NULL DEFAULT '',
                confirmed_ts TEXT,
                dismissed_ts TEXT
            )",
        )
        .unwrap();
        conn.execute_raw(&format!(
            "INSERT INTO project_sibling_suggestions (id, project_a_id, project_b_id, score, rationale) \
             VALUES (1, 1, 1, 0.9, 'shared deploy key {secret_token}')"
        ))
        .unwrap();

        let summary = scrub_snapshot(&db, ScrubPreset::Standard).unwrap();

        let task: String = conn
            .query_sync("SELECT task_description FROM agents WHERE id = 1", &[])
            .unwrap()[0]
            .get_named("task_description")
            .unwrap();
        let rationale: String = conn
            .query_sync(
                "SELECT rationale FROM project_sibling_suggestions WHERE id = 1",
                &[],
            )
            .unwrap()[0]
            .get_named("rationale")
            .unwrap();

        assert!(
            !task.contains(secret_token),
            "task_description secret must be scrubbed, got {task:?}"
        );
        assert!(
            !rationale.contains(secret_token),
            "sibling-suggestion rationale secret must be scrubbed, got {rationale:?}"
        );
        assert!(
            summary.secrets_replaced >= 2,
            "both non-message-field secrets should be counted, got {}",
            summary.secrets_replaced
        );
    }

    #[test]
    fn standard_privacy_redacts_project_paths_in_retained_text_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let conn = Conn::open_file(db.display().to_string()).unwrap();

        conn.execute_raw(
            "UPDATE projects SET human_key = '/home/ubuntu/private/acme-client' WHERE id = 1",
        )
        .unwrap();
        conn.execute_raw(
            "UPDATE agents \
             SET task_description = 'Working in /home/ubuntu/private/acme-client' \
             WHERE id = 1",
        )
        .unwrap();
        conn.execute_raw(
            "UPDATE messages \
             SET subject = 'Open /home/ubuntu/private/acme-client', \
                 body_md = 'See /home/ubuntu/private/acme-client/src/lib.rs', \
                 attachments = '[{\"type\":\"file\",\"path\":\"/home/ubuntu/private/acme-client/secrets.txt\",\"metadata\":{\"note\":\"from /home/ubuntu/private/acme-client\"}}]' \
             WHERE id = 1",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE project_sibling_suggestions (
                id INTEGER PRIMARY KEY,
                project_a_id INTEGER NOT NULL,
                project_b_id INTEGER NOT NULL,
                score REAL NOT NULL DEFAULT 0.0,
                status TEXT NOT NULL DEFAULT 'suggested',
                rationale TEXT NOT NULL DEFAULT '',
                created_ts TEXT NOT NULL DEFAULT '',
                evaluated_ts TEXT NOT NULL DEFAULT '',
                confirmed_ts TEXT,
                dismissed_ts TEXT
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO project_sibling_suggestions \
             (id, project_a_id, project_b_id, score, rationale) \
             VALUES (1, 1, 1, 0.9, 'same checkout at /home/ubuntu/private/acme-client')",
        )
        .unwrap();

        scrub_snapshot(&db, ScrubPreset::Standard).unwrap();

        let rows = conn
            .query_sync(
                "SELECT p.human_key, a.task_description, m.subject, m.body_md, m.attachments, s.rationale \
                 FROM projects p \
                 JOIN agents a ON a.project_id = p.id \
                 JOIN messages m ON m.project_id = p.id \
                 JOIN project_sibling_suggestions s ON s.project_a_id = p.id \
                 WHERE p.id = 1",
                &[],
            )
            .unwrap();
        let rendered = [
            rows[0].get_named::<String>("human_key").unwrap(),
            rows[0].get_named::<String>("task_description").unwrap(),
            rows[0].get_named::<String>("subject").unwrap(),
            rows[0].get_named::<String>("body_md").unwrap(),
            rows[0].get_named::<String>("attachments").unwrap(),
            rows[0].get_named::<String>("rationale").unwrap(),
        ]
        .join("\n");

        assert!(
            !rendered.contains("/home/ubuntu/private/acme-client"),
            "standard scrub must not retain raw local project paths: {rendered}"
        );
        assert!(rendered.contains("[project path redacted: test]"));
        assert!(rendered.contains("secrets.txt"));
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
        let conn = Conn::open_file(snapshot.display().to_string()).unwrap();
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
        let conn = Conn::open_file(db_path.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, created_ts TEXT DEFAULT '', recipients_json TEXT NOT NULL DEFAULT '{}', attachments TEXT DEFAULT '[]')",
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
        conn.execute_raw(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (1, 1, 1, NULL, 'Hi', 'Hello world', 'normal', 0, '', '{\"to\":[\"TestAgent\"],\"cc\":[],\"bcc\":[]}', '[]')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 1, 'to', NULL, NULL)")
            .unwrap();
        db_path
    }

    #[cfg(unix)]
    #[test]
    fn scrub_snapshot_rejects_symlinked_snapshot() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let db = create_fixture_db(dir.path());
        let linked = dir.path().join("linked.sqlite3");
        symlink(&db, &linked).unwrap();

        let err = scrub_snapshot(&linked, ScrubPreset::Standard)
            .expect_err("symlinked snapshots must fail validation");
        assert!(matches!(err, ShareError::Validation { .. }));
        assert!(err.to_string().contains("real file"));
    }

    #[test]
    fn scrub_text_finds_stripe_keys() {
        let (result, count) = scrub_text("Use sk_live_abc123def456ghi7 for prod");
        assert_eq!(result, "Use [REDACTED] for prod");
        assert_eq!(count, 1);

        let (result2, count2) = scrub_text("Test pk_test_0123456789abcdef");
        assert_eq!(result2, "Test [REDACTED]");
        assert_eq!(count2, 1);
    }

    #[test]
    fn scrub_text_finds_azure_keys() {
        let (result, count) =
            scrub_text("AccountKey=abc123def456ghi789jkl012mno345pqr678stu901vwx234y+z=");
        assert_eq!(result, "[REDACTED]");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_finds_google_api_keys() {
        let (result, count) = scrub_text("key=AIzaSyA1234567890abcdefghijklmnopqrstuv");
        assert_eq!(result, "key=[REDACTED]");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_finds_npm_tokens() {
        let (result, count) =
            scrub_text("//registry.npmjs.org/:_authToken=npm_abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(result, "//registry.npmjs.org/:_authToken=[REDACTED]");
        assert_eq!(count, 1);
    }

    #[test]
    fn scrub_text_anthropic_before_generic_sk() {
        // sk-ant- should be caught by the Anthropic-specific pattern
        let input = "sk-ant-api03-ABCdefGHIjklMNOpqrSTUvwxyz0123456789";
        let (result, _count) = scrub_text(input);
        assert_eq!(result, "[REDACTED]");
    }
}
