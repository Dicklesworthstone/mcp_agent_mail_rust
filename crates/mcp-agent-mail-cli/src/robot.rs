//! Robot output types for agent-optimized CLI commands.
//!
//! Provides the `OutputFormat` selector, `RobotEnvelope<T>` response wrapper,
//! and the `format_output()` dispatcher used by all `am robot *` commands.

#![allow(clippy::module_name_repetitions)]

use asupersync::Outcome;
use chrono::Utc;
use clap::{Args, Subcommand};
use fastmcp::McpErrorCode;
use fastmcp::prelude::{McpContext, McpError, McpResult};
use serde::Serialize;
use sqlmodel_core::Value;
use std::path::{Path, PathBuf};

use crate::CliError;

fn has_file_reservations_released_ts_column(conn: &DbConn) -> bool {
    conn.query_sync("PRAGMA table_info(file_reservations)", &[])
        .ok()
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.get_named::<String>("name").ok().as_deref() == Some("released_ts"))
        })
}

fn legacy_active_reservation_predicate_sql(
    table_ref: &str,
    has_legacy_released_ts_column: bool,
) -> String {
    if !has_legacy_released_ts_column {
        return "1 = 1".to_string();
    }
    let table_ref = table_ref.trim().trim_end_matches('.');
    let predicate = mcp_agent_mail_db::queries::ACTIVE_RESERVATION_LEGACY_PREDICATE;
    if table_ref.is_empty() || table_ref == "file_reservations" {
        predicate.to_string()
    } else {
        predicate
            .replace("released_ts", &format!("{table_ref}.released_ts"))
            .replace("file_reservations.id", &format!("{table_ref}.id"))
    }
}

fn has_file_reservation_release_ledger(conn: &DbConn) -> bool {
    conn.query_sync("PRAGMA table_info(file_reservation_releases)", &[])
        .ok()
        .is_some_and(|rows| !rows.is_empty())
}

fn active_reservation_release_join_sql(
    has_release_ledger: bool,
    table_ref: &str,
    release_alias: &str,
) -> String {
    if has_release_ledger {
        format!(
            " LEFT JOIN file_reservation_releases {release_alias} ON {release_alias}.reservation_id = {table_ref}.id"
        )
    } else {
        String::new()
    }
}

fn active_reservation_filter_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
    table_ref: &str,
    release_alias: &str,
) -> String {
    let legacy = legacy_active_reservation_predicate_sql(table_ref, has_legacy_released_ts_column);
    match (has_release_ledger, has_legacy_released_ts_column) {
        (true, true) => format!("({legacy}) AND {release_alias}.reservation_id IS NULL"),
        (true, false) => format!("{release_alias}.reservation_id IS NULL"),
        (false, _) => legacy,
    }
}

fn reservation_released_ts_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
    table_ref: &str,
    release_alias: &str,
) -> String {
    let table_ref = table_ref.trim().trim_end_matches('.');
    let legacy_release_expr = has_legacy_released_ts_column.then(|| {
        format!(
            "CASE \
                 WHEN {table_ref}.released_ts IS NULL THEN NULL \
                 WHEN typeof({table_ref}.released_ts) = 'text' THEN \
                     CAST(strftime('%s', REPLACE(REPLACE({table_ref}.released_ts, 'T', ' '), 'Z', '')) AS INTEGER) * 1000000 + \
                     CASE WHEN instr({table_ref}.released_ts, '.') > 0 \
                          THEN CAST(substr({table_ref}.released_ts || '000000', instr({table_ref}.released_ts, '.') + 1, 6) AS INTEGER) \
                          ELSE 0 \
                     END \
                 ELSE {table_ref}.released_ts \
             END"
        )
    });
    match (has_release_ledger, legacy_release_expr) {
        (true, Some(legacy_release_expr)) => {
            format!("COALESCE({release_alias}.released_ts, {legacy_release_expr})")
        }
        (true, None) => format!("{release_alias}.released_ts"),
        (false, Some(legacy_release_expr)) => legacy_release_expr,
        (false, None) => "NULL".to_string(),
    }
}

fn reservation_is_released(released_ts: Option<i64>) -> bool {
    released_ts.is_some_and(|ts| ts > 0)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn truncated_f64_to_i64(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let truncated = value.trunc();
    if !(i64::MIN as f64..=i64::MAX as f64).contains(&truncated) {
        return None;
    }
    Some(truncated as i64)
}

fn value_to_micros(value: &sqlmodel_core::Value) -> Option<i64> {
    match value {
        sqlmodel_core::Value::Null => None,
        sqlmodel_core::Value::Bool(v) => Some(i64::from(*v)),
        sqlmodel_core::Value::TinyInt(v) => Some(i64::from(*v)),
        sqlmodel_core::Value::SmallInt(v) => Some(i64::from(*v)),
        sqlmodel_core::Value::Int(v) => Some(i64::from(*v)),
        sqlmodel_core::Value::BigInt(v)
        | sqlmodel_core::Value::Time(v)
        | sqlmodel_core::Value::Timestamp(v)
        | sqlmodel_core::Value::TimestampTz(v) => Some(*v),
        sqlmodel_core::Value::Float(v) => truncated_f64_to_i64(f64::from(*v)),
        sqlmodel_core::Value::Double(v) => truncated_f64_to_i64(*v),
        sqlmodel_core::Value::Decimal(text) | sqlmodel_core::Value::Text(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("none")
            {
                return None;
            }
            if let Ok(parsed) = trimmed.parse::<i64>() {
                return Some(parsed);
            }
            if let Ok(parsed) = trimmed.parse::<f64>()
                && let Some(parsed) = truncated_f64_to_i64(parsed)
            {
                return Some(parsed);
            }
            mcp_agent_mail_db::iso_to_micros(trimmed)
        }
        sqlmodel_core::Value::Date(v) => Some(i64::from(*v)),
        sqlmodel_core::Value::Bytes(_)
        | sqlmodel_core::Value::Uuid(_)
        | sqlmodel_core::Value::Json(_)
        | sqlmodel_core::Value::Array(_)
        | sqlmodel_core::Value::Default => None,
    }
}

fn canonical_thread_ref_for_row(message_id: i64, thread_id: Option<&str>) -> String {
    canonical_thread_ref(message_id, thread_id.unwrap_or_default())
}

fn status_alert_hints(anomalies: &[AnomalyCard]) -> Vec<(String, String, Option<String>)> {
    anomalies
        .iter()
        .filter(|anomaly| anomaly.severity == "warn")
        .map(|anomaly| {
            let action_hint = match anomaly.category.as_str() {
                "ack_sla" => Some("am robot inbox --ack-overdue".to_string()),
                "reservation_expiry" => Some("am robot reservations --expiring=5".to_string()),
                _ => None,
            };
            (
                anomaly.severity.clone(),
                anomaly.headline.clone(),
                action_hint,
            )
        })
        .collect()
}

fn append_status_anomaly_alerts<T: Serialize>(
    mut env: RobotEnvelope<T>,
    anomalies: &[AnomalyCard],
) -> RobotEnvelope<T> {
    for (severity, headline, action) in status_alert_hints(anomalies) {
        let already_present = env
            ._alerts
            .iter()
            .any(|alert| alert.severity == severity && alert.summary == headline);
        if !already_present {
            env = env.with_alert(severity, headline, action);
        }
    }
    env
}

#[derive(Debug, Clone)]
struct ParsedAttachment {
    name: String,
    size_text: String,
    size_bytes: usize,
    mime_type: String,
}

fn parse_attachment_size_bytes(value: &serde_json::Value) -> Option<usize> {
    value
        .get("bytes")
        .and_then(|v| v.as_u64())
        .or_else(|| value.get("size").and_then(|v| v.as_u64()))
        .or_else(|| {
            value
                .get("size")
                .and_then(|v| v.as_str())
                .and_then(|raw| raw.parse::<u64>().ok())
        })
        .and_then(|bytes| usize::try_from(bytes).ok())
}

fn parse_attachment_name(value: &serde_json::Value) -> String {
    value
        .get("name")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            value
                .get("path")
                .and_then(|v| v.as_str())
                .and_then(|path| {
                    let file_part = path.rsplit('/').next().unwrap_or(path);
                    let name_only = file_part.split('?').next().unwrap_or(file_part);
                    let name_only = name_only.split('#').next().unwrap_or(name_only);
                    if name_only.is_empty() {
                        None
                    } else {
                        Some(name_only.to_string())
                    }
                })
                .unwrap_or_else(|| "attachment".to_string())
        })
}

fn parse_attachment_mime_type(value: &serde_json::Value) -> String {
    value
        .get("media_type")
        .or_else(|| value.get("content_type"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            value
                .get("type")
                .and_then(|v| v.as_str())
                .filter(|kind| !matches!(*kind, "file" | "inline" | "auto"))
        })
        .unwrap_or("application/octet-stream")
        .to_string()
}

fn parse_attachments_json(attachments_json: &str) -> Vec<ParsedAttachment> {
    serde_json::from_str::<serde_json::Value>(attachments_json)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .map(|items| {
            items
                .iter()
                .map(|value| {
                    let size_bytes = parse_attachment_size_bytes(value).unwrap_or(0);
                    let size_text = parse_attachment_size_bytes(value)
                        .map(|bytes| bytes.to_string())
                        .or_else(|| {
                            value
                                .get("size")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "unknown".to_string());
                    ParsedAttachment {
                        name: parse_attachment_name(value),
                        size_text,
                        size_bytes,
                        mime_type: parse_attachment_mime_type(value),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_attachment_values(attachments_json: Option<&str>) -> Vec<serde_json::Value> {
    attachments_json
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(raw).ok())
        .unwrap_or_default()
}

fn redact_navigate_database_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://")
        && let Some((_creds, host)) = rest.rsplit_once('@')
    {
        return format!("{scheme}://****@{host}");
    }
    url.to_string()
}

fn navigate_workspace_root_from(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let cargo_toml = dir.join("Cargo.toml");
        if !cargo_toml.exists() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&cargo_toml)
            && content.contains("[workspace]")
        {
            return Some(dir.to_path_buf());
        }
    }
    None
}

const NAVIGATE_I64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;
const NAVIGATE_I64_MAX_F64: f64 = 9_223_372_036_854_775_807.0;

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn navigate_ts_f64_to_rfc3339(timestamp: f64) -> Option<String> {
    if !timestamp.is_finite() {
        return None;
    }
    let secs_f = timestamp.floor();
    if !(NAVIGATE_I64_MIN_F64..=NAVIGATE_I64_MAX_F64).contains(&secs_f) {
        return None;
    }
    let secs = secs_f as i64;
    let nanos = ((timestamp - secs_f) * 1e9).clamp(0.0, 999_999_999.0) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).map(|dt| dt.to_rfc3339())
}

fn build_navigate_tooling_locks_data(lock_info: serde_json::Value) -> serde_json::Value {
    let raw_locks = lock_info
        .get("locks")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut locks: Vec<serde_json::Value> = raw_locks
        .iter()
        .filter_map(|lock| {
            let path = lock.get("path").and_then(serde_json::Value::as_str)?;
            let project_slug = path
                .split("projects/")
                .nth(1)
                .and_then(|value| value.split('/').next())
                .filter(|value| !value.is_empty())?
                .to_string();
            let pid = lock
                .get("owner")
                .and_then(|owner| owner.get("pid"))
                .and_then(serde_json::Value::as_u64)?;
            let acquired_ts = lock
                .get("owner")
                .and_then(|owner| owner.get("created_ts"))
                .and_then(serde_json::Value::as_f64)
                .and_then(navigate_ts_f64_to_rfc3339)
                .unwrap_or_default();

            Some(serde_json::json!({
                "project_slug": project_slug,
                "holder": format!("pid:{pid}"),
                "acquired_ts": acquired_ts,
            }))
        })
        .collect();
    locks.sort_by(|left, right| {
        let left_project = left
            .get("project_slug")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let right_project = right
            .get("project_slug")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let left_holder = left
            .get("holder")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let right_holder = right
            .get("holder")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        left_project
            .cmp(right_project)
            .then(left_holder.cmp(right_holder))
    });

    let total = locks.len() as u64;
    let total_raw = raw_locks.len() as u64;
    let metadata_missing = total_raw.saturating_sub(total);
    let stale = locks
        .iter()
        .filter(|lock| {
            lock.get("holder")
                .and_then(serde_json::Value::as_str)
                .and_then(|holder| holder.strip_prefix("pid:"))
                .and_then(|pid| pid.parse::<u32>().ok())
                .is_some_and(|pid| {
                    #[cfg(target_os = "linux")]
                    {
                        !Path::new(&format!("/proc/{pid}")).exists()
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = pid;
                        false
                    }
                })
        })
        .count() as u64;

    serde_json::json!({
        "locks": locks,
        "summary": {
            "total": total,
            "active": total.saturating_sub(stale),
            "stale": stale,
            "metadata_missing": metadata_missing,
        },
    })
}

// ── Output format ────────────────────────────────────────────────────────────

/// Output format for robot commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    /// Token-optimized TOON encoding (default for robot commands).
    Toon,
    /// Full JSON — for piping to jq or programmatic access.
    Json,
    /// Markdown prose — for thread/message rendering.
    Markdown,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Toon => f.write_str("toon"),
            Self::Json => f.write_str("json"),
            Self::Markdown => f.write_str("markdown"),
        }
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "toon" => Ok(Self::Toon),
            "json" => Ok(Self::Json),
            "markdown" | "md" => Ok(Self::Markdown),
            other => Err(format!(
                "unknown output format: {other} (expected toon, json, or markdown)"
            )),
        }
    }
}

impl OutputFormat {
    /// Auto-detect the best format when the user didn't specify one.
    ///
    /// - `prose_hint` true (thread/message) → Markdown
    /// - stdout is a TTY → Toon (compact, human-friendly)
    /// - Otherwise (piped) → Json (machine-readable)
    #[must_use]
    pub fn auto_detect(prose_hint: bool) -> Self {
        if prose_hint {
            Self::Markdown
        } else if crate::output::is_tty() {
            Self::Toon
        } else {
            Self::Json
        }
    }

    /// Resolve an explicit format or auto-detect.
    #[must_use]
    pub fn resolve(explicit: Option<Self>, prose_hint: bool) -> Self {
        explicit.unwrap_or_else(|| Self::auto_detect(prose_hint))
    }
}

// ── Envelope types ───────────────────────────────────────────────────────────

/// Standard response envelope wrapping every robot command's output.
///
/// `_meta` is always present. `_alerts` and `_actions` are omitted when empty
/// to keep output clean. The `data` payload is flattened to top level via
/// `#[serde(flatten)]`.
#[derive(Debug, Serialize)]
pub struct RobotEnvelope<T: Serialize> {
    pub _meta: RobotMeta,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub _alerts: Vec<RobotAlert>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub _actions: Vec<String>,
    #[serde(flatten)]
    pub data: T,
}

/// Infrastructure metadata attached to every robot response.
#[derive(Debug, Serialize)]
pub struct RobotMeta {
    pub command: String,
    pub timestamp: String,
    pub format: String,
    pub version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// An alert surfacing anomalies detected during data collection.
#[derive(Debug, Serialize)]
pub struct RobotAlert {
    pub severity: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

// ── Envelope builder ─────────────────────────────────────────────────────────

impl<T: Serialize> RobotEnvelope<T> {
    /// Create a new envelope with the given command name, format, and data payload.
    pub fn new(command: impl Into<String>, format: OutputFormat, data: T) -> Self {
        Self {
            _meta: RobotMeta {
                command: command.into(),
                timestamp: Utc::now().to_rfc3339(),
                format: format.to_string(),
                version: "1.0",
                project: None,
                agent: None,
            },
            _alerts: Vec::new(),
            _actions: Vec::new(),
            data,
        }
    }

    /// Add an alert to the envelope.
    pub fn with_alert(
        mut self,
        severity: impl Into<String>,
        summary: impl Into<String>,
        action: Option<String>,
    ) -> Self {
        self._alerts.push(RobotAlert {
            severity: severity.into(),
            summary: summary.into(),
            action,
        });
        self
    }

    /// Add a suggested action command.
    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self._actions.push(action.into());
        self
    }
}

// ── Markdown trait ────────────────────────────────────────────────────────────

/// Trait for data types that support custom markdown rendering.
///
/// Most robot commands use TOON/JSON. Only a few (thread, message, search)
/// benefit from markdown. Types implementing this trait get custom markdown
/// output instead of falling back to TOON.
pub trait MarkdownRenderable {
    fn to_markdown(&self, meta: &RobotMeta, alerts: &[RobotAlert], actions: &[String]) -> String;
}

// ── Format dispatcher ────────────────────────────────────────────────────────

/// Serialize a `RobotEnvelope<T>` into the requested output format.
pub fn format_output<T: Serialize>(
    envelope: &RobotEnvelope<T>,
    format: OutputFormat,
) -> Result<String, CliError> {
    match format {
        OutputFormat::Json => {
            serde_json::to_string_pretty(envelope).map_err(|e| CliError::Format(e.to_string()))
        }
        OutputFormat::Toon => {
            let json_str =
                serde_json::to_string(envelope).map_err(|e| CliError::Format(e.to_string()))?;
            toon::json_to_toon(&json_str).map_err(|e| CliError::Format(e.to_string()))
        }
        OutputFormat::Markdown => {
            // Markdown falls back to TOON for types that don't implement MarkdownRenderable.
            // Commands that support markdown should call to_markdown() directly before
            // reaching this generic path.
            let json_str =
                serde_json::to_string(envelope).map_err(|e| CliError::Format(e.to_string()))?;
            toon::json_to_toon(&json_str).map_err(|e| CliError::Format(e.to_string()))
        }
    }
}

/// Format with markdown support for types that implement `MarkdownRenderable`.
pub fn format_output_md<T: Serialize + MarkdownRenderable>(
    envelope: &RobotEnvelope<T>,
    format: OutputFormat,
) -> Result<String, CliError> {
    if format == OutputFormat::Markdown {
        return Ok(envelope.data.to_markdown(
            &envelope._meta,
            &envelope._alerts,
            &envelope._actions,
        ));
    }
    format_output(envelope, format)
}

// ── Format auto-detection ────────────────────────────────────────────────────

/// Whether a robot subcommand is prose-heavy (thread/message rendering).
pub fn is_prose_command(subcmd: &str) -> bool {
    matches!(subcmd, "thread" | "message")
}

/// Resolve the output format from an explicit flag or auto-detection.
///
/// Auto-detection logic:
/// - Explicit `--format` flag → use that
/// - Prose-heavy command (thread/message) → Markdown
/// - stdout is a TTY → TOON (human at terminal)
/// - stdout is piped → JSON (machine-readable)
pub fn resolve_format(explicit: Option<OutputFormat>, subcmd: &str) -> OutputFormat {
    if let Some(fmt) = explicit {
        return fmt;
    }
    if is_prose_command(subcmd) {
        return OutputFormat::Markdown;
    }
    resolve_format_for_terminal()
}

/// Resolve format based on terminal detection only (no command context).
pub fn resolve_format_for_terminal() -> OutputFormat {
    if crate::output::is_tty() {
        OutputFormat::Toon
    } else {
        OutputFormat::Json
    }
}

// ── Domain response types ────────────────────────────────────────────────────
//
// Flat, TOON-friendly structs returned by robot commands. Each derives Serialize
// so it can be used as `RobotEnvelope<T>` data.

/// robot status — dashboard synthesis.
#[derive(Debug, Serialize)]
pub struct StatusData {
    pub health: String,
    pub unread: usize,
    pub urgent: usize,
    pub ack_required: usize,
    pub ack_overdue: usize,
    pub active_reservations: usize,
    pub reservations_expiring_soon: usize,
    pub active_agents: usize,
    pub recent_messages: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub my_reservations: Vec<ReservationEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_threads: Vec<ThreadSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub anomalies: Vec<AnomalyCard>,
}

/// File reservation entry for status/reservation display.
#[derive(Debug, Clone, Serialize)]
pub struct ReservationEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub path: String,
    pub exclusive: bool,
    pub remaining_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
}

/// Summary entry for a thread (used in status and overview).
#[derive(Debug, Serialize)]
pub struct ThreadSummary {
    pub id: String,
    pub subject: String,
    pub participants: usize,
    pub messages: usize,
    pub last_activity: String,
}

/// robot inbox — actionable inbox entry.
#[derive(Debug, Serialize)]
pub struct InboxEntry {
    pub id: i64,
    pub priority: String,
    pub from: String,
    pub subject: String,
    pub thread: String,
    pub age: String,
    pub ack_status: String,
    pub importance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
}

/// robot timeline — chronological event.
#[derive(Debug, Serialize)]
pub struct TimelineEvent {
    pub seq: usize,
    pub timestamp: String,
    pub kind: String,
    pub summary: String,
    pub source: String,
}

/// robot overview — per-project summary.
#[derive(Debug, Serialize)]
pub struct OverviewProject {
    pub slug: String,
    pub unread: usize,
    pub urgent: usize,
    pub ack_overdue: usize,
    pub reservations: usize,
}

/// robot thread — single message in thread rendering.
#[derive(Debug, Serialize)]
pub struct ThreadMessage {
    pub position: usize,
    pub from: String,
    pub to: String,
    pub age: String,
    pub importance: String,
    pub ack: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl MarkdownRenderable for Vec<ThreadMessage> {
    fn to_markdown(&self, meta: &RobotMeta, _alerts: &[RobotAlert], _actions: &[String]) -> String {
        let mut md = format!("# Thread: {}\n\n", meta.command);
        for msg in self {
            md.push_str(&format!(
                "## [{pos}] {from} → {to} ({age})\n**{subject}**\n\n{body}\n\n---\n\n",
                pos = msg.position,
                from = msg.from,
                to = msg.to,
                age = msg.age,
                subject = msg.subject,
                body = msg.body.as_deref().unwrap_or(""),
            ));
        }
        md
    }
}

/// robot search — search result entry.
#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub id: i64,
    pub relevance: f64,
    pub from: String,
    pub subject: String,
    pub thread: String,
    pub snippet: String,
    pub age: String,
}

/// robot message — full message with context.
#[derive(Debug, Serialize)]
pub struct MessageContext {
    pub id: i64,
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_model: Option<String>,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    pub thread: String,
    pub position: usize,
    pub total_in_thread: usize,
    pub importance: String,
    pub ack_status: String,
    pub created: String,
    pub age: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentInfo>,
}

/// Attachment metadata for message context.
#[derive(Debug, Serialize)]
pub struct AttachmentInfo {
    pub name: String,
    pub size: String,
    #[serde(rename = "type")]
    pub mime_type: String,
}

impl MarkdownRenderable for MessageContext {
    fn to_markdown(
        &self,
        _meta: &RobotMeta,
        _alerts: &[RobotAlert],
        _actions: &[String],
    ) -> String {
        let sender_info = match (&self.from_program, &self.from_model) {
            (Some(p), Some(m)) => format!("{} ({p}, {m})", self.from),
            _ => self.from.clone(),
        };
        let mut md = format!(
            "## Message #{id} | Thread: {thread} ({pos} of {total})\n\n\
             **From:** {sender}  \n\
             **To:** {to}  \n\
             **Subject:** {subject}  \n\
             **Importance:** {importance} | **Ack:** {ack}  \n\
             **Sent:** {created} ({age})\n\n---\n\n{body}\n",
            id = self.id,
            thread = self.thread,
            pos = self.position,
            total = self.total_in_thread,
            sender = sender_info,
            to = self.to.join(", "),
            subject = self.subject,
            importance = self.importance,
            ack = self.ack_status,
            created = self.created,
            age = self.age,
            body = self.body,
        );

        if !self.attachments.is_empty() {
            md.push_str(&format!("\n**Attachments:** {}\n", self.attachments.len()));
            for att in &self.attachments {
                md.push_str(&format!(
                    "- {} ({}, {})\n",
                    att.name, att.size, att.mime_type
                ));
            }
        }

        if let Some(prev) = &self.previous {
            md.push_str(&format!("\n**\u{2190} Previous:** {prev}\n"));
        }
        if let Some(next) = &self.next {
            md.push_str(&format!("**\u{2192} Next:** {next}\n"));
        }
        md
    }
}

/// robot metrics — tool performance entry.
#[derive(Debug, Serialize)]
pub struct MetricEntry {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
    pub error_pct: f64,
    pub avg_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

/// robot health — system probe entry.
#[derive(Debug, Serialize)]
pub struct HealthProbe {
    pub name: String,
    pub status: String,
    pub latency_ms: f64,
    pub detail: String,
}

/// robot analytics — anomaly insight.
#[derive(Debug, Clone, Serialize)]
pub struct AnomalyCard {
    pub severity: String,
    pub confidence: f64,
    pub category: String,
    pub headline: String,
    pub rationale: String,
    pub remediation: String,
}

/// robot agents — agent roster entry.
#[derive(Debug, Serialize)]
pub struct AgentRow {
    pub name: String,
    pub program: String,
    pub model: String,
    pub last_active: String,
    pub msg_count: usize,
    pub status: String,
}

/// robot contacts — contact entry.
#[derive(Debug, Serialize)]
pub struct ContactRow {
    pub from: String,
    pub to: String,
    pub status: String,
    pub policy: String,
    pub reason: String,
    pub updated: String,
}

/// robot projects — project entry.
#[derive(Debug, Serialize)]
pub struct ProjectRow {
    pub slug: String,
    pub path: String,
    pub agents: usize,
    pub messages: usize,
    pub reservations: usize,
    pub created: String,
}

/// robot attachments — attachment entry.
#[derive(Debug, Serialize)]
pub struct AttachmentRow {
    pub r#type: String,
    pub size: usize,
    pub sender: String,
    pub subject: String,
    pub message_id: i64,
    pub project: String,
}

// ── Robot subcommand scaffold ────────────────────────────────────────────────

/// Shared arguments for all `am robot` subcommands.
#[derive(Debug, Args)]
pub struct RobotArgs {
    /// Output format: toon (default at TTY), json (default when piped), md (for thread/message).
    #[arg(long, global = true, value_parser = parse_output_format)]
    pub format: Option<OutputFormat>,

    /// Project key (absolute path or slug). Falls back to AGENT_MAIL_PROJECT, then CWD.
    #[arg(long, global = true)]
    pub project: Option<String>,

    /// Agent name. Falls back to AGENT_MAIL_AGENT, then AGENT_NAME.
    #[arg(long, global = true)]
    pub agent: Option<String>,

    #[command(subcommand)]
    pub command: RobotSubcommand,
}

fn parse_output_format(s: &str) -> Result<OutputFormat, String> {
    s.parse()
}

/// All `am robot` subcommands (16 commands across 4 tracks).
#[derive(Debug, Subcommand)]
pub enum RobotSubcommand {
    // ── Track 2: Situational Awareness ──────────────────────────────────
    /// Dashboard synthesis: health, inbox counts, activity, anomalies, reservations, top threads.
    Status,

    /// Actionable inbox with priority ordering, urgency/ack synthesis.
    Inbox {
        /// Show only urgent messages.
        #[arg(long)]
        urgent: bool,
        /// Show only ack-overdue messages.
        #[arg(long)]
        ack_overdue: bool,
        /// Show only unread messages.
        #[arg(long)]
        unread: bool,
        /// Show all messages (no filtering).
        #[arg(long)]
        all: bool,
        /// Maximum messages to return.
        #[arg(long)]
        limit: Option<usize>,
        /// Include message bodies in output.
        #[arg(long)]
        include_bodies: bool,
    },

    /// Events since last check with temporal filters.
    Timeline {
        /// ISO-8601 timestamp — show events after this time.
        #[arg(long)]
        since: Option<String>,
        /// Filter by event kind (message, reservation, agent).
        #[arg(long)]
        kind: Option<String>,
        /// Filter by event source.
        #[arg(long)]
        source: Option<String>,
    },

    /// Cross-project unified summary (per-project unread/urgent/ack counts).
    Overview,

    // ── Track 3: Context & Discovery ────────────────────────────────────
    /// Full conversation rendering for a thread.
    Thread {
        /// Thread ID.
        id: String,
        /// Maximum messages in thread.
        #[arg(long)]
        limit: Option<usize>,
        /// Show messages after this timestamp.
        #[arg(long)]
        since: Option<String>,
    },

    /// Full-text search with facets and relevance scores.
    Search {
        /// Search query.
        query: String,
        /// Filter by message kind.
        #[arg(long)]
        kind: Option<String>,
        /// Filter by importance level.
        #[arg(long)]
        importance: Option<String>,
        /// Limit results to messages after this timestamp.
        #[arg(long)]
        since: Option<String>,
    },

    /// Single message with thread position, adjacent messages, sender info.
    Message {
        /// Message ID.
        id: i64,
    },

    /// Resolve any resource:// URI and return in robot format.
    Navigate {
        /// Resource URI (e.g. resource://inbox/AgentName).
        uri: String,
    },

    // ── Track 4: Monitoring & Analytics ─────────────────────────────────
    /// File reservations with TTL warnings, expiring-soon alerts, conflict detection.
    Reservations {
        /// Filter by agent name.
        #[arg(long)]
        agent: Option<String>,
        /// Show reservations for all agents, not just the selected agent.
        #[arg(long)]
        all: bool,
        /// Show only conflicting reservations.
        #[arg(long)]
        conflicts: bool,
        /// Warn about reservations expiring within N minutes.
        #[arg(long)]
        expiring: Option<u32>,
    },

    /// Tool performance summary (calls, errors, error%, latency percentiles).
    Metrics,

    /// System diagnostics synthesis (probes, DB pool, disk, anomalies).
    Health,

    /// Anomaly insights with severity, confidence, remediation commands.
    Analytics,

    // ── Track 5: Entity Views ───────────────────────────────────────────
    /// Agent roster with activity status, program, model.
    Agents {
        /// Show only active agents.
        #[arg(long)]
        active: bool,
        /// Sort by field (name, last_active, msg_count).
        #[arg(long)]
        sort: Option<String>,
    },

    /// Contact graph with policy surface, pending requests.
    Contacts,

    /// Project summary with per-project agent/message/reservation counts.
    Projects,

    /// Attachment inventory with type, size, provenance, storage mode.
    Attachments,

    // ── Track 5: Air Traffic Controller ─────────────────────────────────
    /// ATC status: liveness assessments, conflicts, calibration, decisions.
    Atc {
        /// Show recent ATC decisions.
        #[arg(long)]
        decisions: bool,
        /// Show agent liveness assessments.
        #[arg(long)]
        liveness: bool,
        /// Show conflict graph.
        #[arg(long)]
        conflicts: bool,
        /// Show session summary.
        #[arg(long)]
        summary: bool,
        /// Maximum items to show.
        #[arg(long)]
        limit: Option<usize>,
    },
}

impl RobotSubcommand {
    /// Whether this command's output is prose-heavy (prefers Markdown by default).
    #[must_use]
    pub const fn is_prose(&self) -> bool {
        matches!(self, Self::Thread { .. } | Self::Message { .. })
    }

    /// Name string for the subcommand (used in envelope `_meta.command`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Status => "robot status",
            Self::Inbox { .. } => "robot inbox",
            Self::Timeline { .. } => "robot timeline",
            Self::Overview => "robot overview",
            Self::Thread { .. } => "robot thread",
            Self::Search { .. } => "robot search",
            Self::Message { .. } => "robot message",
            Self::Navigate { .. } => "robot navigate",
            Self::Reservations { .. } => "robot reservations",
            Self::Metrics => "robot metrics",
            Self::Health => "robot health",
            Self::Analytics => "robot analytics",
            Self::Agents { .. } => "robot agents",
            Self::Contacts => "robot contacts",
            Self::Projects => "robot projects",
            Self::Attachments => "robot attachments",
            Self::Atc { .. } => "robot atc",
        }
    }
}

// ── DB helpers ──────────────────────────────────────────────────────────────

use mcp_agent_mail_db::DbConn;

/// Resolve a project by slug or human_key.
fn resolve_project_sync(conn: &DbConn, key: &str) -> Result<(i64, String), CliError> {
    let key = key.trim();
    // Try slug first
    let rows = conn
        .query_sync(
            "SELECT id, slug FROM projects WHERE lower(slug) = lower(?)",
            &[Value::Text(key.to_string())],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
    if let Some(row) = rows.first() {
        let id: i64 = row.get_as(0).unwrap_or(0);
        let slug: String = row.get_as(1).unwrap_or_default();
        if id > 0 && !slug.is_empty() {
            return Ok((id, slug));
        }
    }
    // Try human_key
    let rows = conn
        .query_sync(
            "SELECT id, slug FROM projects WHERE human_key = ?",
            &[Value::Text(key.to_string())],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
    if let Some(row) = rows.first() {
        let id: i64 = row.get_as(0).unwrap_or(0);
        let slug: String = row.get_as(1).unwrap_or_default();
        if id > 0 && !slug.is_empty() {
            return Ok((id, slug));
        }
    }
    Err(CliError::InvalidArgument(format!(
        "project not found: {key}"
    )))
}

fn normalize_project_lookup_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn find_project_for_path_or_ancestors(
    conn: &DbConn,
    path: &Path,
) -> Result<(i64, String), CliError> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    for candidate in canonical.ancestors() {
        let candidate_key = normalize_project_lookup_path(candidate);
        if let Ok(project) = resolve_project_sync(conn, &candidate_key) {
            return Ok(project);
        }
    }

    Err(CliError::InvalidArgument(format!(
        "project not found for current directory or its ancestors: {}",
        normalize_project_lookup_path(path)
    )))
}

fn project_human_key_sync(conn: &DbConn, project_id: i64) -> Result<Option<String>, CliError> {
    let rows = conn
        .query_sync(
            "SELECT human_key FROM projects WHERE id = ?",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| CliError::Other(format!("query failed: {e}")))?;
    Ok(rows.first().and_then(|row| {
        let human_key: String = row.get_as(0).unwrap_or_default();
        (!human_key.is_empty()).then_some(human_key)
    }))
}

/// Find the project for the current working directory.
fn find_project_for_cwd(conn: &DbConn) -> Result<(i64, String), CliError> {
    let cwd =
        std::env::current_dir().map_err(|e| CliError::Other(format!("cannot get CWD: {e}")))?;
    find_project_for_path_or_ancestors(conn, &cwd)
}

fn resolved_project_flag_or_env(flag: Option<&str>) -> Option<String> {
    flag.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::borrow::ToOwned::to_owned)
        .or_else(|| {
            std::env::var("AGENT_MAIL_PROJECT")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

/// Resolve project from --project flag or CWD.
fn resolve_project(conn: &DbConn, flag: Option<&str>) -> Result<(i64, String), CliError> {
    if let Some(key) = resolved_project_flag_or_env(flag) {
        resolve_project_sync(conn, &key)
    } else {
        find_project_for_cwd(conn)
    }
}

fn split_resource_path_and_query(
    uri: &str,
) -> Result<(String, std::collections::HashMap<String, String>), CliError> {
    let path = uri.strip_prefix("resource://").ok_or_else(|| {
        CliError::InvalidArgument(format!("invalid URI scheme: {uri} (expected resource://)"))
    })?;
    if let Some((base, query)) = path.split_once('?') {
        Ok((base.to_string(), parse_resource_query(query)))
    } else {
        Ok((path.to_string(), std::collections::HashMap::new()))
    }
}

fn parse_resource_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(
            percent_decode_resource_query_component(key),
            percent_decode_resource_query_component(value),
        );
    }
    params
}

fn percent_decode_resource_path_component(input: &str) -> String {
    percent_decode_resource_component(input, false)
}

fn percent_decode_resource_query_component(input: &str) -> String {
    percent_decode_resource_component(input, true)
}

fn percent_decode_resource_component(input: &str, plus_as_space: bool) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
                    && let Ok(value) = u8::from_str_radix(hex, 16)
                {
                    out.push(value);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_resource_bool(value: Option<&String>) -> bool {
    value.is_some_and(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "t" | "yes" | "y" | "on"
        )
    })
}

const RESOURCE_URI_LIMIT_MAX: usize = 10_000;

fn parse_resource_limit(value: Option<&String>, default_limit: usize) -> usize {
    value
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|limit| *limit > 0)
        .and_then(|limit| usize::try_from(limit).ok())
        .map(|limit| limit.min(RESOURCE_URI_LIMIT_MAX))
        .unwrap_or(default_limit)
}

fn resolved_agent_flag_or_env(flag: Option<&str>) -> Option<String> {
    flag.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::borrow::ToOwned::to_owned)
        .or_else(|| {
            std::env::var("AGENT_MAIL_AGENT")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            std::env::var("AGENT_NAME")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

/// Resolve agent ID from --agent flag or AGENT_MAIL_AGENT/AGENT_NAME env vars.
fn resolve_agent_id(
    conn: &DbConn,
    project_id: i64,
    flag: Option<&str>,
) -> Result<Option<(i64, String)>, CliError> {
    let Some(name) = resolved_agent_flag_or_env(flag) else {
        return Ok(None);
    };
    let resolved = crate::context::resolve_agent(conn, project_id, &name)?;
    Ok(Some((resolved.id, resolved.name)))
}

fn soften_implicit_missing_agent_error(
    explicit_flag: Option<&str>,
    result: Result<Option<(i64, String)>, CliError>,
) -> Result<Option<(i64, String)>, CliError> {
    match result {
        Err(CliError::InvalidArgument(message))
            if explicit_flag.is_none() && message.starts_with("agent not found: ") =>
        {
            Ok(None)
        }
        other => other,
    }
}

fn resolve_optional_agent_id(
    conn: &DbConn,
    project_id: i64,
    explicit_flag: Option<&str>,
) -> Result<Option<(i64, String)>, CliError> {
    soften_implicit_missing_agent_error(
        explicit_flag,
        resolve_agent_id(conn, project_id, explicit_flag),
    )
}

struct RobotDbHandle {
    conn: DbConn,
    _snapshot_dir: Option<tempfile::TempDir>,
}

impl RobotDbHandle {
    fn open_local() -> Result<Self, CliError> {
        Ok(Self {
            conn: crate::open_db_sync_robot()?,
            _snapshot_dir: None,
        })
    }

    fn open_archive_snapshot(storage_root: &Path) -> Result<Self, CliError> {
        let snapshot_dir = tempfile::tempdir()
            .map_err(|e| CliError::Other(format!("robot archive snapshot tempdir failed: {e}")))?;
        let db_path = snapshot_dir.path().join("robot-archive-snapshot.sqlite3");
        mcp_agent_mail_db::reconstruct_from_archive(&db_path, storage_root).map_err(|e| {
            CliError::Other(format!("robot archive snapshot reconstruction failed: {e}"))
        })?;
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .map_err(|e| CliError::Other(format!("robot archive snapshot open failed: {e}")))?;
        Ok(Self {
            conn,
            _snapshot_dir: Some(snapshot_dir),
        })
    }

    #[cfg(test)]
    fn from_conn(conn: DbConn) -> Self {
        Self {
            conn,
            _snapshot_dir: None,
        }
    }
}

struct ResolvedRobotScope {
    db: RobotDbHandle,
    project_id: i64,
    project_slug: String,
    agent: Option<(i64, String)>,
}

impl ResolvedRobotScope {
    fn conn(&self) -> &DbConn {
        &self.db.conn
    }
}

fn archive_project_dir_for_key(storage_root: &Path, project_key: &str) -> Option<PathBuf> {
    let project_key = project_key.trim();
    if project_key.is_empty() {
        return None;
    }
    let projects_dir = storage_root.join("projects");
    if !projects_dir.is_dir() {
        return None;
    }

    let project_path = Path::new(project_key);
    let normalized_project_key = project_key.replace('\\', "/");
    let mut candidate_slugs = Vec::new();
    if project_path.is_absolute() {
        if let Some(file_name) = project_path.file_name().and_then(|value| value.to_str()) {
            let file_name = file_name.trim();
            if !file_name.is_empty() {
                candidate_slugs.push(file_name.to_string());
            }
        }
        let derived = mcp_agent_mail_core::resolve_project_identity(project_key).slug;
        if !candidate_slugs.iter().any(|existing| existing == &derived) {
            candidate_slugs.push(derived);
        }
    } else {
        candidate_slugs.push(project_key.to_string());
    }

    if let Some(project_dir) = candidate_slugs
        .into_iter()
        .map(|slug| projects_dir.join(slug))
        .find(|path| path.is_dir())
    {
        return Some(project_dir);
    }

    std::fs::read_dir(&projects_dir)
        .ok()?
        .flatten()
        .find_map(|entry| {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                return None;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            let metadata = std::fs::read_to_string(path.join("project.json")).ok()?;
            let project_meta = serde_json::from_str::<serde_json::Value>(&metadata).ok()?;
            let human_key = project_meta
                .get("human_key")
                .and_then(serde_json::Value::as_str)
                .map(|value| value.replace('\\', "/"));
            let slug = project_meta
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            if human_key.as_deref() == Some(normalized_project_key.as_str())
                || slug.as_deref() == Some(project_key)
            {
                Some(path)
            } else {
                None
            }
        })
}

fn archive_has_agent_profile(project_dir: &Path, agent_name: &str) -> bool {
    let agent_name = agent_name.trim();
    if agent_name.is_empty() {
        return false;
    }

    let agents_dir = project_dir.join("agents");
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            return false;
        };
        file_type.is_dir()
            && !file_type.is_symlink()
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(agent_name)
            && path.join("profile.json").is_file()
    })
}

fn should_try_archive_snapshot(
    error: &CliError,
    project_key: Option<&str>,
    agent_name: Option<&str>,
    storage_root: &Path,
) -> bool {
    let Some(project_key) = project_key else {
        return false;
    };
    let Some(project_dir) = archive_project_dir_for_key(storage_root, project_key) else {
        return false;
    };

    match error {
        CliError::InvalidArgument(message) if message.starts_with("project not found: ") => true,
        CliError::InvalidArgument(message) if message.starts_with("agent not found: ") => {
            agent_name.is_some_and(|name| archive_has_agent_profile(&project_dir, name))
        }
        _ => false,
    }
}

fn resolve_robot_scope_with_handle(
    db: RobotDbHandle,
    project_flag: Option<&str>,
    agent_flag: Option<&str>,
) -> Result<ResolvedRobotScope, CliError> {
    let (project_id, project_slug) = resolve_project(&db.conn, project_flag)?;
    let agent = resolve_optional_agent_id(&db.conn, project_id, agent_flag)?;
    Ok(ResolvedRobotScope {
        db,
        project_id,
        project_slug,
        agent,
    })
}

fn resolve_robot_scope_with_archive_fallback(
    local: RobotDbHandle,
    storage_root: &Path,
    project_flag: Option<&str>,
    agent_flag: Option<&str>,
) -> Result<ResolvedRobotScope, CliError> {
    match resolve_robot_scope_with_handle(local, project_flag, agent_flag) {
        Ok(scope) => Ok(scope),
        Err(local_error) => {
            let project_key = resolved_project_flag_or_env(project_flag);
            let agent_name = resolved_agent_flag_or_env(agent_flag);
            if !should_try_archive_snapshot(
                &local_error,
                project_key.as_deref(),
                agent_name.as_deref(),
                storage_root,
            ) {
                return Err(local_error);
            }

            tracing::warn!(
                project = project_key.as_deref().unwrap_or(""),
                agent = agent_name.as_deref().unwrap_or(""),
                error = %local_error,
                "robot command falling back to archive snapshot because local db is missing requested scope"
            );

            let snapshot = RobotDbHandle::open_archive_snapshot(storage_root)?;
            resolve_robot_scope_with_handle(snapshot, project_flag, agent_flag)
        }
    }
}

fn resolve_robot_scope(
    project_flag: Option<&str>,
    agent_flag: Option<&str>,
) -> Result<ResolvedRobotScope, CliError> {
    let local = RobotDbHandle::open_local()?;
    let storage_root = mcp_agent_mail_core::Config::from_env().storage_root;
    resolve_robot_scope_with_archive_fallback(local, &storage_root, project_flag, agent_flag)
}

/// Format seconds into human-readable relative time.
fn format_age(seconds: i64) -> String {
    if seconds < 0 {
        return "just now".to_string();
    }
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    if seconds < 3600 {
        return format!("{}m ago", seconds / 60);
    }
    if seconds < 86400 {
        return format!("{}h ago", seconds / 3600);
    }
    format!("{}d ago", seconds / 86400)
}

fn parse_since_micros(s: &str) -> Result<i64, CliError> {
    mcp_agent_mail_db::iso_to_micros(s)
        .ok_or_else(|| CliError::InvalidArgument(format!("invalid --since timestamp: {s}")))
}

const ACK_OVERDUE_THRESHOLD_US: i64 = 30 * 60 * 1_000_000;
const ACK_SLA_VIOLATION_THRESHOLD_US: i64 = 60 * 60 * 1_000_000;

// ── Status command implementation ───────────────────────────────────────────

fn build_status(
    conn: &DbConn,
    project_id: i64,
    project_slug: &str,
    agent: Option<(i64, String)>,
) -> Result<(StatusData, Vec<String>), CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    let _now_s = now_us / 1_000_000;
    let has_release_ledger = has_file_reservation_release_ledger(conn);
    let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
    let active_reservation_join =
        active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );

    // 1. Inbox counts (agent-specific, if resolved)
    let (unread, urgent, ack_required, ack_overdue) = if let Some((agent_id, _)) = &agent {
        let inbox_sql = "
            SELECT
                SUM(CASE WHEN mr.read_ts IS NULL THEN 1 ELSE 0 END) AS unread,
                SUM(CASE WHEN mr.read_ts IS NULL AND m.importance IN ('urgent','high') THEN 1 ELSE 0 END) AS urgent,
                SUM(CASE WHEN m.ack_required = 1 AND mr.ack_ts IS NULL THEN 1 ELSE 0 END) AS ack_required,
                SUM(CASE WHEN m.ack_required = 1 AND mr.ack_ts IS NULL
                    AND m.created_ts < ? THEN 1 ELSE 0 END) AS ack_overdue
            FROM message_recipients mr
            JOIN messages m ON m.id = mr.message_id
            WHERE mr.agent_id = ? AND m.project_id = ?
        ";
        let threshold = now_us - ACK_OVERDUE_THRESHOLD_US;
        let rows = conn
            .query_sync(
                inbox_sql,
                &[
                    Value::BigInt(threshold),
                    Value::BigInt(*agent_id),
                    Value::BigInt(project_id),
                ],
            )
            .map_err(|e| CliError::Other(format!("inbox query failed: {e}")))?;
        if let Some(row) = rows.first() {
            (
                row.get_named::<i64>("unread").unwrap_or(0) as usize,
                row.get_named::<i64>("urgent").unwrap_or(0) as usize,
                row.get_named::<i64>("ack_required").unwrap_or(0) as usize,
                row.get_named::<i64>("ack_overdue").unwrap_or(0) as usize,
            )
        } else {
            (0, 0, 0, 0)
        }
    } else {
        (0, 0, 0, 0)
    };

    // 2. Active agents (active in last 15 minutes)
    let active_threshold = now_us - 15 * 60 * 1_000_000;
    let active_agents = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM agents WHERE project_id = ? AND last_active_ts > ?",
            &[Value::BigInt(project_id), Value::BigInt(active_threshold)],
        )
        .map_err(|e| CliError::Other(format!("active agents query failed: {e}")))?
        .first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .unwrap_or(0) as usize;

    // 3. Recent messages (last hour)
    let hour_ago = now_us - 3600 * 1_000_000;
    let recent_messages = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM messages WHERE project_id = ? AND created_ts > ?",
            &[Value::BigInt(project_id), Value::BigInt(hour_ago)],
        )
        .map_err(|e| CliError::Other(format!("recent messages query failed: {e}")))?
        .first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .unwrap_or(0) as usize;

    // 4. File reservations (active = not released and not expired)
    let active_reservations = conn
        .query_sync(
            &format!(
                "SELECT COUNT(*) AS cnt
                 FROM file_reservations fr{active_reservation_join}
                 WHERE fr.project_id = ? AND ({active_reservation_predicate}) AND fr.expires_ts > ?"
            ),
            &[Value::BigInt(project_id), Value::BigInt(now_us)],
        )
        .map_err(|e| CliError::Other(format!("active reservations query failed: {e}")))?
        .first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .unwrap_or(0) as usize;

    // Reservations expiring soon (within 5 minutes)
    let expiring_threshold = now_us + 5 * 60 * 1_000_000;
    let reservations_expiring_soon = conn
        .query_sync(
            &format!(
                "SELECT COUNT(*) AS cnt
                 FROM file_reservations fr{active_reservation_join}
                 WHERE fr.project_id = ? AND ({active_reservation_predicate})
                 AND fr.expires_ts > ? AND fr.expires_ts < ?"
            ),
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us),
                Value::BigInt(expiring_threshold),
            ],
        )
        .map_err(|e| CliError::Other(format!("expiring reservations query failed: {e}")))?
        .first()
        .and_then(|r| r.get_named::<i64>("cnt").ok())
        .unwrap_or(0) as usize;

    // 5. My reservations (agent-specific)
    let my_reservations = if let Some((agent_id, _)) = &agent {
        conn.query_sync(
            &format!(
                "SELECT fr.path_pattern, fr.\"exclusive\", fr.expires_ts
                 FROM file_reservations fr{active_reservation_join}
                 WHERE fr.project_id = ? AND fr.agent_id = ? AND ({active_reservation_predicate})
                   AND fr.expires_ts > ?
                 ORDER BY fr.expires_ts ASC"
            ),
            &[
                Value::BigInt(project_id),
                Value::BigInt(*agent_id),
                Value::BigInt(now_us),
            ],
        )
        .map_err(|e| CliError::Other(format!("my reservations query failed: {e}")))?
        .iter()
        .map(|r| {
            let expires: i64 = r.get_named("expires_ts").unwrap_or(0);
            ReservationEntry {
                agent: None,
                path: r.get_named("path_pattern").unwrap_or_default(),
                exclusive: r.get_named::<i64>("exclusive").unwrap_or(1) != 0,
                remaining_seconds: (expires - now_us) / 1_000_000,
                remaining: None,
                granted_at: None,
            }
        })
        .collect()
    } else {
        vec![]
    };

    // 6. Top threads (3 most recently active)
    let top_threads_rows = conn
        .query_sync(
            "SELECT CASE
                        WHEN m.thread_id IS NOT NULL AND m.thread_id <> '' THEN m.thread_id
                        ELSE CAST(m.id AS TEXT)
                    END AS thread_id,
                    COUNT(*) AS msg_count,
                    MAX(created_ts) AS last_ts
             FROM messages m
             WHERE project_id = ?
             GROUP BY CASE
                        WHEN m.thread_id IS NOT NULL AND m.thread_id <> '' THEN m.thread_id
                        ELSE CAST(m.id AS TEXT)
                      END
             ORDER BY last_ts DESC
             LIMIT 3",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| CliError::Other(format!("top threads query failed: {e}")))?;

    let top_threads: Vec<ThreadSummary> = top_threads_rows
        .iter()
        .map(|r| -> Result<ThreadSummary, CliError> {
            let thread_id: String = r.get_named("thread_id").unwrap_or_default();
            let msg_count: i64 = r.get_named("msg_count").unwrap_or(0);
            let last_ts: i64 = r.get_named("last_ts").unwrap_or(0);
            let subject = load_thread_subject(conn, project_id, &thread_id, true)?
                .ok_or_else(|| CliError::Other(format!("top thread not found: {thread_id}")))?;
            let participants = load_thread_participants(conn, project_id, &thread_id)?.len();
            let age_seconds = now_us.saturating_sub(last_ts) / 1_000_000;
            Ok(ThreadSummary {
                id: thread_id,
                subject,
                participants,
                messages: msg_count as usize,
                last_activity: format_age(age_seconds),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // 7. Build anomalies
    let mut anomalies = Vec::new();
    if ack_overdue > 0 {
        anomalies.push(AnomalyCard {
            severity: "warn".to_string(),
            confidence: 1.0,
            category: "ack_sla".to_string(),
            headline: format!("{ack_overdue} message(s) pending ack > 30 minutes"),
            rationale: "Messages with ack_required=true have been waiting beyond the 30-minute SLA threshold".to_string(),
            remediation: "am robot inbox --ack-overdue".to_string(),
        });
    }
    if reservations_expiring_soon > 0 {
        anomalies.push(AnomalyCard {
            severity: "warn".to_string(),
            confidence: 1.0,
            category: "reservation_expiry".to_string(),
            headline: format!(
                "{reservations_expiring_soon} reservation(s) expiring within 5 minutes"
            ),
            rationale: "File reservations are about to expire which may cause edit conflicts"
                .to_string(),
            remediation: "am robot reservations --expiring=5".to_string(),
        });
    }

    // 8. Build suggested actions
    let mut actions = Vec::new();
    if urgent > 0 {
        actions.push("am robot inbox --urgent".to_string());
    }
    if ack_overdue > 0
        && let Some((_, ref name)) = agent
    {
        actions.push(format!(
            "am robot inbox --project {project_slug} --agent {name} --ack-overdue"
        ));
    }
    if let Some(top) = top_threads.first() {
        actions.push(format!(
            "am robot thread --project {project_slug} {}",
            top.id
        ));
    }

    let health = if anomalies.iter().any(|a| a.severity == "error") {
        "error"
    } else if anomalies.is_empty() {
        "ok"
    } else {
        "degraded"
    }
    .to_string();

    let data = StatusData {
        health,
        unread,
        urgent,
        ack_required,
        ack_overdue,
        active_reservations,
        reservations_expiring_soon,
        active_agents,
        recent_messages,
        my_reservations,
        top_threads,
        anomalies,
    };

    Ok((data, actions))
}

// ── Inbox command implementation ────────────────────────────────────────────

/// Inbox result with entries and generated alerts/actions.
struct InboxResult {
    entries: Vec<InboxEntry>,
    alerts: Vec<(String, String, Option<String>)>,
    actions: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn build_inbox(
    conn: &DbConn,
    project_id: i64,
    project_slug: &str,
    agent_id: i64,
    agent_name: &str,
    urgent_only: bool,
    ack_overdue_only: bool,
    unread_only: bool,
    show_all: bool,
    limit: usize,
    include_bodies: bool,
) -> Result<InboxResult, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    // ack_overdue threshold: 30 minutes
    let ack_threshold = now_us - ACK_OVERDUE_THRESHOLD_US;

    // Build WHERE filter based on flags
    let bucket_filter = if urgent_only {
        "AND priority_bucket = 1"
    } else if ack_overdue_only {
        "AND priority_bucket = 2"
    } else if show_all {
        "" // no filter
    } else if unread_only {
        "AND priority_bucket <= 4" // unread only (read_ts IS NULL)
    } else {
        "AND priority_bucket <= 5" // include read but un-acked messages
    };

    let sql = format!(
        "SELECT sub.id, sub.subject, sub.thread_id, sub.importance, sub.ack_required,
                sub.created_ts, sub.sender_id, sub.read_ts, sub.ack_ts, sub.body_md,
                sub.priority_bucket, a_sender.name AS sender_name
         FROM (
             SELECT m.id, m.subject, m.thread_id, m.importance, m.ack_required,
                    m.created_ts, m.sender_id, mr.read_ts, mr.ack_ts, m.body_md,
                    CASE
                        WHEN m.importance IN ('urgent','high') AND mr.read_ts IS NULL THEN 1
                        WHEN m.ack_required = 1 AND mr.ack_ts IS NULL AND m.created_ts < ? THEN 2
                        WHEN m.ack_required = 1 AND mr.ack_ts IS NULL AND mr.read_ts IS NULL THEN 3
                        WHEN mr.read_ts IS NULL THEN 4
                        WHEN m.ack_required = 1 AND mr.ack_ts IS NULL THEN 5
                        ELSE 6
                    END AS priority_bucket
             FROM message_recipients mr
             JOIN messages m ON m.id = mr.message_id
             WHERE mr.agent_id = ? AND m.project_id = ?
         ) sub
         JOIN agents a_sender ON a_sender.id = sub.sender_id
         WHERE 1=1 {bucket_filter}
         ORDER BY sub.priority_bucket ASC, sub.created_ts DESC
         LIMIT ?"
    );

    let rows = conn
        .query_sync(
            &sql,
            &[
                Value::BigInt(ack_threshold),
                Value::BigInt(agent_id),
                Value::BigInt(project_id),
                Value::BigInt(limit.try_into().unwrap_or(i64::MAX)),
            ],
        )
        .map_err(|e| CliError::Other(format!("inbox query failed: {e}")))?;

    let mut entries = Vec::new();
    let mut overdue_ids = Vec::new();

    for row in &rows {
        let id: i64 = row.get_named("id").unwrap_or(0);
        let bucket: i64 = row.get_named("priority_bucket").unwrap_or(7);
        let sender: String = row.get_named("sender_name").unwrap_or_default();
        let subject: String = row.get_named("subject").unwrap_or_default();
        let thread_id: String = row.get_named("thread_id").unwrap_or_default();
        let thread_ref = canonical_thread_ref(id, &thread_id);
        let importance: String = row.get_named("importance").unwrap_or_default();
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
        let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
        let ack_ts: Option<i64> = row.get_named("ack_ts").ok();
        let read_ts: Option<i64> = row.get_named("read_ts").ok();

        let priority_label = match bucket {
            1 => "urgent",
            2 => "ack-overdue",
            3 => "ack-required",
            4 => "unread",
            5 => "read-unacked",
            _ => "read",
        };

        let ack_status = if ack_required == 0 {
            "none".to_string()
        } else if ack_ts.is_some() {
            "acked".to_string()
        } else if bucket == 2 {
            "overdue".to_string()
        } else if read_ts.is_some() {
            "pending".to_string()
        } else {
            "required".to_string()
        };

        let age_seconds = now_us.saturating_sub(created_ts) / 1_000_000;

        if bucket == 2 {
            overdue_ids.push(id);
        }

        let body_md = if include_bodies {
            row.get_named::<String>("body_md").ok()
        } else {
            None
        };

        entries.push(InboxEntry {
            id,
            priority: priority_label.to_string(),
            from: sender,
            subject,
            thread: thread_ref,
            age: format_age(age_seconds),
            ack_status,
            importance,
            body_md,
        });
    }

    // Build alerts
    let mut alerts = Vec::new();
    if !overdue_ids.is_empty() {
        let ids_str: Vec<String> = overdue_ids.iter().map(|id| format!("#{id}")).collect();
        alerts.push((
            "warn".to_string(),
            format!(
                "{} message(s) ack overdue (>30m): {}",
                overdue_ids.len(),
                ids_str.join(", ")
            ),
            Some(format!(
                "am mail ack --project {project_slug} --agent {agent_name} {}",
                overdue_ids[0]
            )),
        ));
    }

    // Build actions
    let mut actions = Vec::new();
    for &id in overdue_ids.iter().take(3) {
        actions.push(format!(
            "am mail ack --project {project_slug} --agent {agent_name} {id}"
        ));
    }
    if let Some(entry) = entries.first()
        && !entry.thread.is_empty()
    {
        actions.push(format!(
            "am robot thread --project {project_slug} {}",
            entry.thread
        ));
    }

    Ok(InboxResult {
        entries,
        alerts,
        actions,
    })
}

fn load_recipient_display_names(
    conn: &DbConn,
    message_id: i64,
    context: &'static str,
) -> Result<Vec<String>, CliError> {
    let primary_rows = conn
        .query_sync(
            "SELECT a.name AS name
             FROM message_recipients mr
             JOIN agents a ON a.id = mr.agent_id
             WHERE mr.message_id = ? AND mr.kind = 'to'
             ORDER BY a.name COLLATE NOCASE ASC, a.name ASC",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| CliError::Other(format!("{context} recipient to-query failed: {e}")))?;
    let primary_names: Vec<String> = primary_rows
        .iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .filter(|name| !name.is_empty())
        .collect();
    if !primary_names.is_empty() {
        return Ok(primary_names);
    }

    let fallback_rows = conn
        .query_sync(
            "SELECT mr.kind AS kind, a.name AS name
             FROM message_recipients mr
             JOIN agents a ON a.id = mr.agent_id
             WHERE mr.message_id = ?
             ORDER BY CASE mr.kind
                        WHEN 'cc' THEN 0
                        WHEN 'bcc' THEN 1
                        ELSE 2
                      END ASC,
                      a.name COLLATE NOCASE ASC,
                      a.name ASC",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| CliError::Other(format!("{context} recipient fallback query failed: {e}")))?;
    Ok(fallback_rows
        .iter()
        .filter_map(|row| {
            let name = row.get_named::<String>("name").ok()?;
            if name.is_empty() {
                return None;
            }
            let kind = row.get_named::<String>("kind").ok().unwrap_or_default();
            Some(match kind.as_str() {
                "cc" | "bcc" => format!("{kind}: {name}"),
                _ => name,
            })
        })
        .collect())
}

fn build_outbox_entries(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    limit: usize,
    include_bodies: bool,
) -> Result<Vec<InboxEntry>, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    let rows = conn
        .query_sync(
            "SELECT m.id, m.subject, m.thread_id, m.importance, m.ack_required, m.created_ts, m.body_md,
                    COUNT(mr.agent_id) AS recipient_count,
                    SUM(CASE WHEN mr.ack_ts IS NOT NULL THEN 1 ELSE 0 END) AS acked_count
             FROM messages m
             LEFT JOIN message_recipients mr ON mr.message_id = m.id
             WHERE m.sender_id = ? AND m.project_id = ?
             GROUP BY m.id
             ORDER BY m.created_ts DESC
             LIMIT ?",
            &[
                Value::BigInt(agent_id),
                Value::BigInt(project_id),
                Value::BigInt(limit.try_into().unwrap_or(i64::MAX)),
            ],
        )
        .map_err(|e| CliError::Other(format!("outbox query failed: {e}")))?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.get_named("id").unwrap_or(0);
        let subject: String = row.get_named("subject").unwrap_or_default();
        let thread_id: String = row.get_named("thread_id").unwrap_or_default();
        let thread_ref = canonical_thread_ref(id, &thread_id);
        let importance: String = row.get_named("importance").unwrap_or_default();
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
        let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
        let acked_count = row.get_named::<i64>("acked_count").unwrap_or(0);
        let recipient_count = row.get_named::<i64>("recipient_count").unwrap_or(0);

        let recipient_names = load_recipient_display_names(conn, id, "outbox")?.join(", ");
        let recipient_names = if recipient_names.is_empty() {
            "(no recipients)".to_string()
        } else {
            recipient_names
        };

        let ack_status = if ack_required == 0 {
            "none".to_string()
        } else if recipient_count > 0 && acked_count >= recipient_count {
            "done".to_string()
        } else if recipient_count > 0 && acked_count > 0 {
            format!("partial ({acked_count}/{recipient_count})")
        } else if acked_count > 0 {
            format!("acked ({acked_count})")
        } else {
            "pending".to_string()
        };

        let body_md = if include_bodies {
            row.get_named::<String>("body_md").ok()
        } else {
            None
        };

        let age_seconds = now_us.saturating_sub(created_ts) / 1_000_000;
        entries.push(InboxEntry {
            id,
            priority: "sent".to_string(),
            from: recipient_names,
            subject,
            thread: thread_ref,
            age: format_age(age_seconds),
            ack_status,
            importance,
            body_md,
        });
    }

    Ok(entries)
}

// ── Thread command implementation ───────────────────────────────────────────

/// Thread rendering response data.
#[derive(Debug, Serialize)]
struct ThreadData {
    thread_id: String,
    subject: String,
    message_count: usize,
    participants: Vec<String>,
    last_activity: String,
    messages: Vec<ThreadMessage>,
}

impl MarkdownRenderable for ThreadData {
    fn to_markdown(
        &self,
        _meta: &RobotMeta,
        _alerts: &[RobotAlert],
        _actions: &[String],
    ) -> String {
        let mut md = format!(
            "# Thread: {} — {}\n**Messages**: {} | **Participants**: {} | **Last activity**: {}\n\n---\n\n",
            self.thread_id,
            self.subject,
            self.message_count,
            self.participants.join(", "),
            self.last_activity,
        );
        for msg in &self.messages {
            md.push_str(&format!(
                "### [{pos}] {from} → {to} | {age} | importance: {imp} | ack: {ack}\n**Subject**: {subj}\n\n{body}\n\n---\n\n",
                pos = msg.position,
                from = msg.from,
                to = msg.to,
                age = msg.age,
                imp = msg.importance,
                ack = msg.ack,
                subj = msg.subject,
                body = msg.body.as_deref().unwrap_or("*(no body)*"),
            ));
        }
        md
    }
}

fn build_thread(
    conn: &DbConn,
    project_id: i64,
    thread_id: &str,
    limit: Option<usize>,
    since: Option<&str>,
    include_bodies: bool,
) -> Result<ThreadData, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    let thread_subject = load_thread_subject(conn, project_id, thread_id, false)?
        .ok_or_else(|| CliError::InvalidArgument(format!("thread not found: {thread_id}")))?;
    let participants = load_thread_participants(conn, project_id, thread_id)?;

    let mut conditions = vec!["m.project_id = ?".to_string()];
    let mut params: Vec<Value> = vec![Value::BigInt(project_id)];
    append_thread_membership_condition("m", thread_id, &mut conditions, &mut params);

    if let Some(since_str) = since {
        let since_us = parse_since_micros(since_str)?;
        conditions.push("m.created_ts > ?".to_string());
        params.push(Value::BigInt(since_us));
    }

    let where_clause = conditions.join(" AND ");
    let effective_limit = limit.unwrap_or(200);
    params.push(Value::BigInt(
        effective_limit.try_into().unwrap_or(i64::MAX),
    ));
    let sql = format!(
        "SELECT m.id, m.subject, m.body_md, m.importance, m.ack_required, m.created_ts,
                m.sender_id, a_sender.name AS sender_name,
                COUNT(mr.agent_id) AS recipient_count,
                SUM(CASE WHEN mr.ack_ts IS NOT NULL THEN 1 ELSE 0 END) AS acked_count
         FROM messages m
         JOIN agents a_sender ON a_sender.id = m.sender_id
         LEFT JOIN message_recipients mr ON mr.message_id = m.id
         WHERE {where_clause}
         GROUP BY m.id
         ORDER BY m.created_ts DESC, m.id DESC
         LIMIT ?"
    );

    let mut rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| CliError::Other(format!("thread query failed: {e}")))?;
    rows.reverse();

    let mut messages = Vec::new();
    let mut last_ts: i64 = 0;

    for (idx, row) in rows.iter().enumerate() {
        let msg_id: i64 = row.get_named("id").unwrap_or(0);
        let subject: String = row.get_named("subject").unwrap_or_default();
        let body: String = row.get_named("body_md").unwrap_or_default();
        let importance: String = row.get_named("importance").unwrap_or_default();
        let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
        let sender: String = row.get_named("sender_name").unwrap_or_default();
        let recipient_count = row.get_named::<i64>("recipient_count").unwrap_or(0);
        let acked_count = row.get_named::<i64>("acked_count").unwrap_or(0);

        if created_ts > last_ts {
            last_ts = created_ts;
        }

        // Get recipients for this message
        let to_names = load_recipient_display_names(conn, msg_id, "thread")?;

        // Check ack status
        let ack_status = if ack_required == 0 {
            "none".to_string()
        } else if recipient_count > 0 && acked_count >= recipient_count {
            "done".to_string()
        } else if recipient_count > 0 && acked_count > 0 {
            format!("partial ({acked_count}/{recipient_count})")
        } else if acked_count > 0 {
            format!("acked ({acked_count})")
        } else {
            "required".to_string()
        };

        let age_seconds = now_us.saturating_sub(created_ts) / 1_000_000;

        messages.push(ThreadMessage {
            position: idx + 1,
            from: sender,
            to: to_names.join(", "),
            age: format_age(age_seconds),
            importance,
            ack: ack_status,
            subject,
            body: if include_bodies { Some(body) } else { None },
        });
    }

    let last_activity = if last_ts > 0 {
        format_age(now_us.saturating_sub(last_ts) / 1_000_000)
    } else {
        "unknown".to_string()
    };

    Ok(ThreadData {
        thread_id: thread_id.to_string(),
        subject: thread_subject,
        message_count: messages.len(),
        participants,
        last_activity,
        messages,
    })
}

// ── Message command implementation ──────────────────────────────────────────

fn append_thread_membership_condition(
    alias: &str,
    thread_ref: &str,
    conditions: &mut Vec<String>,
    params: &mut Vec<Value>,
) {
    if let Ok(root_id) = thread_ref.parse::<i64>() {
        conditions.push(format!("({alias}.id = ? OR {alias}.thread_id = ?)"));
        params.push(Value::BigInt(root_id));
    } else {
        conditions.push(format!("{alias}.thread_id = ?"));
    }
    params.push(Value::Text(thread_ref.to_string()));
}

fn thread_scope_params(alias: &str, project_id: i64, thread_ref: &str) -> (String, Vec<Value>) {
    let mut conditions = vec![format!("{alias}.project_id = ?")];
    let mut params = vec![Value::BigInt(project_id)];
    append_thread_membership_condition(alias, thread_ref, &mut conditions, &mut params);
    (conditions.join(" AND "), params)
}

fn load_thread_subject(
    conn: &DbConn,
    project_id: i64,
    thread_ref: &str,
    newest_first: bool,
) -> Result<Option<String>, CliError> {
    let (thread_where, thread_params) = thread_scope_params("m", project_id, thread_ref);
    let order_clause = if newest_first {
        "ORDER BY m.created_ts DESC, m.id DESC"
    } else {
        "ORDER BY m.created_ts ASC, m.id ASC"
    };
    let rows = conn
        .query_sync(
            &format!(
                "SELECT m.subject, m.created_ts, m.id
                 FROM messages m
                 WHERE {thread_where}
                 {order_clause}
                 LIMIT 1"
            ),
            &thread_params,
        )
        .map_err(|e| CliError::Other(format!("thread subject query failed: {e}")))?;
    match rows.first() {
        Some(row) => row
            .get_named::<String>("subject")
            .map(Some)
            .map_err(|e| CliError::Other(format!("thread subject decode failed: {e}"))),
        None => Ok(None),
    }
}

fn load_thread_participants(
    conn: &DbConn,
    project_id: i64,
    thread_ref: &str,
) -> Result<Vec<String>, CliError> {
    let (sender_where, mut sender_params) = thread_scope_params("m1", project_id, thread_ref);
    let (recipient_where, mut recipient_params) = thread_scope_params("m2", project_id, thread_ref);
    let sql = format!(
        "SELECT name, MIN(first_ts), MIN(first_id), lower(name)
         FROM (
             SELECT a_sender.name AS name, MIN(m1.created_ts) AS first_ts, MIN(m1.id) AS first_id
             FROM messages m1
             JOIN agents a_sender ON a_sender.id = m1.sender_id
             WHERE {sender_where}
             GROUP BY a_sender.name
             UNION ALL
             SELECT a_rec.name AS name, MIN(m2.created_ts) AS first_ts, MIN(m2.id) AS first_id
             FROM message_recipients mr
             JOIN messages m2 ON m2.id = mr.message_id
             JOIN agents a_rec ON a_rec.id = mr.agent_id
             WHERE {recipient_where}
             GROUP BY a_rec.name
         ) participants
         GROUP BY name
         ORDER BY MIN(first_ts) ASC, MIN(first_id) ASC, lower(name) ASC, name ASC"
    );
    let mut params = Vec::with_capacity(sender_params.len() + recipient_params.len());
    params.append(&mut sender_params);
    params.append(&mut recipient_params);
    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| CliError::Other(format!("thread participants query failed: {e}")))?;
    rows.iter()
        .map(|row| {
            row.get_named::<String>("name")
                .map_err(|e| CliError::Other(format!("thread participant decode failed: {e}")))
        })
        .collect()
}

fn canonical_thread_ref(message_id: i64, thread_id: &str) -> String {
    if thread_id.is_empty() {
        message_id.to_string()
    } else {
        thread_id.to_string()
    }
}

fn format_adjacent_thread_row(row: &sqlmodel_core::Row) -> String {
    let id: i64 = row.get_named("id").unwrap_or(0);
    let sender: String = row.get_named("sender").unwrap_or_default();
    let subject: String = row.get_named("subject").unwrap_or_default();
    format!("#{id} {sender}: {}", truncate_str(&subject, 60))
}

/// Truncate a string to `max_len` chars, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{}...", truncated)
    }
}

fn build_message(
    conn: &DbConn,
    project_id: i64,
    message_id: i64,
) -> Result<MessageContext, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();

    // Fetch the message
    let rows = conn
        .query_sync(
            "SELECT m.id, m.subject, m.body_md, m.importance, m.ack_required,
                    m.created_ts, m.thread_id, m.attachments,
                    a.name AS sender_name, a.program, a.model
             FROM messages m
             JOIN agents a ON a.id = m.sender_id
             WHERE m.id = ? AND m.project_id = ?",
            &[Value::BigInt(message_id), Value::BigInt(project_id)],
        )
        .map_err(|e| CliError::Other(format!("message query failed: {e}")))?;

    let row = rows
        .first()
        .ok_or_else(|| CliError::InvalidArgument(format!("message #{message_id} not found")))?;

    let subject: String = row.get_named("subject").unwrap_or_default();
    let body: String = row.get_named("body_md").unwrap_or_default();
    let importance: String = row.get_named("importance").unwrap_or_default();
    let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
    let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
    let thread_id: String = row.get_named("thread_id").unwrap_or_default();
    let attachments_json: String = row.get_named("attachments").unwrap_or_default();
    let sender_name: String = row.get_named("sender_name").unwrap_or_default();
    let program: String = row.get_named("program").unwrap_or_default();
    let model: String = row.get_named("model").unwrap_or_default();
    let thread_ref = canonical_thread_ref(message_id, &thread_id);

    // Recipients
    let to = load_recipient_display_names(conn, message_id, "message")?;

    // Ack status
    let ack_status = if ack_required == 0 {
        "none".to_string()
    } else {
        let acked_count: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients
                 WHERE message_id = ? AND ack_ts IS NOT NULL",
                &[Value::BigInt(message_id)],
            )
            .map_err(|e| CliError::Other(format!("message ack-count query failed: {e}")))?
            .first()
            .and_then(|r2| r2.get_named::<i64>("cnt").ok())
            .unwrap_or(0);
        let total_recipients: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients WHERE message_id = ?",
                &[Value::BigInt(message_id)],
            )
            .map_err(|e| CliError::Other(format!("message recipient-count query failed: {e}")))?
            .first()
            .and_then(|r2| r2.get_named::<i64>("cnt").ok())
            .unwrap_or(0);
        if total_recipients > 0 && acked_count >= total_recipients {
            "done".to_string()
        } else if total_recipients > 0 && acked_count > 0 {
            format!("partial ({acked_count}/{total_recipients})")
        } else if acked_count > 0 {
            format!("acked ({acked_count})")
        } else {
            "pending".to_string()
        }
    };

    // Thread context
    let (position, total_in_thread, previous, next) = if !thread_ref.is_empty() {
        let (thread_where, thread_params) = thread_scope_params("m", project_id, &thread_ref);
        let thread_rows = conn
            .query_sync(
                &format!(
                    "SELECT m.id, m.subject, m.created_ts, a.name AS sender
                     FROM messages m
                     JOIN agents a ON a.id = m.sender_id
                     WHERE {thread_where}
                     ORDER BY m.created_ts ASC, m.id ASC"
                ),
                &thread_params,
            )
            .map_err(|e| CliError::Other(format!("thread context query failed: {e}")))?;
        let total = thread_rows.len().max(1);
        let current_index = thread_rows
            .iter()
            .position(|thread_row| thread_row.get_named::<i64>("id").ok() == Some(message_id));
        let position = current_index.map_or(1, |idx| idx + 1);
        let previous = current_index
            .and_then(|idx| idx.checked_sub(1))
            .and_then(|idx| thread_rows.get(idx))
            .map(format_adjacent_thread_row);
        let next = current_index
            .and_then(|idx| thread_rows.get(idx + 1))
            .map(format_adjacent_thread_row);
        (position, total, previous, next)
    } else {
        (1, 1, None, None)
    };

    // Parse attachments JSON
    let attachments: Vec<AttachmentInfo> = parse_attachments_json(&attachments_json)
        .into_iter()
        .map(|attachment| AttachmentInfo {
            name: attachment.name,
            size: attachment.size_text,
            mime_type: attachment.mime_type,
        })
        .collect();

    let age_seconds = now_us.saturating_sub(created_ts) / 1_000_000;
    let created_iso = mcp_agent_mail_db::micros_to_iso(created_ts);

    Ok(MessageContext {
        id: message_id,
        from: sender_name,
        from_program: if program.is_empty() {
            None
        } else {
            Some(program)
        },
        from_model: if model.is_empty() { None } else { Some(model) },
        to,
        subject,
        body,
        thread: thread_ref,
        position,
        total_in_thread,
        importance,
        ack_status,
        created: created_iso,
        age: format_age(age_seconds),
        previous,
        next,
        attachments,
    })
}

// ── Search command implementation ───────────────────────────────────────────

/// Facet count entry.
#[derive(Debug, Serialize)]
struct FacetEntry {
    value: String,
    count: usize,
}

/// Search result data with facets.
#[derive(Debug, Serialize)]
struct SearchData {
    query: String,
    total_results: usize,
    results: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    by_thread: Vec<FacetEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    by_agent: Vec<FacetEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    by_importance: Vec<FacetEntry>,
}

fn build_search(
    conn: &DbConn,
    project_id: i64,
    query: &str,
    kind_filter: Option<&str>,
    importance_filter: Option<&str>,
    since: Option<&str>,
    limit: usize,
) -> Result<SearchData, CliError> {
    let raw_query = query.trim();
    if raw_query.is_empty() {
        return Ok(SearchData {
            query: query.to_string(),
            total_results: 0,
            results: vec![],
            by_thread: vec![],
            by_agent: vec![],
            by_importance: vec![],
        });
    }

    let now_us = mcp_agent_mail_db::now_micros();
    let mut search_query =
        mcp_agent_mail_db::search_planner::SearchQuery::messages(raw_query.to_string(), project_id);

    if let Some(imp) = importance_filter.map(str::trim).filter(|s| !s.is_empty()) {
        let parsed = mcp_agent_mail_db::search_planner::Importance::parse(imp);
        let Some(parsed) = parsed else {
            return Err(CliError::InvalidArgument(format!(
                "invalid importance filter: {imp}"
            )));
        };
        search_query.importance = vec![parsed];
    }

    if let Some(since_str) = since.map(str::trim).filter(|s| !s.is_empty()) {
        let since_us = parse_since_micros(since_str)?;
        search_query.time_range = mcp_agent_mail_db::search_planner::TimeRange {
            min_ts: Some(since_us),
            max_ts: None,
        };
    }

    let recipient_kind = kind_filter.map(str::trim).filter(|s| !s.is_empty());
    let search_rows = collect_search_rows(conn, &search_query, recipient_kind, limit)?;

    // Build results and facets
    let mut results = Vec::new();
    let mut thread_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut agent_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut importance_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for row in search_rows {
        let subject = row.title;
        let thread_ref = canonical_thread_ref_for_row(row.id, row.thread_id.as_deref());
        let importance = row.importance.unwrap_or_else(|| "normal".to_string());
        let created_ts = row.created_ts.unwrap_or(0);
        let sender = row.from_agent.unwrap_or_default();
        let snippet_source = if row.body.is_empty() {
            subject.clone()
        } else {
            row.body
        };
        let snippet = truncate_str(&snippet_source, 220);

        if !thread_ref.is_empty() {
            *thread_counts.entry(thread_ref.clone()).or_insert(0) += 1;
        }
        *agent_counts.entry(sender.clone()).or_insert(0) += 1;
        *importance_counts.entry(importance.clone()).or_insert(0) += 1;

        let age_seconds = if created_ts > 0 {
            now_us.saturating_sub(created_ts) / 1_000_000
        } else {
            0
        };

        results.push(SearchResult {
            id: row.id,
            relevance: row.score.unwrap_or(0.0),
            from: sender,
            subject,
            thread: thread_ref,
            snippet,
            age: format_age(age_seconds),
        });
    }

    // Sort facets by count descending
    let mut by_thread: Vec<FacetEntry> = thread_counts
        .into_iter()
        .map(|(v, c)| FacetEntry { value: v, count: c })
        .collect();
    by_thread.sort_by_key(|x| std::cmp::Reverse(x.count));

    let mut by_agent: Vec<FacetEntry> = agent_counts
        .into_iter()
        .map(|(v, c)| FacetEntry { value: v, count: c })
        .collect();
    by_agent.sort_by_key(|x| std::cmp::Reverse(x.count));

    let mut by_importance: Vec<FacetEntry> = importance_counts
        .into_iter()
        .map(|(v, c)| FacetEntry { value: v, count: c })
        .collect();
    by_importance.sort_by_key(|x| std::cmp::Reverse(x.count));

    let total = results.len();
    Ok(SearchData {
        query: query.to_string(),
        total_results: total,
        results,
        by_thread,
        by_agent,
        by_importance,
    })
}

fn collect_search_rows(
    conn: &DbConn,
    query: &mcp_agent_mail_db::search_planner::SearchQuery,
    recipient_kind: Option<&str>,
    limit: usize,
) -> Result<Vec<mcp_agent_mail_db::search_planner::SearchResult>, CliError> {
    let cfg = mcp_agent_mail_db::DbPoolConfig::from_env();
    let pool = mcp_agent_mail_db::create_pool(&cfg)
        .map_err(|e| CliError::Other(format!("failed to initialize DB pool for search: {e}")))?;
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| CliError::Other(format!("failed to build async runtime for search: {e}")))?;

    let requested_limit = limit.clamp(1, 1000);
    let mut paged_query = query.clone();
    paged_query.limit = Some(search_page_limit(requested_limit, recipient_kind));

    collect_search_rows_with_runner(
        conn,
        paged_query,
        recipient_kind,
        requested_limit,
        |query| execute_robot_search_query(&runtime, &pool, query),
    )
}

fn collect_search_rows_with_runner<F>(
    conn: &DbConn,
    mut paged_query: mcp_agent_mail_db::search_planner::SearchQuery,
    recipient_kind: Option<&str>,
    requested_limit: usize,
    mut run_query: F,
) -> Result<Vec<mcp_agent_mail_db::search_planner::SearchResult>, CliError>
where
    F: FnMut(
        &mcp_agent_mail_db::search_planner::SearchQuery,
    ) -> Result<mcp_agent_mail_db::search_planner::SearchResponse, CliError>,
{
    let mut out = Vec::with_capacity(requested_limit);
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_cursors = std::collections::HashSet::new();

    loop {
        let response = run_query(&paged_query)?;
        let kind_id_filter = match recipient_kind {
            Some(kind) => Some(search_message_ids_by_recipient_kind(
                conn,
                kind,
                &response.results,
            )?),
            None => None,
        };

        for row in response.results {
            if let Some(filter_ids) = &kind_id_filter
                && !filter_ids.contains(&row.id)
            {
                continue;
            }
            if !seen_ids.insert(row.id) {
                continue;
            }
            out.push(row);
            if out.len() >= requested_limit {
                return Ok(out);
            }
        }

        let Some(next_cursor) = response.next_cursor else {
            return Ok(out);
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            return Ok(out);
        }
        paged_query.cursor = Some(next_cursor);
    }
}

fn search_page_limit(limit: usize, recipient_kind: Option<&str>) -> usize {
    if recipient_kind.is_some() {
        limit.saturating_mul(4).clamp(limit, 1000)
    } else {
        limit
    }
}

fn execute_robot_search_query(
    runtime: &asupersync::runtime::Runtime,
    pool: &mcp_agent_mail_db::DbPool,
    query: &mcp_agent_mail_db::search_planner::SearchQuery,
) -> Result<mcp_agent_mail_db::search_planner::SearchResponse, CliError> {
    let cx = asupersync::Cx::for_request();

    match runtime.block_on(async {
        mcp_agent_mail_db::search_service::execute_search_simple(&cx, pool, query).await
    }) {
        Outcome::Ok(resp) => Ok(resp),
        Outcome::Err(e) => Err(CliError::Other(format!("search query failed: {e}"))),
        Outcome::Cancelled(_) => Err(CliError::Other("search request cancelled".to_string())),
        Outcome::Panicked(p) => Err(CliError::Other(format!("search request panicked: {p}"))),
    }
}

fn search_message_ids_by_recipient_kind(
    conn: &DbConn,
    kind: &str,
    results: &[mcp_agent_mail_db::search_planner::SearchResult],
) -> Result<std::collections::HashSet<i64>, CliError> {
    if results.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    let mut ids = Vec::with_capacity(results.len());
    for result in results {
        ids.push(result.id);
    }

    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT DISTINCT message_id FROM message_recipients \
         WHERE kind = ? AND message_id IN ({placeholders})"
    );

    let mut params = Vec::with_capacity(ids.len() + 1);
    params.push(Value::Text(kind.to_string()));
    params.extend(ids.into_iter().map(Value::BigInt));

    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| CliError::Other(format!("kind filter query failed: {e}")))?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        if let Ok(id) = row.get_named::<i64>("message_id") {
            out.insert(id);
        }
    }
    Ok(out)
}

fn summarize_integrity_probe(metrics: &mcp_agent_mail_db::IntegrityMetrics) -> (HealthProbe, bool) {
    let has_checks = metrics.checks_total > 0;
    let last_check_failed = has_checks && metrics.last_ok_ts < metrics.last_check_ts;
    let runtime_failures_without_checks = !has_checks && metrics.failures_total > 0;
    let detail = if !has_checks && metrics.failures_total == 0 {
        "No checks run yet".to_string()
    } else if runtime_failures_without_checks {
        format!(
            "No integrity checks run yet; {} runtime corruption failures recorded",
            metrics.failures_total
        )
    } else if last_check_failed {
        format!(
            "last check failed; {} cumulative failures across {} checks",
            metrics.failures_total, metrics.checks_total
        )
    } else if metrics.failures_total == 0 {
        format!("{} checks, all passed", metrics.checks_total)
    } else {
        format!(
            "last check passed; {} historical failures across {} checks",
            metrics.failures_total, metrics.checks_total
        )
    };

    (
        HealthProbe {
            name: "integrity".into(),
            status: if last_check_failed || runtime_failures_without_checks {
                "warn"
            } else {
                "ok"
            }
            .into(),
            latency_ms: 0.0,
            detail,
        },
        !(last_check_failed || runtime_failures_without_checks),
    )
}

// ── Reservations command implementation ─────────────────────────────────────

/// Conflict between two reservations.
#[derive(Debug, Clone, Serialize)]
struct ReservationConflict {
    agent_a: String,
    path_a: String,
    agent_b: String,
    path_b: String,
}

/// Full reservations response data.
#[derive(Debug, Serialize)]
struct ReservationsData {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    my_reservations: Vec<ReservationEntry>,
    all_active: Vec<ReservationEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<ReservationConflict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    expiring_soon: Vec<ReservationEntry>,
}

/// Format remaining seconds with warning markers.
fn format_remaining(seconds: i64) -> String {
    let base = format_age(seconds).replace(" ago", "");
    if seconds < 120 {
        format!("{base} \u{26a0}\u{26a0}")
    } else if seconds < 600 {
        format!("{base} \u{26a0}")
    } else {
        base
    }
}

fn build_reservations(
    conn: &DbConn,
    project_id: i64,
    project_slug: &str,
    agent: Option<(i64, String)>,
    show_all: bool,
    conflicts_only: bool,
    expiring_minutes: Option<u32>,
) -> Result<(ReservationsData, Vec<String>), CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    let expiring_threshold = now_us + i64::from(expiring_minutes.unwrap_or(10)) * 60 * 1_000_000;
    let has_release_ledger = has_file_reservation_release_ledger(conn);
    let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
    let active_reservation_join =
        active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );

    // Fetch all active reservations
    let all_rows = conn
        .query_sync(
            &format!(
                "SELECT fr.id, fr.path_pattern, fr.\"exclusive\", fr.created_ts, fr.expires_ts,
                        a.name AS agent_name, a.id AS agent_id
                 FROM file_reservations fr{active_reservation_join}
                 JOIN agents a ON a.id = fr.agent_id
                 WHERE fr.project_id = ? AND ({active_reservation_predicate}) AND fr.expires_ts > ?
                 ORDER BY fr.expires_ts ASC"
            ),
            &[Value::BigInt(project_id), Value::BigInt(now_us)],
        )
        .map_err(|e| CliError::Other(format!("reservations query failed: {e}")))?;

    let mut all_active = Vec::new();
    let mut my_reservations = Vec::new();
    let mut expiring_soon = Vec::new();

    for row in &all_rows {
        let path: String = row.get_named("path_pattern").unwrap_or_default();
        let exclusive: bool = row.get_named::<i64>("exclusive").unwrap_or(1) != 0;
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
        let expires_ts: i64 = row.get_named("expires_ts").unwrap_or(0);
        let agent_name: String = row.get_named("agent_name").unwrap_or_default();
        let agent_id_row: i64 = row.get_named("agent_id").unwrap_or(0);

        let remaining_seconds = expires_ts.saturating_sub(now_us) / 1_000_000;
        let created_age = now_us.saturating_sub(created_ts) / 1_000_000;

        let entry = ReservationEntry {
            agent: Some(agent_name.clone()),
            path: path.clone(),
            exclusive,
            remaining_seconds,
            remaining: Some(format_remaining(remaining_seconds)),
            granted_at: Some(format_age(created_age)),
        };

        all_active.push(entry.clone());

        if let Some((my_id, _)) = &agent
            && agent_id_row == *my_id
        {
            my_reservations.push(entry.clone());
        }

        if expires_ts < expiring_threshold {
            expiring_soon.push(entry);
        }
    }

    // Detect conflicts (exclusive reservations with overlapping paths from different agents)
    let mut conflicts = Vec::new();
    for (i, a) in all_active.iter().enumerate() {
        if !a.exclusive {
            continue;
        }
        for b in all_active.iter().skip(i + 1) {
            if !b.exclusive {
                continue;
            }
            if a.agent == b.agent {
                continue;
            }
            // Check overlap using glob pattern matching
            if glob_matches(&a.path, &b.path) || glob_matches(&b.path, &a.path) || a.path == b.path
            {
                let left = (a.agent.as_deref().unwrap_or_default(), a.path.as_str());
                let right = (b.agent.as_deref().unwrap_or_default(), b.path.as_str());
                let ((agent_a, path_a), (agent_b, path_b)) = if left <= right {
                    (
                        (a.agent.clone().unwrap_or_default(), a.path.clone()),
                        (b.agent.clone().unwrap_or_default(), b.path.clone()),
                    )
                } else {
                    (
                        (b.agent.clone().unwrap_or_default(), b.path.clone()),
                        (a.agent.clone().unwrap_or_default(), a.path.clone()),
                    )
                };
                conflicts.push(ReservationConflict {
                    agent_a,
                    path_a,
                    agent_b,
                    path_b,
                });
            }
        }
    }
    conflicts.sort_by(|left, right| {
        left.agent_a
            .cmp(&right.agent_a)
            .then(left.path_a.cmp(&right.path_a))
            .then(left.agent_b.cmp(&right.agent_b))
            .then(left.path_b.cmp(&right.path_b))
    });
    conflicts.dedup_by(|left, right| {
        left.agent_a == right.agent_a
            && left.path_a == right.path_a
            && left.agent_b == right.agent_b
            && left.path_b == right.path_b
    });

    let scoped_agent_name = if !show_all {
        agent.as_ref().map(|(_, name)| name.as_str())
    } else {
        None
    };
    let scoped_conflicts: Vec<_> = if let Some(agent_name) = scoped_agent_name {
        conflicts
            .iter()
            .filter(|conflict| conflict.agent_a == agent_name || conflict.agent_b == agent_name)
            .cloned()
            .collect()
    } else {
        conflicts.clone()
    };
    let scoped_expiring_soon: Vec<_> = if let Some(agent_name) = scoped_agent_name {
        expiring_soon
            .iter()
            .filter(|entry| entry.agent.as_deref() == Some(agent_name))
            .cloned()
            .collect()
    } else {
        expiring_soon.clone()
    };
    let scoped_all_active = if scoped_agent_name.is_some() {
        my_reservations.clone()
    } else {
        all_active.clone()
    };

    // Build actions
    let mut actions = Vec::new();
    if let Some((_, ref agent_name)) = agent {
        for entry in &scoped_expiring_soon {
            if entry.agent.as_deref() == Some(agent_name.as_str()) {
                actions.push(format!(
                    "am file_reservations renew {project_slug} {agent_name} --paths {} --extend-seconds 3600",
                    entry.path
                ));
            }
        }
    }

    // Apply filters
    if conflicts_only {
        // Only keep entries involved in conflicts
        let conflict_paths: std::collections::HashSet<String> = scoped_conflicts
            .iter()
            .flat_map(|c| vec![c.path_a.clone(), c.path_b.clone()])
            .collect();
        let filtered: Vec<_> = scoped_all_active
            .into_iter()
            .filter(|e| conflict_paths.contains(&e.path))
            .collect();
        return Ok((
            ReservationsData {
                my_reservations: vec![],
                all_active: filtered,
                conflicts: scoped_conflicts,
                expiring_soon: vec![],
            },
            actions,
        ));
    }

    if scoped_agent_name.is_some() {
        // When not --all, only show my reservations in all_active
        return Ok((
            ReservationsData {
                my_reservations: my_reservations.clone(),
                all_active: scoped_all_active,
                conflicts: scoped_conflicts,
                expiring_soon: scoped_expiring_soon,
            },
            actions,
        ));
    }

    Ok((
        ReservationsData {
            my_reservations,
            all_active,
            conflicts,
            expiring_soon,
        },
        actions,
    ))
}

/// Check whether two reservation path patterns can overlap.
fn glob_matches(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }

    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    if glob::Pattern::new(pattern)
        .ok()
        .is_some_and(|compiled| compiled.matches_with(path, options))
    {
        return true;
    }
    if glob::Pattern::new(path)
        .ok()
        .is_some_and(|compiled| compiled.matches_with(pattern, options))
    {
        return true;
    }

    if !pattern.is_empty() && !path.is_empty() {
        if path.starts_with(pattern) && path.as_bytes().get(pattern.len()) == Some(&b'/') {
            return true;
        }
        if pattern.starts_with(path) && pattern.as_bytes().get(path.len()) == Some(&b'/') {
            return true;
        }
    }

    false
}

// ── Timeline command implementation ─────────────────────────────────────────

fn build_timeline(
    conn: &DbConn,
    project_id: i64,
    since: Option<&str>,
    kind_filter: Option<&str>,
    source_filter: Option<&str>,
) -> Result<Vec<TimelineEvent>, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();

    // Default "since" to 24h ago
    let since_us = if let Some(s) = since {
        parse_since_micros(s)?
    } else {
        now_us - 24 * 3600 * 1_000_000
    };

    let mut events: Vec<TimelineEvent> = Vec::new();

    // Message events
    if kind_filter.is_none() || kind_filter == Some("message") {
        let msg_rows = conn
            .query_sync(
                "SELECT m.id, m.subject, m.created_ts, m.importance, a.name AS sender
                 FROM messages m
                 JOIN agents a ON a.id = m.sender_id
                 WHERE m.project_id = ? AND m.created_ts > ?
                 ORDER BY m.created_ts ASC",
                &[Value::BigInt(project_id), Value::BigInt(since_us)],
            )
            .map_err(|e| CliError::Other(format!("timeline messages query: {e}")))?;

        for row in &msg_rows {
            let id: i64 = row.get_named("id").unwrap_or(0);
            let subject: String = row.get_named("subject").unwrap_or_default();
            let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
            let importance: String = row.get_named("importance").unwrap_or_default();
            let sender: String = row.get_named("sender").unwrap_or_default();

            if source_filter.is_some() && source_filter != Some(sender.as_str()) {
                continue;
            }

            events.push(TimelineEvent {
                seq: 0,
                timestamp: mcp_agent_mail_db::micros_to_iso(created_ts),
                kind: "message".to_string(),
                summary: format!(
                    "#{id} [{importance}] {sender}: {}",
                    truncate_str(&subject, 60)
                ),
                source: sender,
            });
        }
    }

    // Reservation events
    if kind_filter.is_none() || kind_filter == Some("reservation") {
        let has_release_ledger = has_file_reservation_release_ledger(conn);
        let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
        let release_join = active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
        let released_ts_sql = reservation_released_ts_sql(
            has_release_ledger,
            has_legacy_released_ts_column,
            "fr",
            "rr",
        );
        let res_rows = conn
            .query_sync(
                &format!(
                    "SELECT fr.id, fr.path_pattern, fr.created_ts, {released_ts_sql} AS released_ts, a.name AS agent
                 FROM file_reservations fr{release_join}
                 JOIN agents a ON a.id = fr.agent_id
                 WHERE fr.project_id = ?
                   AND (fr.created_ts > ? OR (({released_ts_sql}) IS NOT NULL AND ({released_ts_sql}) > ?))
                 ORDER BY fr.created_ts ASC"
                ),
                &[
                    Value::BigInt(project_id),
                    Value::BigInt(since_us),
                    Value::BigInt(since_us),
                ],
            )
            .map_err(|e| CliError::Other(format!("timeline reservations query: {e}")))?;

        for row in &res_rows {
            let path: String = row.get_named("path_pattern").unwrap_or_default();
            let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
            let released_ts: Option<i64> = row.get_named("released_ts").ok();
            let agent: String = row.get_named("agent").unwrap_or_default();

            if source_filter.is_some() && source_filter != Some(agent.as_str()) {
                continue;
            }

            if created_ts > since_us {
                events.push(TimelineEvent {
                    seq: 0,
                    timestamp: mcp_agent_mail_db::micros_to_iso(created_ts),
                    kind: "reservation".to_string(),
                    summary: format!("{agent} reserved {path}"),
                    source: agent.clone(),
                });
            }
            if let Some(rel_ts) = released_ts
                && rel_ts > since_us
            {
                events.push(TimelineEvent {
                    seq: 0,
                    timestamp: mcp_agent_mail_db::micros_to_iso(rel_ts),
                    kind: "reservation".to_string(),
                    summary: format!("{agent} released {path}"),
                    source: agent.clone(),
                });
            }
        }
    }

    // Agent events (registration)
    if kind_filter.is_none() || kind_filter == Some("agent") {
        let agent_rows = conn
            .query_sync(
                "SELECT name, inception_ts, program
                 FROM agents
                 WHERE project_id = ? AND inception_ts > ?
                 ORDER BY inception_ts ASC",
                &[Value::BigInt(project_id), Value::BigInt(since_us)],
            )
            .map_err(|e| CliError::Other(format!("timeline agents query: {e}")))?;

        for row in &agent_rows {
            let name: String = row.get_named("name").unwrap_or_default();
            let inception_ts: i64 = row.get_named("inception_ts").unwrap_or(0);
            let program: String = row.get_named("program").unwrap_or_default();

            if source_filter.is_some() && source_filter != Some(name.as_str()) {
                continue;
            }

            events.push(TimelineEvent {
                seq: 0,
                timestamp: mcp_agent_mail_db::micros_to_iso(inception_ts),
                kind: "agent".to_string(),
                summary: format!("{name} registered ({program})"),
                source: name,
            });
        }
    }

    // Sort by timestamp and assign sequence numbers
    events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    for (i, ev) in events.iter_mut().enumerate() {
        ev.seq = i + 1;
    }

    Ok(events)
}

// ── Overview command implementation ─────────────────────────────────────────

fn build_overview(conn: &DbConn) -> Result<Vec<OverviewProject>, CliError> {
    fn decode_count(row: &sqlmodel_core::Row, label: &str) -> Result<i64, CliError> {
        row.get_by_name("cnt")
            .and_then(sqlmodel_core::Value::as_i64)
            .ok_or_else(|| CliError::Other(format!("{label} query returned a non-integer count")))
    }

    let now_us = mcp_agent_mail_db::now_micros();
    let has_release_ledger = has_file_reservation_release_ledger(conn);
    let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
    let active_reservation_join =
        active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );

    let rows = conn
        .query_sync("SELECT id, slug FROM projects ORDER BY slug ASC", &[])
        .map_err(|e| CliError::Other(format!("overview projects query: {e}")))?;

    let mut projects = Vec::new();
    for row in &rows {
        let pid = row
            .get_by_name("id")
            .and_then(sqlmodel_core::Value::as_i64)
            .ok_or_else(|| CliError::Other("overview project row missing integer id".into()))?;
        let slug = row
            .get_named::<String>("slug")
            .map_err(|e| CliError::Other(format!("overview project slug decode failed: {e}")))?;

        // Count unread messages across all agents in project
        let unread: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients mr
                 JOIN messages m ON m.id = mr.message_id
                WHERE m.project_id = ? AND mr.read_ts IS NULL",
                &[Value::BigInt(pid)],
            )
            .map_err(|e| CliError::Other(format!("overview unread query failed: {e}")))?
            .first()
            .map(|row| decode_count(row, "overview unread"))
            .transpose()?
            .unwrap_or(0);

        // Count urgent/high unread messages
        let urgent: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients mr
                 JOIN messages m ON m.id = mr.message_id
                 WHERE m.project_id = ? AND m.importance IN ('urgent', 'high')
                 AND mr.read_ts IS NULL",
                &[Value::BigInt(pid)],
            )
            .map_err(|e| CliError::Other(format!("overview urgent query failed: {e}")))?
            .first()
            .map(|row| decode_count(row, "overview urgent"))
            .transpose()?
            .unwrap_or(0);

        // Count actionable ack-overdue items using the same 30m threshold as status/inbox.
        let ack_overdue: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients mr
                 JOIN messages m ON m.id = mr.message_id
                 WHERE m.project_id = ? AND m.ack_required = 1 AND mr.ack_ts IS NULL
                   AND m.created_ts < ?",
                &[
                    Value::BigInt(pid),
                    Value::BigInt(now_us - ACK_OVERDUE_THRESHOLD_US),
                ],
            )
            .map_err(|e| CliError::Other(format!("overview ack-overdue query failed: {e}")))?
            .first()
            .map(|row| decode_count(row, "overview ack-overdue"))
            .transpose()?
            .unwrap_or(0);

        // Count active reservations
        let reservations: i64 = conn
            .query_sync(
                &format!(
                    "SELECT COUNT(*) AS cnt
                     FROM file_reservations fr{active_reservation_join}
                     WHERE fr.project_id = ? AND ({active_reservation_predicate}) AND fr.expires_ts > ?"
                ),
                &[Value::BigInt(pid), Value::BigInt(now_us)],
            )
            .map_err(|e| CliError::Other(format!("overview reservations query failed: {e}")))?
            .first()
            .map(|row| decode_count(row, "overview reservations"))
            .transpose()?
            .unwrap_or(0);

        projects.push(OverviewProject {
            slug,
            unread: unread as usize,
            urgent: urgent as usize,
            ack_overdue: ack_overdue as usize,
            reservations: reservations as usize,
        });
    }

    Ok(projects)
}

// ── Analytics command implementation ────────────────────────────────────────

fn build_analytics(
    conn: &DbConn,
    project_id: i64,
    agent: Option<(i64, String)>,
) -> Result<Vec<AnomalyCard>, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    let mut anomalies = Vec::new();
    let has_release_ledger = has_file_reservation_release_ledger(conn);

    // Check for ack SLA violations (>1h old unacked)
    let ack_overdue: i64 = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM message_recipients mr
             JOIN messages m ON m.id = mr.message_id
             WHERE m.project_id = ? AND m.ack_required = 1 AND mr.ack_ts IS NULL
               AND m.created_ts < ?",
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us - ACK_SLA_VIOLATION_THRESHOLD_US),
            ],
        )
        .map_err(|e| CliError::Other(format!("analytics ack-overdue query failed: {e}")))?
        .first()
        .and_then(|r| r.get_named("cnt").ok())
        .unwrap_or(0);
    if ack_overdue > 0 {
        anomalies.push(AnomalyCard {
            severity: "warn".to_string(),
            confidence: 0.9,
            category: "ack_sla".to_string(),
            headline: format!("{ack_overdue} messages ack-overdue (>1h)"),
            rationale: "Messages requiring acknowledgement have been pending over 1 hour"
                .to_string(),
            remediation: "am robot inbox --ack-overdue".to_string(),
        });
    }

    // Check for reservation conflicts
    let active_reservation_join_fr1 =
        active_reservation_release_join_sql(has_release_ledger, "fr1", "rr1");
    let active_reservation_join_fr2 =
        active_reservation_release_join_sql(has_release_ledger, "fr2", "rr2");
    let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
    let active_reservation_predicate_fr1 = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr1",
        "rr1",
    );
    let active_reservation_predicate_fr2 = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr2",
        "rr2",
    );
    let conflict_rows = conn
        .query_sync(
            &format!(
                "SELECT fr1.path_pattern AS p1, fr2.path_pattern AS p2,
                        a1.name AS agent1, a2.name AS agent2
                 FROM file_reservations fr1{active_reservation_join_fr1}
                 JOIN file_reservations fr2{active_reservation_join_fr2} ON fr1.id < fr2.id
                   AND fr1.project_id = fr2.project_id
                   AND fr1.agent_id != fr2.agent_id
                 JOIN agents a1 ON a1.id = fr1.agent_id
                 JOIN agents a2 ON a2.id = fr2.agent_id
                 WHERE fr1.project_id = ? AND fr1.\"exclusive\" = 1 AND fr2.\"exclusive\" = 1
                   AND ({active_reservation_predicate_fr1}) AND ({active_reservation_predicate_fr2})
                   AND fr1.expires_ts > ? AND fr2.expires_ts > ?"
            ),
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us),
                Value::BigInt(now_us),
            ],
        )
        .map_err(|e| CliError::Other(format!("analytics conflicts query failed: {e}")))?;
    let mut conflict_count = 0;
    for row in &conflict_rows {
        let p1: String = row.get_named("p1").unwrap_or_default();
        let p2: String = row.get_named("p2").unwrap_or_default();
        if glob_matches(&p1, &p2) || glob_matches(&p2, &p1) || p1 == p2 {
            conflict_count += 1;
        }
    }
    if conflict_count > 0 {
        anomalies.push(AnomalyCard {
            severity: "error".to_string(),
            confidence: 1.0,
            category: "reservation_conflict".to_string(),
            headline: format!("{conflict_count} reservation conflict(s) detected"),
            rationale: "Multiple agents hold exclusive reservations on overlapping paths"
                .to_string(),
            remediation: "am robot reservations --conflicts".to_string(),
        });
    }

    // Check for expiring-soon reservations
    let expiring_threshold = now_us + 10 * 60 * 1_000_000;
    let active_reservation_join =
        active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
    let expiring_count: i64 = if let Some((agent_id, _)) = &agent {
        conn.query_sync(
            &format!(
                "SELECT COUNT(*) AS cnt
                 FROM file_reservations fr{active_reservation_join}
                 WHERE fr.project_id = ? AND fr.agent_id = ? AND ({active_reservation_predicate})
                   AND fr.expires_ts > ? AND fr.expires_ts < ?"
            ),
            &[
                Value::BigInt(project_id),
                Value::BigInt(*agent_id),
                Value::BigInt(now_us),
                Value::BigInt(expiring_threshold),
            ],
        )
    } else {
        conn.query_sync(
            &format!(
                "SELECT COUNT(*) AS cnt
                 FROM file_reservations fr{active_reservation_join}
                 WHERE fr.project_id = ? AND ({active_reservation_predicate})
                   AND fr.expires_ts > ? AND fr.expires_ts < ?"
            ),
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us),
                Value::BigInt(expiring_threshold),
            ],
        )
    }
    .map_err(|e| CliError::Other(format!("analytics expiring reservations query failed: {e}")))?
    .first()
    .and_then(|r| r.get_named("cnt").ok())
    .unwrap_or(0);
    if expiring_count > 0 {
        anomalies.push(AnomalyCard {
            severity: "warn".to_string(),
            confidence: 0.95,
            category: "reservation_expiry".to_string(),
            headline: format!("{expiring_count} reservation(s) expiring within 10 minutes"),
            rationale: "Reservations nearing expiry may cause unprotected concurrent edits"
                .to_string(),
            remediation: "am robot reservations --expiring=10".to_string(),
        });
    }

    // Check for idle agents (registered but no activity in 24h)
    let idle_rows = conn
        .query_sync(
            "SELECT a.name FROM agents a
             WHERE a.project_id = ? AND a.last_active_ts < ?
               AND NOT EXISTS (
                   SELECT 1 FROM messages m WHERE m.sender_id = a.id AND m.created_ts > ?
               )",
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us - 24 * 3600 * 1_000_000),
                Value::BigInt(now_us - 24 * 3600 * 1_000_000),
            ],
        )
        .map_err(|e| CliError::Other(format!("analytics idle agents query failed: {e}")))?;
    if !idle_rows.is_empty() {
        let idle_names: Vec<String> = idle_rows
            .iter()
            .filter_map(|r| r.get_named("name").ok())
            .collect();
        anomalies.push(AnomalyCard {
            severity: "info".to_string(),
            confidence: 0.7,
            category: "agent_idle".to_string(),
            headline: format!("{} agent(s) idle >24h", idle_names.len()),
            rationale: format!("Agents inactive: {}", idle_names.join(", ")),
            remediation: "am robot agents".to_string(),
        });
    }

    // Tool error rate anomaly
    let snapshot = mcp_agent_mail_tools::tool_metrics_snapshot();
    for entry in &snapshot {
        if entry.calls >= 10 {
            let error_pct = (entry.errors as f64 / entry.calls as f64) * 100.0;
            if error_pct > 25.0 {
                anomalies.push(AnomalyCard {
                    severity: "warn".to_string(),
                    confidence: 0.85,
                    category: "tool_errors".to_string(),
                    headline: format!("{} error rate {error_pct:.1}%", entry.name),
                    rationale: format!(
                        "{}/{} calls failed for {}",
                        entry.errors, entry.calls, entry.name
                    ),
                    remediation: "am robot metrics".to_string(),
                });
            }
        }
    }

    Ok(anomalies)
}

// ── Agents command implementation ───────────────────────────────────────────

fn build_agents(
    conn: &DbConn,
    project_id: i64,
    active_only: bool,
    sort_field: Option<&str>,
) -> Result<Vec<AgentRow>, CliError> {
    struct PendingAgentRow {
        row: AgentRow,
        last_active_ts: i64,
    }

    let now_us = mcp_agent_mail_db::now_micros();
    let active_threshold = now_us - 15 * 60 * 1_000_000; // 15 min
    let idle_threshold = now_us - 4 * 3600 * 1_000_000; // 4 hours

    let rows = conn
        .query_sync(
            "SELECT a.id, a.name, a.program, a.model, a.last_active_ts,
                    (SELECT COUNT(*) FROM messages m WHERE m.sender_id = a.id) AS msg_count
             FROM agents a
             WHERE a.project_id = ?
             ORDER BY a.last_active_ts DESC, a.id DESC",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| CliError::Other(format!("agents query: {e}")))?;

    let mut agents_by_name: std::collections::HashMap<String, PendingAgentRow> =
        std::collections::HashMap::new();
    for row in &rows {
        let name: String = row.get_named("name").unwrap_or_default();
        let program: String = row.get_named("program").unwrap_or_default();
        let model: String = row.get_named("model").unwrap_or_default();
        let last_active_ts: i64 = row.get_named("last_active_ts").unwrap_or(0);
        let msg_count: i64 = row.get_named("msg_count").unwrap_or(0);

        let status = if last_active_ts >= active_threshold {
            "active"
        } else if last_active_ts >= idle_threshold {
            "idle"
        } else {
            "inactive"
        };

        if active_only && status != "active" {
            continue;
        }

        let age_seconds = now_us.saturating_sub(last_active_ts) / 1_000_000;

        let logical_name = name.to_lowercase();
        let candidate = PendingAgentRow {
            row: AgentRow {
                name,
                program,
                model,
                last_active: format_age(age_seconds),
                msg_count: msg_count as usize,
                status: status.to_string(),
            },
            last_active_ts,
        };
        match agents_by_name.entry(logical_name) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(candidate);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if candidate.last_active_ts > slot.get().last_active_ts {
                    slot.insert(candidate);
                }
            }
        }
    }
    let mut pending_agents: Vec<PendingAgentRow> = agents_by_name.into_values().collect();

    // Sort
    match sort_field {
        Some("name") => pending_agents.sort_by(|a, b| a.row.name.cmp(&b.row.name)),
        Some("msg_count") => pending_agents.sort_by_key(|x| std::cmp::Reverse(x.row.msg_count)),
        _ => pending_agents.sort_by_key(|x| {
            (
                std::cmp::Reverse(x.last_active_ts),
                std::cmp::Reverse(x.row.msg_count),
            )
        }),
    }

    Ok(pending_agents
        .into_iter()
        .map(|pending| pending.row)
        .collect())
}

// ── Contacts command implementation ─────────────────────────────────────────

fn build_contacts(conn: &DbConn, project_id: i64) -> Result<Vec<ContactRow>, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();

    let rows = conn
        .query_sync(
            "SELECT al.status, al.reason, al.updated_ts,
                    a1.name AS from_agent, a1.contact_policy AS from_policy,
                    a2.name AS to_agent
             FROM agent_links al
             JOIN agents a1 ON a1.id = al.a_agent_id
             JOIN agents a2 ON a2.id = al.b_agent_id
             WHERE al.a_project_id = ?
             ORDER BY al.updated_ts DESC",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| CliError::Other(format!("contacts query: {e}")))?;

    let mut contacts = Vec::new();
    for row in &rows {
        let from: String = row.get_named("from_agent").unwrap_or_default();
        let to: String = row.get_named("to_agent").unwrap_or_default();
        let status: String = row.get_named("status").unwrap_or_default();
        let from_policy: String = row.get_named("from_policy").unwrap_or_default();
        let reason: String = row.get_named("reason").unwrap_or_default();
        let updated_ts: i64 = row.get_named("updated_ts").unwrap_or(0);

        let age = now_us.saturating_sub(updated_ts) / 1_000_000;

        contacts.push(ContactRow {
            from,
            to,
            status,
            policy: from_policy,
            reason,
            updated: format_age(age),
        });
    }

    Ok(contacts)
}

// ── Projects command implementation ─────────────────────────────────────────

fn build_projects(conn: &DbConn) -> Result<Vec<ProjectRow>, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    let has_release_ledger = has_file_reservation_release_ledger(conn);
    let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
    let active_reservation_join =
        active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );

    let rows = conn
        .query_sync(
            &format!(
                "SELECT p.id, p.slug, p.human_key, p.created_at,
                        (SELECT COUNT(*) FROM agents a WHERE a.project_id = p.id) AS agent_count,
                        (SELECT COUNT(*) FROM messages m WHERE m.project_id = p.id) AS msg_count,
                        (SELECT COUNT(*) FROM file_reservations fr{active_reservation_join}
                         WHERE fr.project_id = p.id AND ({active_reservation_predicate}) AND fr.expires_ts > ?) AS res_count
                 FROM projects p
                 ORDER BY p.slug ASC"
            ),
            &[Value::BigInt(now_us)],
        )
        .map_err(|e| CliError::Other(format!("projects query: {e}")))?;

    let mut projects = Vec::new();
    for row in &rows {
        let slug: String = row.get_named("slug").unwrap_or_default();
        let path: String = row.get_named("human_key").unwrap_or_default();
        let agent_count: i64 = row.get_named("agent_count").unwrap_or(0);
        let msg_count: i64 = row.get_named("msg_count").unwrap_or(0);
        let res_count: i64 = row.get_named("res_count").unwrap_or(0);
        let created_at: i64 = row.get_named("created_at").unwrap_or(0);

        let age = now_us.saturating_sub(created_at) / 1_000_000;

        projects.push(ProjectRow {
            slug,
            path,
            agents: agent_count as usize,
            messages: msg_count as usize,
            reservations: res_count as usize,
            created: format_age(age),
        });
    }

    Ok(projects)
}

// ── Navigate command implementation ──────────────────────────────────────────

/// Resolved navigate data - wraps whatever resource was requested.
#[derive(Debug, Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
enum NavigateResult {
    Projects {
        projects: Vec<ProjectRow>,
    },
    Agents {
        agents: Vec<AgentRow>,
    },
    Inbox {
        entries: Vec<InboxEntry>,
    },
    Thread {
        thread: ThreadData,
    },
    Message {
        message: MessageContext,
    },
    Generic {
        resource_type: String,
        data: serde_json::Value,
    },
}

fn navigate_mcp_error_to_cli_error(err: McpError) -> CliError {
    match err.code {
        McpErrorCode::InvalidParams | McpErrorCode::ResourceNotFound => {
            CliError::InvalidArgument(err.message)
        }
        _ => CliError::Other(err.message),
    }
}

fn navigate_query_string(query: &std::collections::HashMap<String, String>) -> String {
    let mut entries: Vec<(&str, &str)> = query
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    entries.sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
    entries
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn navigate_param_with_query(
    base: &str,
    query: &std::collections::HashMap<String, String>,
) -> String {
    if query.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", navigate_query_string(query))
    }
}

fn navigate_query_with_default_project(
    query: &std::collections::HashMap<String, String>,
    default_project: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut effective = query.clone();
    if !effective.contains_key("project")
        && let Some(project) = default_project
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        effective.insert("project".to_string(), project.to_string());
    }
    effective
}

fn navigate_json_result(
    resource_type: &str,
    payload: String,
    project_scope: Option<String>,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let data = serde_json::from_str::<serde_json::Value>(&payload)
        .map_err(|e| CliError::Other(format!("navigate resource JSON parse failed: {e}")))?;
    Ok((
        NavigateResult::Generic {
            resource_type: resource_type.to_string(),
            data,
        },
        project_scope,
    ))
}

fn navigate_async_resource<F, Fut>(
    resource_type: &str,
    project_scope: Option<String>,
    call: F,
) -> Result<(NavigateResult, Option<String>), CliError>
where
    F: FnOnce(McpContext) -> Fut,
    Fut: std::future::Future<Output = McpResult<String>>,
{
    let payload = crate::context::run_async(async move {
        let ctx = McpContext::new(asupersync::Cx::for_request(), 1);
        call(ctx).await.map_err(navigate_mcp_error_to_cli_error)
    })?;
    navigate_json_result(resource_type, payload, project_scope)
}

fn navigate_should_use_canonical_resource(uri: &str) -> bool {
    let Ok((path, _query)) = split_resource_path_and_query(uri) else {
        return false;
    };
    let parts: Vec<String> = path
        .split('/')
        .map(percent_decode_resource_path_component)
        .collect();
    let parts_ref: Vec<&str> = parts.iter().map(String::as_str).collect();
    matches!(
        parts_ref.as_slice(),
        ["projects"]
            | ["inbox", ..]
            | ["mailbox", ..]
            | ["mailbox-with-commits", ..]
            | ["outbox", ..]
            | ["views", "urgent-unread", ..]
            | ["views", "ack-required", ..]
            | ["views", "acks-stale", ..]
            | ["views", "ack-overdue", ..]
            | ["file_reservations"]
            | ["file_reservations", ..]
            | ["reservations"]
            | ["reservations", ..]
    )
}

fn build_navigate_without_db(
    uri: &str,
) -> Result<Option<(NavigateResult, Option<String>)>, CliError> {
    let (path, query) = split_resource_path_and_query(uri)?;
    let parts: Vec<String> = path
        .split('/')
        .map(percent_decode_resource_path_component)
        .collect();
    let parts_ref: Vec<&str> = parts.iter().map(String::as_str).collect();

    match parts_ref.as_slice() {
        ["config", "environment"] => Ok(Some(build_navigate_config_environment())),
        ["identity", project_ref] => Ok(Some(build_navigate_identity(project_ref)?)),
        ["tooling", ..] => Ok(Some(build_navigate_tooling(&parts_ref, &query)?)),
        _ => Ok(None),
    }
}

fn build_navigate_from_canonical_resource(
    uri: &str,
    default_project: Option<&str>,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let (path, query) = split_resource_path_and_query(uri)?;
    let parts: Vec<String> = path
        .split('/')
        .map(percent_decode_resource_path_component)
        .collect();
    let parts_ref: Vec<&str> = parts.iter().map(String::as_str).collect();

    match parts_ref.as_slice() {
        ["projects"] => {
            let conn = crate::open_db_sync_robot()?;
            let mut rows = conn
                .query_sync(
                    "SELECT id, slug, human_key, created_at
                     FROM projects
                     ORDER BY created_at ASC, id ASC",
                    &[],
                )
                .map_err(|e| CliError::Other(format!("projects resource query failed: {e}")))?;

            if let Some(contains) = query
                .get("contains")
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
            {
                rows.retain(|row| {
                    let slug = row
                        .get_named::<String>("slug")
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let human_key = row
                        .get_named::<String>("human_key")
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    slug.contains(&contains) || human_key.contains(&contains)
                });
            }

            if let Some(format) = query.get("format") {
                let format = format.trim();
                if !format.is_empty() && !format.eq_ignore_ascii_case("json") {
                    return Err(CliError::InvalidArgument(format!(
                        "Unsupported projects format '{format}'. Supported values: json"
                    )));
                }
            }

            let limit = query
                .get("limit")
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|raw| {
                    raw.parse::<usize>().map_err(|_| {
                        CliError::InvalidArgument(format!(
                            "Invalid limit '{raw}'. Expected a non-negative integer."
                        ))
                    })
                })
                .transpose()?;
            if let Some(limit) = limit {
                rows.truncate(limit);
            }

            Ok((
                NavigateResult::Generic {
                    resource_type: "projects".to_string(),
                    data: serde_json::Value::Array(
                        rows.into_iter()
                            .map(|row| {
                                serde_json::json!({
                                    "id": row.get_named::<i64>("id").unwrap_or(0),
                                    "slug": row.get_named::<String>("slug").unwrap_or_default(),
                                    "human_key": row.get_named::<String>("human_key").unwrap_or_default(),
                                    "created_at": row
                                        .get_named::<i64>("created_at")
                                        .ok()
                                        .map(mcp_agent_mail_db::micros_to_iso),
                                })
                            })
                            .collect(),
                    ),
                },
                None,
            ))
        }
        ["inbox", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource("inbox", project_scope, move |ctx| async move {
                mcp_agent_mail_tools::inbox(&ctx, agent).await
            })
        }
        ["mailbox", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource("mailbox", project_scope, move |ctx| async move {
                mcp_agent_mail_tools::mailbox(&ctx, agent).await
            })
        }
        ["mailbox-with-commits", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource(
                "mailbox-with-commits",
                project_scope,
                move |ctx| async move { mcp_agent_mail_tools::mailbox_with_commits(&ctx, agent).await },
            )
        }
        ["outbox", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource("outbox", project_scope, move |ctx| async move {
                mcp_agent_mail_tools::outbox(&ctx, agent).await
            })
        }
        ["views", "urgent-unread", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource(
                "views/urgent-unread",
                project_scope,
                move |ctx| async move { mcp_agent_mail_tools::views_urgent_unread(&ctx, agent).await },
            )
        }
        ["views", "ack-required", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource("views/ack-required", project_scope, move |ctx| async move {
                mcp_agent_mail_tools::views_ack_required(&ctx, agent).await
            })
        }
        ["views", "acks-stale", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource("views/acks-stale", project_scope, move |ctx| async move {
                mcp_agent_mail_tools::views_acks_stale(&ctx, agent).await
            })
        }
        ["views", "ack-overdue", agent_name] => {
            let effective_query = navigate_query_with_default_project(&query, default_project);
            let project_scope = effective_query.get("project").cloned();
            let agent = navigate_param_with_query(agent_name, &effective_query);
            navigate_async_resource("views/ack-overdue", project_scope, move |ctx| async move {
                mcp_agent_mail_tools::views_ack_overdue(&ctx, agent).await
            })
        }
        ["file_reservations", project_key] | ["reservations", project_key] => {
            let conn = crate::open_db_sync_robot()?;
            let (resolved_project_id, resolved_project_slug) =
                resolve_project_sync(&conn, project_key)?;
            build_navigate_file_reservations(
                &conn,
                resolved_project_id,
                &resolved_project_slug,
                &query,
            )
        }
        ["file_reservations"] | ["reservations"] => {
            let Some(project_key) = query
                .get("project")
                .cloned()
                .or_else(|| default_project.map(str::to_string))
                .filter(|value| !value.trim().is_empty())
            else {
                return Err(CliError::InvalidArgument(
                    "project query parameter is required".to_string(),
                ));
            };
            let conn = crate::open_db_sync_robot()?;
            let (resolved_project_id, resolved_project_slug) =
                resolve_project_sync(&conn, &project_key)?;
            build_navigate_file_reservations(
                &conn,
                resolved_project_id,
                &resolved_project_slug,
                &query,
            )
        }
        _ => Err(CliError::InvalidArgument(format!(
            "unsupported canonical navigate resource URI pattern: {uri}"
        ))),
    }
}

fn build_navigate_file_reservations(
    conn: &DbConn,
    project_id: i64,
    project_slug: &str,
    query: &std::collections::HashMap<String, String>,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let active_only = query
        .get("active_only")
        .is_none_or(|value| parse_resource_bool(Some(value)));
    let has_release_ledger = has_file_reservation_release_ledger(conn);
    let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
    let active_reservation_join =
        active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
    let released_ts_sql = reservation_released_ts_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );

    let (sql, params) = if active_only {
        (
            format!(
                "SELECT fr.path_pattern, a.name, fr.\"exclusive\", fr.expires_ts, {released_ts_sql} AS released_ts, fr.created_ts
                 FROM file_reservations fr{active_reservation_join}
                 JOIN agents a ON a.id = fr.agent_id
                 WHERE fr.project_id = ? AND ({active_reservation_predicate})
                 ORDER BY fr.created_ts DESC LIMIT 50"
            ),
            vec![Value::BigInt(project_id)],
        )
    } else {
        (
            format!(
                "SELECT fr.path_pattern, a.name, fr.\"exclusive\", fr.expires_ts, {released_ts_sql} AS released_ts, fr.created_ts
                 FROM file_reservations fr{active_reservation_join}
                 JOIN agents a ON a.id = fr.agent_id
                 WHERE fr.project_id = ?
                 ORDER BY fr.created_ts DESC LIMIT 50"
            ),
            vec![Value::BigInt(project_id)],
        )
    };
    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| CliError::Other(format!("navigate reservations query failed: {e}")))?;

    let reservations: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let released_ts = r
                .get_as::<sqlmodel_core::Value>(4)
                .ok()
                .and_then(|value| value_to_micros(&value));
            serde_json::json!({
                "path": r.get_as::<String>(0).unwrap_or_default(),
                "agent": r.get_as::<String>(1).unwrap_or_default(),
                "exclusive": r.get_as::<i64>(2).unwrap_or(0) == 1,
                "expires_ts": r.get_as::<i64>(3).unwrap_or(0),
                "released": reservation_is_released(released_ts),
            })
        })
        .collect();

    Ok((
        NavigateResult::Generic {
            resource_type: "file_reservations".to_string(),
            data: serde_json::json!({ "reservations": reservations }),
        },
        Some(project_slug.to_string()),
    ))
}

fn emit_robot_output(out: &str) {
    #[cfg(test)]
    {
        let _ = out;
    }
    #[cfg(not(test))]
    {
        println!("{out}");
    }
}

fn build_navigate_config_environment() -> (NavigateResult, Option<String>) {
    let config = mcp_agent_mail_core::Config::get();
    (
        NavigateResult::Generic {
            resource_type: "config/environment".to_string(),
            data: serde_json::json!({
                "environment": config.app_environment.to_string(),
                "database_url": redact_navigate_database_url(&config.database_url),
                "http": {
                    "host": config.http_host.clone(),
                    "port": config.http_port,
                    "path": config.http_path.clone(),
                },
            }),
        },
        None,
    )
}

fn build_navigate_identity(
    project_ref: &str,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let resolved_human_key = {
        let path = PathBuf::from(project_ref);
        if path.is_absolute() {
            path
        } else {
            let cwd = std::env::current_dir()
                .map_err(|e| CliError::Other(format!("cannot get CWD for identity: {e}")))?;
            let base = navigate_workspace_root_from(&cwd).unwrap_or(cwd);
            base.join(path)
        }
    };
    let resolved = resolved_human_key.to_string_lossy().to_string();
    let identity = mcp_agent_mail_core::resolve_project_identity(&resolved);
    let data = serde_json::to_value(identity)
        .map_err(|e| CliError::Other(format!("identity serialization failed: {e}")))?;
    Ok((
        NavigateResult::Generic {
            resource_type: "identity".to_string(),
            data,
        },
        None,
    ))
}

fn build_navigate_tooling(
    parts_ref: &[&str],
    query: &std::collections::HashMap<String, String>,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let (resource_type, data) = match parts_ref {
        ["tooling", "metrics"] => {
            let config = mcp_agent_mail_core::Config::get();
            let mut tools: Vec<serde_json::Value> =
                mcp_agent_mail_tools::tool_metrics_snapshot_full()
                    .into_iter()
                    .map(|entry| {
                        serde_json::json!({
                            "name": entry.name,
                            "calls": entry.calls,
                            "errors": entry.errors,
                            "cluster": entry.cluster,
                            "capabilities": entry.capabilities,
                            "complexity": entry.complexity,
                        })
                    })
                    .collect();
            if config.tool_filter.enabled {
                tools.retain(|entry| {
                    entry
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|name| {
                            mcp_agent_mail_tools::tool_cluster(name)
                                .is_none_or(|cluster| config.should_expose_tool(name, cluster))
                        })
                });
            }
            (
                "tooling/metrics".to_string(),
                serde_json::json!({
                    "generated_at": serde_json::Value::Null,
                    "health_level": mcp_agent_mail_core::compute_health_level().to_string(),
                    "tools": tools,
                }),
            )
        }
        ["tooling", "metrics_core"] => (
            "tooling/metrics_core".to_string(),
            serde_json::json!({
                "generated_at": serde_json::Value::Null,
                "health_level": mcp_agent_mail_core::compute_health_level().to_string(),
                "metrics": mcp_agent_mail_core::global_metrics().snapshot(),
                "lock_contention": mcp_agent_mail_core::lock_contention_snapshot(),
            }),
        ),
        ["tooling", "diagnostics"] => {
            let tools_detail: Vec<serde_json::Value> =
                mcp_agent_mail_tools::tool_metrics_snapshot()
                    .into_iter()
                    .filter_map(|entry| serde_json::to_value(entry).ok())
                    .collect();
            let slow: Vec<serde_json::Value> = mcp_agent_mail_tools::slow_tools()
                .into_iter()
                .filter_map(|entry| serde_json::to_value(entry).ok())
                .collect();
            let report = mcp_agent_mail_core::DiagnosticReport::build(tools_detail, slow);
            let data = serde_json::from_str::<serde_json::Value>(&report.to_json())
                .map_err(|e| CliError::Other(format!("diagnostics serialization failed: {e}")))?;
            ("tooling/diagnostics".to_string(), data)
        }
        ["tooling", "locks"] => {
            let config = mcp_agent_mail_core::Config::get();
            let lock_info =
                mcp_agent_mail_storage::collect_lock_status(&config).unwrap_or_else(|_err| {
                    serde_json::json!({
                        "archive_root": "",
                        "exists": false,
                        "locks": [],
                    })
                });
            let data = build_navigate_tooling_locks_data(lock_info);
            ("tooling/locks".to_string(), data)
        }
        ["tooling", "capabilities", agent_name] => (
            "tooling/capabilities".to_string(),
            serde_json::json!({
                "agent": *agent_name,
                "project": query.get("project").cloned().unwrap_or_default(),
                "capabilities": mcp_agent_mail_tools::identity::DEFAULT_AGENT_CAPABILITIES,
                "generated_at": serde_json::Value::Null,
            }),
        ),
        ["tooling", "recent", window_seconds] => {
            let parsed_window = window_seconds.parse::<u64>().unwrap_or(0);
            (
                "tooling/recent".to_string(),
                serde_json::json!({
                    "generated_at": serde_json::Value::Null,
                    "window_seconds": parsed_window,
                    "count": 0,
                    "entries": [],
                }),
            )
        }
        _ => {
            return Err(CliError::InvalidArgument(format!(
                "unsupported tooling resource URI pattern: resource://{}",
                parts_ref.join("/")
            )));
        }
    };

    Ok((
        NavigateResult::Generic {
            resource_type,
            data,
        },
        None,
    ))
}

#[derive(Clone, Copy)]
enum NavigateViewKind {
    UrgentUnread,
    AckRequired,
    AcksStale,
    AckOverdue,
}

impl NavigateViewKind {
    const fn resource_type(self) -> &'static str {
        match self {
            Self::UrgentUnread => "views/urgent-unread",
            Self::AckRequired => "views/ack-required",
            Self::AcksStale => "views/acks-stale",
            Self::AckOverdue => "views/ack-overdue",
        }
    }
}

fn build_navigate_view(
    conn: &DbConn,
    project_id: i64,
    project_slug: &str,
    agent_name: &str,
    query: &std::collections::HashMap<String, String>,
    kind: NavigateViewKind,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let limit = parse_resource_limit(query.get("limit"), 20);
    let project_human_key =
        project_human_key_sync(conn, project_id)?.unwrap_or_else(|| project_slug.to_string());
    let Some((agent_id, resolved_agent_name)) =
        resolve_agent_id(conn, project_id, Some(agent_name))?
    else {
        let empty = match kind {
            NavigateViewKind::AcksStale => serde_json::json!({
                "project": project_human_key,
                "agent": agent_name,
                "ttl_seconds": query
                    .get("ttl_seconds")
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(3600),
                "count": 0,
                "messages": [],
            }),
            NavigateViewKind::AckOverdue => serde_json::json!({
                "project": project_human_key,
                "agent": agent_name,
                "count": 0,
                "messages": [],
            }),
            _ => serde_json::json!({
                "project": project_human_key,
                "agent": agent_name,
                "count": 0,
                "messages": [],
            }),
        };
        return Ok((
            NavigateResult::Generic {
                resource_type: kind.resource_type().to_string(),
                data: empty,
            },
            Some(project_slug.to_string()),
        ));
    };

    let data = match kind {
        NavigateViewKind::UrgentUnread => {
            let rows = conn
                .query_sync(
                    "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject,
                            m.importance, m.ack_required, m.created_ts, m.attachments, mr.kind,
                            a_sender.name AS sender_name
                     FROM message_recipients mr
                     JOIN messages m ON m.id = mr.message_id
                     JOIN agents a_sender ON a_sender.id = m.sender_id
                     WHERE mr.agent_id = ? AND m.project_id = ?
                       AND mr.read_ts IS NULL
                       AND m.importance IN ('urgent', 'high')
                     ORDER BY m.created_ts DESC
                     LIMIT ?",
                    &[
                        Value::BigInt(agent_id),
                        Value::BigInt(project_id),
                        Value::BigInt(limit.try_into().unwrap_or(i64::MAX)),
                    ],
                )
                .map_err(|e| {
                    CliError::Other(format!("navigate urgent-unread query failed: {e}"))
                })?;

            let messages: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    let attachments = row
                        .get_named::<String>("attachments")
                        .ok()
                        .map(|raw| parse_attachment_values(Some(raw.as_str())))
                        .unwrap_or_default();
                    let thread_id = row
                        .get_named::<String>("thread_id")
                        .ok()
                        .filter(|value| !value.is_empty());
                    serde_json::json!({
                        "id": row.get_named::<i64>("id").unwrap_or(0),
                        "project_id": row.get_named::<i64>("project_id").unwrap_or(0),
                        "sender_id": row.get_named::<i64>("sender_id").unwrap_or(0),
                        "thread_id": thread_id,
                        "subject": row.get_named::<String>("subject").unwrap_or_default(),
                        "importance": row.get_named::<String>("importance").unwrap_or_default(),
                        "ack_required": row.get_named::<i64>("ack_required").unwrap_or(0) == 1,
                        "created_ts": mcp_agent_mail_db::micros_to_iso(
                            row.get_named::<i64>("created_ts").unwrap_or(0),
                        ),
                        "attachments": attachments,
                        "from": row.get_named::<String>("sender_name").unwrap_or_default(),
                        "kind": row.get_named::<String>("kind").unwrap_or_default(),
                    })
                })
                .collect();

            serde_json::json!({
                "project": project_human_key,
                "agent": resolved_agent_name,
                "count": messages.len(),
                "messages": messages,
            })
        }
        NavigateViewKind::AckRequired
        | NavigateViewKind::AcksStale
        | NavigateViewKind::AckOverdue => {
            let rows = conn
                .query_sync(
                    "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject,
                            m.importance, m.created_ts, m.attachments, mr.kind, mr.read_ts
                     FROM message_recipients mr
                     JOIN messages m ON m.id = mr.message_id
                     WHERE mr.agent_id = ? AND m.project_id = ?
                       AND m.ack_required = 1 AND mr.ack_ts IS NULL
                     ORDER BY m.created_ts DESC
                     LIMIT ?",
                    &[
                        Value::BigInt(agent_id),
                        Value::BigInt(project_id),
                        Value::BigInt(limit.saturating_mul(5).try_into().unwrap_or(i64::MAX)),
                    ],
                )
                .map_err(|e| CliError::Other(format!("navigate ack view query failed: {e}")))?;

            let now_us = mcp_agent_mail_db::now_micros();
            let ttl_seconds = query
                .get("ttl_seconds")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(3600);
            let ttl_minutes = query
                .get("ttl_minutes")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(60);
            let ack_overdue_cutoff = now_us.saturating_sub(
                i64::try_from(ttl_minutes.max(1))
                    .unwrap_or(i64::MAX)
                    .saturating_mul(60)
                    .saturating_mul(1_000_000),
            );
            let stale_ttl_us = i64::try_from(ttl_seconds)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000_000);

            let mut messages = Vec::new();
            for row in &rows {
                let created_ts = row.get_named::<i64>("created_ts").unwrap_or(0);
                let include = match kind {
                    NavigateViewKind::AckRequired => true,
                    NavigateViewKind::AckOverdue => created_ts <= ack_overdue_cutoff,
                    NavigateViewKind::AcksStale => {
                        now_us.saturating_sub(created_ts) >= stale_ttl_us
                    }
                    NavigateViewKind::UrgentUnread => false,
                };
                if !include {
                    continue;
                }

                let attachments = row
                    .get_named::<String>("attachments")
                    .ok()
                    .map(|raw| parse_attachment_values(Some(raw.as_str())))
                    .unwrap_or_default();
                let thread_id = row
                    .get_named::<String>("thread_id")
                    .ok()
                    .filter(|value| !value.is_empty());
                let read_ts = row.get_named::<i64>("read_ts").ok();
                let age_seconds = now_us.saturating_sub(created_ts) / 1_000_000;

                let message = match kind {
                    NavigateViewKind::AcksStale => serde_json::json!({
                        "id": row.get_named::<i64>("id").unwrap_or(0),
                        "project_id": row.get_named::<i64>("project_id").unwrap_or(0),
                        "sender_id": row.get_named::<i64>("sender_id").unwrap_or(0),
                        "thread_id": thread_id,
                        "subject": row.get_named::<String>("subject").unwrap_or_default(),
                        "importance": row.get_named::<String>("importance").unwrap_or_default(),
                        "ack_required": true,
                        "created_ts": mcp_agent_mail_db::micros_to_iso(created_ts),
                        "attachments": attachments,
                        "kind": row.get_named::<String>("kind").unwrap_or_default(),
                        "read_at": read_ts.filter(|ts| *ts > 0).map(mcp_agent_mail_db::micros_to_iso),
                        "age_seconds": age_seconds,
                    }),
                    _ => serde_json::json!({
                        "id": row.get_named::<i64>("id").unwrap_or(0),
                        "project_id": row.get_named::<i64>("project_id").unwrap_or(0),
                        "sender_id": row.get_named::<i64>("sender_id").unwrap_or(0),
                        "thread_id": thread_id,
                        "subject": row.get_named::<String>("subject").unwrap_or_default(),
                        "importance": row.get_named::<String>("importance").unwrap_or_default(),
                        "ack_required": true,
                        "created_ts": mcp_agent_mail_db::micros_to_iso(created_ts),
                        "attachments": attachments,
                        "kind": row.get_named::<String>("kind").unwrap_or_default(),
                    }),
                };
                messages.push(message);
                if messages.len() >= limit {
                    break;
                }
            }

            match kind {
                NavigateViewKind::AcksStale => serde_json::json!({
                    "project": project_human_key,
                    "agent": resolved_agent_name,
                    "ttl_seconds": ttl_seconds,
                    "count": messages.len(),
                    "messages": messages,
                }),
                NavigateViewKind::AckOverdue => serde_json::json!({
                    "project": project_human_key,
                    "agent": resolved_agent_name,
                    "count": messages.len(),
                    "messages": messages,
                }),
                NavigateViewKind::AckRequired => serde_json::json!({
                    "project": project_human_key,
                    "agent": resolved_agent_name,
                    "count": messages.len(),
                    "messages": messages,
                }),
                NavigateViewKind::UrgentUnread => unreachable!(),
            }
        }
    };

    Ok((
        NavigateResult::Generic {
            resource_type: kind.resource_type().to_string(),
            data,
        },
        Some(project_slug.to_string()),
    ))
}

fn build_navigate(
    conn: &DbConn,
    uri: &str,
    project_id: i64,
    project_slug: &str,
    _agent: Option<(i64, String)>,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let (path, query) = split_resource_path_and_query(uri)?;
    let (effective_project_id, effective_project_slug) = query
        .get("project")
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || Ok((project_id, project_slug.to_string())),
            |project| resolve_project_sync(conn, project),
        )?;

    let parts: Vec<String> = path
        .split('/')
        .map(percent_decode_resource_path_component)
        .collect();
    let parts_ref: Vec<&str> = parts.iter().map(String::as_str).collect();

    match parts_ref.as_slice() {
        ["config", "environment"] => Ok(build_navigate_config_environment()),
        ["identity", project_ref] => build_navigate_identity(project_ref),
        ["projects"] => {
            let projects = build_projects(conn)?;
            Ok((NavigateResult::Projects { projects }, None))
        }
        ["project", project_key] => {
            let (resolved_project_id, resolved_project_slug) =
                resolve_project_sync(conn, project_key)?;
            let row = conn
                .query_sync(
                    "SELECT id, slug, human_key, created_at FROM projects WHERE id = ?",
                    &[Value::BigInt(resolved_project_id)],
                )
                .map_err(|e| CliError::Other(format!("project query: {e}")))?
                .into_iter()
                .next()
                .ok_or_else(|| CliError::Other(format!("project not found: {project_key}")))?;

            let data = serde_json::json!({
                "id": row.get_named::<i64>("id").unwrap_or(0),
                "slug": row.get_named::<String>("slug").unwrap_or_default(),
                "path": row.get_named::<String>("human_key").unwrap_or_default(),
                "created_at": mcp_agent_mail_db::micros_to_iso(row.get_named::<i64>("created_at").unwrap_or(0)),
            });
            Ok((
                NavigateResult::Generic {
                    resource_type: "project".to_string(),
                    data,
                },
                Some(resolved_project_slug),
            ))
        }
        ["agents", project_key] => {
            let (resolved_project_id, resolved_project_slug) =
                resolve_project_sync(conn, project_key)?;
            let agents = build_agents(conn, resolved_project_id, false, None)?;
            Ok((
                NavigateResult::Agents { agents },
                Some(resolved_project_slug),
            ))
        }
        ["agents"] => {
            let agents = build_agents(conn, effective_project_id, false, None)?;
            Ok((
                NavigateResult::Agents { agents },
                Some(effective_project_slug.clone()),
            ))
        }
        ["inbox", agent_name] => {
            // Resolve agent and get inbox using simplified direct query
            let agent_opt = resolve_agent_id(conn, effective_project_id, Some(agent_name))?;
            if let Some((agent_id, name)) = agent_opt {
                let limit = parse_resource_limit(query.get("limit"), 50);
                let include_bodies = parse_resource_bool(query.get("include_bodies"));
                let urgent_only = parse_resource_bool(query.get("urgent_only"));
                let result = build_inbox(
                    conn,
                    effective_project_id,
                    &effective_project_slug,
                    agent_id,
                    &name,
                    urgent_only,
                    false,
                    true,
                    false,
                    limit,
                    include_bodies,
                )?;
                Ok((
                    NavigateResult::Inbox {
                        entries: result.entries,
                    },
                    Some(effective_project_slug.clone()),
                ))
            } else {
                Ok((
                    NavigateResult::Inbox { entries: vec![] },
                    Some(effective_project_slug.clone()),
                ))
            }
        }
        ["message", id_str] => {
            let msg_id: i64 = id_str
                .parse()
                .map_err(|_| CliError::InvalidArgument(format!("invalid message id: {id_str}")))?;
            let message = build_message(conn, effective_project_id, msg_id)?;
            Ok((
                NavigateResult::Message { message },
                Some(effective_project_slug.clone()),
            ))
        }
        ["thread", thread_id] => {
            let limit = parse_resource_limit(query.get("limit"), 100);
            let include_bodies = parse_resource_bool(query.get("include_bodies"));
            let since = query
                .get("since_ts")
                .or_else(|| query.get("since"))
                .map(String::as_str);
            let thread = build_thread(
                conn,
                effective_project_id,
                thread_id,
                Some(limit),
                since,
                include_bodies,
            )?;
            Ok((
                NavigateResult::Thread { thread },
                Some(effective_project_slug.clone()),
            ))
        }
        ["views", "urgent-unread", agent_name] => build_navigate_view(
            conn,
            effective_project_id,
            &effective_project_slug,
            agent_name,
            &query,
            NavigateViewKind::UrgentUnread,
        ),
        ["views", "ack-required", agent_name] => build_navigate_view(
            conn,
            effective_project_id,
            &effective_project_slug,
            agent_name,
            &query,
            NavigateViewKind::AckRequired,
        ),
        ["views", "acks-stale", agent_name] => build_navigate_view(
            conn,
            effective_project_id,
            &effective_project_slug,
            agent_name,
            &query,
            NavigateViewKind::AcksStale,
        ),
        ["views", "ack-overdue", agent_name] => build_navigate_view(
            conn,
            effective_project_id,
            &effective_project_slug,
            agent_name,
            &query,
            NavigateViewKind::AckOverdue,
        ),
        ["file_reservations"] | ["reservations"] => build_navigate_file_reservations(
            conn,
            effective_project_id,
            &effective_project_slug,
            &query,
        ),
        ["file_reservations", project_key] | ["reservations", project_key] => {
            let (resolved_project_id, resolved_project_slug) =
                resolve_project_sync(conn, project_key)?;
            build_navigate_file_reservations(
                conn,
                resolved_project_id,
                &resolved_project_slug,
                &query,
            )
        }
        ["mailbox", agent_name] => {
            let agent_opt = resolve_agent_id(conn, effective_project_id, Some(agent_name))?;
            if let Some((agent_id, name)) = agent_opt {
                let limit = parse_resource_limit(query.get("limit"), 50);
                let include_bodies = parse_resource_bool(query.get("include_bodies"));
                let result = build_inbox(
                    conn,
                    effective_project_id,
                    &effective_project_slug,
                    agent_id,
                    &name,
                    false,
                    false,
                    false,
                    true,
                    limit,
                    include_bodies,
                )?;
                Ok((
                    NavigateResult::Inbox {
                        entries: result.entries,
                    },
                    Some(effective_project_slug.clone()),
                ))
            } else {
                Ok((
                    NavigateResult::Inbox { entries: vec![] },
                    Some(effective_project_slug.clone()),
                ))
            }
        }
        ["outbox", agent_name] => {
            let agent_opt = resolve_agent_id(conn, effective_project_id, Some(agent_name))?;
            if let Some((agent_id, _name)) = agent_opt {
                let limit = parse_resource_limit(query.get("limit"), 50);
                let include_bodies = parse_resource_bool(query.get("include_bodies"));
                let entries = build_outbox_entries(
                    conn,
                    effective_project_id,
                    agent_id,
                    limit,
                    include_bodies,
                )?;
                Ok((
                    NavigateResult::Inbox { entries },
                    Some(effective_project_slug.clone()),
                ))
            } else {
                Ok((
                    NavigateResult::Inbox { entries: vec![] },
                    Some(effective_project_slug.clone()),
                ))
            }
        }
        ["tooling", ..] => build_navigate_tooling(&parts_ref, &query),
        _ => Err(CliError::InvalidArgument(format!(
            "unsupported resource URI pattern: {uri}\n\
             Supported patterns:\n\
             - resource://config/environment\n\
             - resource://identity/<project>\n\
             - resource://projects\n\
             - resource://project/<slug>\n\
             - resource://agents/<slug>\n\
             - resource://agents?project=<slug>\n\
             - resource://inbox/<agent>\n\
             - resource://message/<id>\n\
             - resource://thread/<id>\n\
             - resource://views/urgent-unread/<agent>\n\
             - resource://views/ack-required/<agent>\n\
             - resource://views/acks-stale/<agent>\n\
             - resource://views/ack-overdue/<agent>\n\
             - resource://file_reservations/<slug>\n\
             - resource://file_reservations?project=<slug>\n\
             - resource://reservations/<slug>\n\
             - resource://reservations?project=<slug>\n\
             - resource://tooling/metrics\n\
             - resource://tooling/metrics_core\n\
             - resource://tooling/diagnostics\n\
             - resource://tooling/locks\n\
             - resource://tooling/capabilities/<agent>\n\
             - resource://tooling/recent/<window_seconds>\n\
             - resource://mailbox/<agent>\n\
             - resource://outbox/<agent>"
        ))),
    }
}

// ── Dispatch ────────────────────────────────────────────────────────────────

/// Execute a robot subcommand and print formatted output.
pub fn handle_robot(args: RobotArgs) -> Result<(), CliError> {
    let format = OutputFormat::resolve(args.format, args.command.is_prose());
    let cmd_name = args.command.name();

    let out = match args.command {
        RobotSubcommand::Status => {
            let scope = resolve_robot_scope(args.project.as_deref(), args.agent.as_deref())?;

            let agent_name = scope.agent.as_ref().map(|(_, n)| n.clone());
            let agent = scope.agent.clone();
            let (data, actions) =
                build_status(scope.conn(), scope.project_id, &scope.project_slug, agent)?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(scope.project_slug);
            env._meta.agent = agent_name.clone();
            if agent_name.is_none() {
                env = env.with_alert(
                    "info",
                    "Agent not detected — inbox/reservation sections omitted. Use --agent to specify.",
                    Some("am robot status --agent <NAME>".to_string()),
                );
            }
            for a in actions {
                env = env.with_action(&a);
            }
            let anomalies = env.data.anomalies.clone();
            env = append_status_anomaly_alerts(env, &anomalies);
            format_output(&env, format)?
        }
        RobotSubcommand::Inbox {
            urgent,
            ack_overdue,
            unread,
            all,
            limit,
            include_bodies,
        } => {
            let scope = resolve_robot_scope(args.project.as_deref(), args.agent.as_deref())?;
            let (agent_id, agent_name_str) = scope.agent.clone().ok_or_else(|| {
                CliError::InvalidArgument(
                    "agent required for inbox — use --agent or set AGENT_MAIL_AGENT/AGENT_NAME"
                        .to_string(),
                )
            })?;

            let result = build_inbox(
                scope.conn(),
                scope.project_id,
                &scope.project_slug,
                agent_id,
                &agent_name_str,
                urgent,
                ack_overdue,
                unread || (!urgent && !ack_overdue && !all),
                all,
                limit.unwrap_or(20),
                include_bodies,
            )?;

            #[derive(Serialize)]
            struct InboxData {
                count: usize,
                inbox: Vec<InboxEntry>,
            }

            let count = result.entries.len();
            let mut env = RobotEnvelope::new(
                cmd_name,
                format,
                InboxData {
                    count,
                    inbox: result.entries,
                },
            );
            env._meta.project = Some(scope.project_slug);
            env._meta.agent = Some(agent_name_str);
            for (severity, headline, action) in result.alerts {
                env = env.with_alert(severity, headline, action);
            }
            for a in result.actions {
                env = env.with_action(&a);
            }
            format_output(&env, format)?
        }
        RobotSubcommand::Thread { id, limit, since } => {
            let conn = crate::open_db_sync_robot()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;

            // For thread command, bodies included in md/json, excluded in toon
            let include_bodies = format != OutputFormat::Toon;
            let data = build_thread(
                &conn,
                project_id,
                &id,
                limit,
                since.as_deref(),
                include_bodies,
            )?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(project_slug);
            format_output_md(&env, format)?
        }
        RobotSubcommand::Message { id } => {
            let conn = crate::open_db_sync_robot()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let data = build_message(&conn, project_id, id)?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(project_slug);
            format_output_md(&env, format)?
        }
        RobotSubcommand::Search {
            query,
            kind,
            importance,
            since,
        } => {
            // Search executes via the async search service and its full DB pool, so it
            // cannot safely rely on the reduced best-effort robot fallback path.
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let data = build_search(
                &conn,
                project_id,
                &query,
                kind.as_deref(),
                importance.as_deref(),
                since.as_deref(),
                20,
            )?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(project_slug);
            format_output(&env, format)?
        }
        RobotSubcommand::Reservations {
            agent: agent_override,
            all,
            conflicts,
            expiring,
        } => {
            let agent_flag = agent_override.as_deref().or(args.agent.as_deref());
            let scope = resolve_robot_scope(args.project.as_deref(), agent_flag)?;
            let agent = scope.agent.clone();
            let (data, actions) = build_reservations(
                scope.conn(),
                scope.project_id,
                &scope.project_slug,
                agent,
                all,
                conflicts,
                expiring,
            )?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(scope.project_slug);
            for a in actions {
                env = env.with_action(&a);
            }
            format_output(&env, format)?
        }
        RobotSubcommand::Metrics => {
            let snapshot = mcp_agent_mail_tools::tool_metrics_snapshot();

            let total_calls: u64 = snapshot.iter().map(|e| e.calls).sum();
            let total_errors: u64 = snapshot.iter().map(|e| e.errors).sum();
            let error_rate = if total_calls > 0 {
                (total_errors as f64 / total_calls as f64) * 100.0
            } else {
                0.0
            };
            let avg_latency = if !snapshot.is_empty() {
                let sum: f64 = snapshot
                    .iter()
                    .filter_map(|e| e.latency.as_ref().map(|l| l.avg_ms * e.calls as f64))
                    .sum();
                if total_calls > 0 {
                    sum / total_calls as f64
                } else {
                    0.0
                }
            } else {
                0.0
            };

            let tools: Vec<MetricEntry> = snapshot
                .iter()
                .map(|e| {
                    let error_pct = if e.calls > 0 {
                        (e.errors as f64 / e.calls as f64) * 100.0
                    } else {
                        0.0
                    };
                    MetricEntry {
                        name: e.name.clone(),
                        calls: e.calls,
                        errors: e.errors,
                        error_pct,
                        avg_ms: e.latency.as_ref().map_or(0.0, |l| l.avg_ms),
                        p95_ms: e.latency.as_ref().map_or(0.0, |l| l.p95_ms),
                        p99_ms: e.latency.as_ref().map_or(0.0, |l| l.p99_ms),
                    }
                })
                .collect();

            #[derive(Serialize)]
            struct MetricsData {
                total_calls: u64,
                total_errors: u64,
                error_rate_pct: f64,
                avg_latency_ms: f64,
                tools: Vec<MetricEntry>,
            }

            let mut env = RobotEnvelope::new(
                cmd_name,
                format,
                MetricsData {
                    total_calls,
                    total_errors,
                    error_rate_pct: (error_rate * 100.0).round() / 100.0,
                    avg_latency_ms: (avg_latency * 100.0).round() / 100.0,
                    tools,
                },
            );

            // Generate alerts for problematic tools
            for e in &snapshot {
                let error_pct = if e.calls > 0 {
                    (e.errors as f64 / e.calls as f64) * 100.0
                } else {
                    0.0
                };
                if error_pct > 50.0 {
                    env = env.with_alert(
                        "error",
                        format!(
                            "{} has {:.1}% error rate ({}/{})",
                            e.name, error_pct, e.errors, e.calls
                        ),
                        None,
                    );
                } else if error_pct > 10.0 {
                    env = env.with_alert(
                        "warn",
                        format!(
                            "{} has {:.1}% error rate ({}/{})",
                            e.name, error_pct, e.errors, e.calls
                        ),
                        None,
                    );
                }
                if let Some(lat) = &e.latency
                    && lat.avg_ms > 2000.0
                {
                    env = env.with_alert(
                        "warn",
                        format!("{} avg latency {:.0}ms (>2s)", e.name, lat.avg_ms),
                        None,
                    );
                }
            }
            if error_rate > 5.0 {
                env = env.with_alert(
                    "error",
                    format!("Overall error rate {error_rate:.1}% (>{} threshold)", 5),
                    None,
                );
            }

            format_output(&env, format)?
        }
        RobotSubcommand::Health => {
            let mut probes: Vec<HealthProbe> = Vec::new();

            // 1. DB connectivity probe
            let db_start = std::time::Instant::now();
            let db_ok = match crate::open_db_sync() {
                Ok(conn) => {
                    // Verify canonical DB reachability with a lightweight query.
                    conn.query_sync("SELECT 1", &[]).is_ok()
                }
                Err(_) => false,
            };
            let db_ms = db_start.elapsed().as_secs_f64() * 1000.0;
            probes.push(HealthProbe {
                name: "db_connectivity".into(),
                status: if db_ok { "ok" } else { "fail" }.into(),
                latency_ms: (db_ms * 100.0).round() / 100.0,
                detail: if db_ok {
                    "SQLite connection healthy".into()
                } else {
                    "Cannot connect to database".into()
                },
            });

            // 2. Circuit breaker status
            let db_health = mcp_agent_mail_db::db_health_status();
            let open_circuits: Vec<String> = db_health
                .circuits
                .iter()
                .filter(|c| c.state != "closed")
                .map(|c| format!("{}={} ({} failures)", c.subsystem, c.state, c.failures))
                .collect();
            let circuits_ok = open_circuits.is_empty();
            let circuit_detail = if circuits_ok {
                "All circuits closed".to_string()
            } else {
                format!("Active circuit faults: {}", open_circuits.join(", "))
            };
            probes.push(HealthProbe {
                name: "circuit_breakers".into(),
                status: if circuits_ok { "ok" } else { "degraded" }.into(),
                latency_ms: 0.0,
                detail: circuit_detail,
            });

            // Per-subsystem circuit details
            let mut circuit_entries: Vec<CircuitEntry> = Vec::new();
            for c in &db_health.circuits {
                circuit_entries.push(CircuitEntry {
                    subsystem: c.subsystem.clone(),
                    state: c.state.clone(),
                    failures: c.failures,
                    threshold: c.threshold,
                });
            }

            // 3. Health level (backpressure)
            let (health_level, signals) =
                mcp_agent_mail_core::backpressure::compute_health_level_with_signals();
            let health_str = format!("{health_level:?}").to_lowercase();
            let backpressure_degraded = health_str != "green";
            let backpressure_unhealthy = health_str == "red";
            probes.push(HealthProbe {
                name: "backpressure".into(),
                status: health_str.clone(),
                latency_ms: 0.0,
                detail: format!(
                    "pool={}% wbq={}% commit={}%",
                    signals.pool_utilization_pct, signals.wbq_depth_pct, signals.commit_depth_pct
                ),
            });

            // 4. Integrity metrics
            let integrity = mcp_agent_mail_db::integrity_metrics();
            let (integrity_probe, integrity_ok) = summarize_integrity_probe(&integrity);
            probes.push(integrity_probe);

            // 5. Disk space probe
            let config = mcp_agent_mail_core::Config::from_env();
            let disk = mcp_agent_mail_core::disk::sample_disk(&config);
            let disk_probe_failed = !disk.errors.is_empty();
            let disk_status = if disk_probe_failed {
                "degraded"
            } else {
                disk.pressure.label()
            };
            let free_mb = disk
                .effective_free_bytes
                .map(|b| b / (1024 * 1024))
                .unwrap_or(0);
            probes.push(HealthProbe {
                name: "disk".into(),
                status: disk_status.into(),
                latency_ms: 0.0,
                detail: if disk_probe_failed {
                    disk.errors.join(" | ")
                } else {
                    format!("{free_mb} MB free")
                },
            });

            // Overall health
            let overall = if !db_ok || backpressure_unhealthy {
                "unhealthy"
            } else if !integrity_ok
                || !circuits_ok
                || backpressure_degraded
                || disk_probe_failed
                || disk.pressure != mcp_agent_mail_core::disk::DiskPressure::Ok
            {
                "degraded"
            } else {
                "healthy"
            };

            #[derive(Serialize)]
            struct HealthData {
                overall: String,
                health_level: String,
                probes: Vec<HealthProbe>,
                circuits: Vec<CircuitEntry>,
            }

            #[derive(Serialize)]
            struct CircuitEntry {
                subsystem: String,
                state: String,
                failures: u32,
                threshold: u32,
            }

            let mut env = RobotEnvelope::new(
                cmd_name,
                format,
                HealthData {
                    overall: overall.into(),
                    health_level: health_str,
                    probes,
                    circuits: circuit_entries,
                },
            );

            // Alerts
            if !db_ok {
                env = env.with_alert("error", "Database connectivity probe failed", None);
            }
            if !circuits_ok && let Some(rec) = &db_health.recommendation {
                env = env.with_alert("error", rec, None);
            }
            if !integrity_ok {
                env = env.with_alert(
                    "warn",
                    format!(
                        "Latest integrity check failed ({} cumulative failures recorded)",
                        integrity.failures_total
                    ),
                    None,
                );
            }
            if disk.pressure == mcp_agent_mail_core::disk::DiskPressure::Critical
                || disk.pressure == mcp_agent_mail_core::disk::DiskPressure::Fatal
            {
                env = env.with_alert(
                    "error",
                    format!(
                        "Disk pressure: {} ({free_mb} MB free)",
                        disk.pressure.label()
                    ),
                    None,
                );
            } else if disk.pressure == mcp_agent_mail_core::disk::DiskPressure::Warning {
                env = env.with_alert(
                    "warn",
                    format!("Disk pressure: warning ({free_mb} MB free)"),
                    None,
                );
            }

            // Actions
            if !db_ok {
                env = env.with_action("Check DATABASE_URL env var and SQLite file accessibility");
            }
            if config.integrity_check_interval_hours > 0
                && mcp_agent_mail_db::is_full_check_due(config.integrity_check_interval_hours)
            {
                env = env.with_action(format!(
                    "Run full integrity check (last full check >{}h ago)",
                    config.integrity_check_interval_hours
                ));
            }

            format_output(&env, format)?
        }
        RobotSubcommand::Timeline {
            since,
            kind,
            source,
        } => {
            let conn = crate::open_db_sync_robot()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let events = build_timeline(
                &conn,
                project_id,
                since.as_deref(),
                kind.as_deref(),
                source.as_deref(),
            )?;

            #[derive(Serialize)]
            struct TimelineData {
                count: usize,
                events: Vec<TimelineEvent>,
            }

            let count = events.len();
            let mut env = RobotEnvelope::new(cmd_name, format, TimelineData { count, events });
            env._meta.project = Some(project_slug);
            format_output(&env, format)?
        }
        RobotSubcommand::Overview => {
            let conn = crate::open_db_sync_robot()?;
            let projects = build_overview(&conn)?;

            #[derive(Serialize)]
            struct OverviewData {
                project_count: usize,
                projects: Vec<OverviewProject>,
            }

            let project_count = projects.len();
            let env = RobotEnvelope::new(
                cmd_name,
                format,
                OverviewData {
                    project_count,
                    projects,
                },
            );
            format_output(&env, format)?
        }
        RobotSubcommand::Analytics => {
            let conn = crate::open_db_sync_robot()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let agent = resolve_optional_agent_id(&conn, project_id, args.agent.as_deref())?;
            let anomalies = build_analytics(&conn, project_id, agent)?;

            #[derive(Serialize)]
            struct AnalyticsData {
                anomaly_count: usize,
                anomalies: Vec<AnomalyCard>,
            }

            let anomaly_count = anomalies.len();
            let mut env = RobotEnvelope::new(
                cmd_name,
                format,
                AnalyticsData {
                    anomaly_count,
                    anomalies: anomalies.clone(),
                },
            );
            env._meta.project = Some(project_slug);
            for a in &anomalies {
                env = env.with_alert(&a.severity, &a.headline, Some(a.remediation.clone()));
            }
            format_output(&env, format)?
        }
        RobotSubcommand::Agents { active, sort } => {
            let scope = resolve_robot_scope(args.project.as_deref(), None)?;
            let agents = build_agents(scope.conn(), scope.project_id, active, sort.as_deref())?;

            #[derive(Serialize)]
            struct AgentsData {
                count: usize,
                agents: Vec<AgentRow>,
            }

            let count = agents.len();
            let mut env = RobotEnvelope::new(cmd_name, format, AgentsData { count, agents });
            env._meta.project = Some(scope.project_slug);
            format_output(&env, format)?
        }
        RobotSubcommand::Contacts => {
            let conn = crate::open_db_sync_robot()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let contacts = build_contacts(&conn, project_id)?;

            #[derive(Serialize)]
            struct ContactsData {
                count: usize,
                contacts: Vec<ContactRow>,
            }

            let count = contacts.len();
            let mut env = RobotEnvelope::new(cmd_name, format, ContactsData { count, contacts });
            env._meta.project = Some(project_slug);
            format_output(&env, format)?
        }
        RobotSubcommand::Projects => {
            let conn = crate::open_db_sync_robot()?;
            let projects = build_projects(&conn)?;

            #[derive(Serialize)]
            struct ProjectsData {
                count: usize,
                projects: Vec<ProjectRow>,
            }

            let count = projects.len();
            let env = RobotEnvelope::new(cmd_name, format, ProjectsData { count, projects });
            format_output(&env, format)?
        }
        RobotSubcommand::Attachments => {
            let conn = crate::open_db_sync_robot_attachments()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;

            let rows = conn
                .query_sync(
                    "SELECT m.id, m.subject, m.attachments, a.name AS sender_name, m.created_ts
                     FROM messages m
                     JOIN agents a ON a.id = m.sender_id
                     WHERE m.project_id = ? AND m.attachments != '[]'
                     ORDER BY m.created_ts DESC
                     LIMIT 100",
                    &[Value::BigInt(project_id)],
                )
                .map_err(|e| CliError::Other(format!("attachments query: {e}")))?;

            let mut attachments: Vec<AttachmentRow> = Vec::new();
            for row in &rows {
                let msg_id: i64 = row.get_named("id").unwrap_or(0);
                let subject: String = row.get_named("subject").unwrap_or_default();
                let sender: String = row.get_named("sender_name").unwrap_or_default();
                let att_json: String = row.get_named("attachments").unwrap_or_default();

                for attachment in parse_attachments_json(&att_json) {
                    attachments.push(AttachmentRow {
                        r#type: attachment.mime_type,
                        size: attachment.size_bytes,
                        sender: sender.clone(),
                        subject: truncate_str(&subject, 60).to_string(),
                        message_id: msg_id,
                        project: project_slug.clone(),
                    });
                }
            }

            #[derive(Serialize)]
            struct AttachmentsData {
                count: usize,
                attachments: Vec<AttachmentRow>,
            }

            let count = attachments.len();
            let mut env =
                RobotEnvelope::new(cmd_name, format, AttachmentsData { count, attachments });
            env._meta.project = Some(project_slug);
            format_output(&env, format)?
        }
        RobotSubcommand::Navigate { uri } => {
            let (result, resolved_project) = if navigate_should_use_canonical_resource(&uri) {
                build_navigate_from_canonical_resource(&uri, args.project.as_deref())?
            } else if let Some(result) = build_navigate_without_db(&uri)? {
                result
            } else {
                let conn = crate::open_db_sync_robot()?;
                let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
                let agent = resolve_optional_agent_id(&conn, project_id, args.agent.as_deref())?;
                build_navigate(&conn, &uri, project_id, &project_slug, agent)?
            };

            #[derive(Serialize)]
            struct NavigateData {
                uri: String,
                #[serde(flatten)]
                result: NavigateResult,
            }

            let mut env = RobotEnvelope::new(
                cmd_name,
                format,
                NavigateData {
                    uri: uri.clone(),
                    result,
                },
            );
            env._meta.project = resolved_project;
            format_output(&env, format)?
        }
        RobotSubcommand::Atc {
            decisions,
            liveness,
            conflicts,
            summary,
            limit,
        } => {
            // ATC is a server-side subsystem; the robot CLI surfaces its
            // state via a lightweight snapshot.  Since the ATC runs on the
            // poller thread and we're in a separate CLI process, we report
            // a static "ATC available" status.  Full live state requires
            // the TUI (Track 9 Option B) or a future HTTP API.
            #[derive(Serialize)]
            struct AtcData {
                enabled: bool,
                note: String,
                #[serde(skip_serializing_if = "Option::is_none")]
                decisions_requested: Option<bool>,
                #[serde(skip_serializing_if = "Option::is_none")]
                liveness_requested: Option<bool>,
                #[serde(skip_serializing_if = "Option::is_none")]
                conflicts_requested: Option<bool>,
                #[serde(skip_serializing_if = "Option::is_none")]
                summary_requested: Option<bool>,
                #[serde(skip_serializing_if = "Option::is_none")]
                limit: Option<usize>,
            }

            let data = AtcData {
                enabled: true,
                note: "ATC runs as a server-side subsystem. Use the TUI \
                       System Health screen for live state, or start the \
                       server with AM_ATC_ENABLED=true."
                    .to_string(),
                decisions_requested: if decisions { Some(true) } else { None },
                liveness_requested: if liveness { Some(true) } else { None },
                conflicts_requested: if conflicts { Some(true) } else { None },
                summary_requested: if summary { Some(true) } else { None },
                limit,
            };

            let env = RobotEnvelope::new(cmd_name, format, data);
            format_output(&env, format)?
        }
    };

    emit_robot_output(&out);
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static NAVIGATE_RESOURCE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_navigate_resource_env<F, T>(database_url: &str, storage_root: &str, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let _lock = NAVIGATE_RESOURCE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", database_url),
                ("STORAGE_ROOT", storage_root),
            ],
            f,
        )
    }

    #[derive(Debug, Serialize)]
    struct TestData {
        items: Vec<String>,
        count: usize,
    }

    #[test]
    fn test_output_format_display_and_parse() {
        assert_eq!(OutputFormat::Toon.to_string(), "toon");
        assert_eq!(OutputFormat::Json.to_string(), "json");
        assert_eq!(OutputFormat::Markdown.to_string(), "markdown");

        assert_eq!("toon".parse::<OutputFormat>().unwrap(), OutputFormat::Toon);
        assert_eq!("JSON".parse::<OutputFormat>().unwrap(), OutputFormat::Json);
        assert_eq!(
            "md".parse::<OutputFormat>().unwrap(),
            OutputFormat::Markdown
        );
        assert_eq!(
            "Markdown".parse::<OutputFormat>().unwrap(),
            OutputFormat::Markdown
        );
        assert!("xml".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn test_envelope_serialization_empty_alerts_actions() {
        let envelope = RobotEnvelope::new(
            "robot status",
            OutputFormat::Json,
            TestData {
                items: vec!["a".into(), "b".into()],
                count: 2,
            },
        );

        let json_str = serde_json::to_string_pretty(&envelope).unwrap();
        let v: Value = serde_json::from_str(&json_str).unwrap();

        // _meta must be present
        assert!(v.get("_meta").is_some());
        let meta = &v["_meta"];
        assert_eq!(meta["command"], "robot status");
        assert_eq!(meta["format"], "json");
        assert_eq!(meta["version"], "1.0");
        assert!(meta["timestamp"].as_str().is_some());

        // _alerts and _actions must be absent (empty → skipped)
        assert!(v.get("_alerts").is_none());
        assert!(v.get("_actions").is_none());

        // data fields flattened to top level
        assert_eq!(v["count"], 2);
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_envelope_with_alerts_and_actions() {
        let envelope = RobotEnvelope::new(
            "robot inbox",
            OutputFormat::Toon,
            TestData {
                items: vec![],
                count: 0,
            },
        )
        .with_alert(
            "warn",
            "3 ack-overdue messages",
            Some("am robot inbox --ack-overdue".into()),
        )
        .with_action("am acknowledge 42")
        .with_action("am robot reservations --expiring=30");

        let json_str = serde_json::to_string(&envelope).unwrap();
        let v: Value = serde_json::from_str(&json_str).unwrap();

        let alerts = v["_alerts"].as_array().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["severity"], "warn");
        assert_eq!(alerts[0]["summary"], "3 ack-overdue messages");
        assert!(alerts[0]["action"].as_str().is_some());

        let actions = v["_actions"].as_array().unwrap();
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn test_format_output_json() {
        let envelope = RobotEnvelope::new(
            "test",
            OutputFormat::Json,
            TestData {
                items: vec!["x".into()],
                count: 1,
            },
        );
        let out = format_output(&envelope, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["count"], 1);
    }

    #[test]
    fn test_format_output_toon() {
        let envelope = RobotEnvelope::new(
            "test",
            OutputFormat::Toon,
            TestData {
                items: vec!["hello".into()],
                count: 1,
            },
        );
        let out = format_output(&envelope, OutputFormat::Toon).unwrap();
        // TOON output should be non-empty and different from JSON
        assert!(!out.is_empty());
        assert!(!out.starts_with('{'));
    }

    #[test]
    fn test_format_output_markdown_fallback() {
        let envelope = RobotEnvelope::new(
            "test",
            OutputFormat::Markdown,
            TestData {
                items: vec![],
                count: 0,
            },
        );
        // Without MarkdownRenderable, falls back to TOON
        let out = format_output(&envelope, OutputFormat::Markdown).unwrap();
        assert!(!out.is_empty());
    }

    #[derive(Debug, Serialize)]
    struct MdData {
        title: String,
    }

    impl MarkdownRenderable for MdData {
        fn to_markdown(
            &self,
            meta: &RobotMeta,
            _alerts: &[RobotAlert],
            _actions: &[String],
        ) -> String {
            format!(
                "# {}\n\n*Generated by {} at {}*",
                self.title, meta.command, meta.timestamp
            )
        }
    }

    #[test]
    fn test_format_output_md_with_trait() {
        let envelope = RobotEnvelope::new(
            "robot thread",
            OutputFormat::Markdown,
            MdData {
                title: "Test Thread".into(),
            },
        );
        let out = format_output_md(&envelope, OutputFormat::Markdown).unwrap();
        assert!(out.starts_with("# Test Thread"));
        assert!(out.contains("robot thread"));

        // Non-markdown formats should still work through format_output
        let json_out = format_output_md(&envelope, OutputFormat::Json).unwrap();
        assert!(json_out.starts_with('{'));
    }

    #[test]
    fn test_is_prose_command() {
        assert!(is_prose_command("thread"));
        assert!(is_prose_command("message"));
        assert!(!is_prose_command("status"));
        assert!(!is_prose_command("inbox"));
        assert!(!is_prose_command("metrics"));
    }

    #[test]
    fn test_resolve_format_explicit_overrides() {
        assert_eq!(
            resolve_format(Some(OutputFormat::Json), "thread"),
            OutputFormat::Json
        );
        assert_eq!(
            resolve_format(Some(OutputFormat::Toon), "message"),
            OutputFormat::Toon
        );
        assert_eq!(
            resolve_format(Some(OutputFormat::Markdown), "status"),
            OutputFormat::Markdown
        );
    }

    #[test]
    fn test_resolve_format_prose_default() {
        // Without explicit format, prose commands default to Markdown
        assert_eq!(resolve_format(None, "thread"), OutputFormat::Markdown);
        assert_eq!(resolve_format(None, "message"), OutputFormat::Markdown);
    }

    #[test]
    fn test_resolve_format_non_prose_auto() {
        // Non-prose, non-TTY (test runner pipes stdout) → JSON
        let fmt = resolve_format(None, "status");
        // In test context, stdout is not a TTY → JSON
        assert_eq!(fmt, OutputFormat::Json);
    }

    #[test]
    fn test_domain_types_serialize_to_toon() {
        // Verify all domain types can round-trip through JSON → TOON
        let inbox = vec![InboxEntry {
            id: 42,
            priority: "high".into(),
            from: "RedHarbor".into(),
            subject: "Test".into(),
            thread: "br-123".into(),
            age: "5m".into(),
            ack_status: "pending".into(),
            importance: "urgent".into(),
            body_md: None,
        }];
        let json = serde_json::to_string(&inbox).unwrap();
        let toon_out = toon::json_to_toon(&json).unwrap();
        assert!(!toon_out.is_empty());

        let agents = vec![AgentRow {
            name: "BlueLake".into(),
            program: "claude-code".into(),
            model: "opus-4.6".into(),
            last_active: "2m ago".into(),
            msg_count: 15,
            status: "active".into(),
        }];
        let json = serde_json::to_string(&agents).unwrap();
        let toon_out = toon::json_to_toon(&json).unwrap();
        assert!(!toon_out.is_empty());

        let metrics = vec![MetricEntry {
            name: "send_message".into(),
            calls: 100,
            errors: 2,
            error_pct: 2.0,
            avg_ms: 12.5,
            p95_ms: 25.0,
            p99_ms: 50.0,
        }];
        let json = serde_json::to_string(&metrics).unwrap();
        let toon_out = toon::json_to_toon(&json).unwrap();
        assert!(!toon_out.is_empty());
    }

    #[test]
    fn test_toon_token_savings() {
        // Verify TOON produces fewer characters than JSON for tabular data
        let rows: Vec<AgentRow> = (0..5)
            .map(|i| AgentRow {
                name: format!("Agent{i}"),
                program: "claude-code".into(),
                model: "opus-4.6".into(),
                last_active: format!("{i}m ago"),
                msg_count: i * 10,
                status: "active".into(),
            })
            .collect();

        let json = serde_json::to_string_pretty(&rows).unwrap();
        let json_compact = serde_json::to_string(&rows).unwrap();
        let toon_out = toon::json_to_toon(&json_compact).unwrap();

        // TOON should be shorter than pretty JSON
        assert!(
            toon_out.len() < json.len(),
            "TOON ({} bytes) should be shorter than JSON ({} bytes)",
            toon_out.len(),
            json.len()
        );
    }

    #[test]
    fn test_thread_message_markdown_rendering() {
        let messages = vec![
            ThreadMessage {
                position: 1,
                from: "RedHarbor".into(),
                to: "BlueLake".into(),
                age: "10m".into(),
                importance: "normal".into(),
                ack: "read".into(),
                subject: "Plan review".into(),
                body: Some("Looks good.".into()),
            },
            ThreadMessage {
                position: 2,
                from: "BlueLake".into(),
                to: "RedHarbor".into(),
                age: "5m".into(),
                importance: "normal".into(),
                ack: "pending".into(),
                subject: "Re: Plan review".into(),
                body: Some("Thanks!".into()),
            },
        ];

        let envelope = RobotEnvelope::new("robot thread TKT-1", OutputFormat::Markdown, messages);
        let md = format_output_md(&envelope, OutputFormat::Markdown).unwrap();
        assert!(md.contains("RedHarbor"));
        assert!(md.contains("BlueLake"));
        assert!(md.contains("Looks good."));
        assert!(md.contains("Thanks!"));
    }

    #[test]
    fn test_message_context_markdown() {
        let msg = MessageContext {
            id: 42,
            from: "GoldHawk".into(),
            from_program: Some("claude-code".into()),
            from_model: Some("opus-4.6".into()),
            to: vec!["SilverCove".into(), "RedHarbor".into()],
            subject: "Important update".into(),
            body: "Here are the details...".into(),
            thread: "TKT-5".into(),
            position: 3,
            total_in_thread: 7,
            importance: "high".into(),
            ack_status: "required".into(),
            created: "2026-02-11T10:00:00Z".into(),
            age: "2h ago".into(),
            previous: Some("#41 RedHarbor: Previous message".into()),
            next: None,
            attachments: vec![],
        };

        let envelope = RobotEnvelope::new("robot message 42", OutputFormat::Markdown, msg);
        let md = format_output_md(&envelope, OutputFormat::Markdown).unwrap();
        assert!(md.contains("Important update"));
        assert!(md.contains("GoldHawk"));
        assert!(md.contains("claude-code"));
        assert!(md.contains("SilverCove, RedHarbor"));
        assert!(md.contains("3 of 7"));
        assert!(md.contains("Here are the details..."));
        assert!(md.contains("Previous"));
    }

    #[test]
    fn test_inbox_entry_serialization_with_body() {
        let entry = InboxEntry {
            id: 100,
            priority: "ack-overdue".into(),
            from: "RedFox".into(),
            subject: "Urgent review needed".into(),
            thread: "AUTH-001".into(),
            age: "35m ago".into(),
            ack_status: "overdue".into(),
            importance: "high".into(),
            body_md: Some("Please review the auth changes".into()),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["id"], 100);
        assert_eq!(json["priority"], "ack-overdue");
        assert_eq!(json["body_md"], "Please review the auth changes");
    }

    #[test]
    fn test_inbox_entry_serialization_without_body() {
        let entry = InboxEntry {
            id: 200,
            priority: "unread".into(),
            from: "BlueLake".into(),
            subject: "FYI".into(),
            thread: "".into(),
            age: "1h ago".into(),
            ack_status: "none".into(),
            importance: "normal".into(),
            body_md: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["id"], 200);
        assert!(
            json.get("body_md").is_none(),
            "body_md should be omitted when None"
        );
    }

    #[test]
    fn test_inbox_envelope_format_toon() {
        #[derive(Serialize)]
        struct InboxData {
            count: usize,
            inbox: Vec<InboxEntry>,
        }
        let data = InboxData {
            count: 2,
            inbox: vec![
                InboxEntry {
                    id: 1,
                    priority: "ack-overdue".into(),
                    from: "RedFox".into(),
                    subject: "Review auth".into(),
                    thread: "AUTH-1".into(),
                    age: "35m ago".into(),
                    ack_status: "overdue".into(),
                    importance: "high".into(),
                    body_md: None,
                },
                InboxEntry {
                    id: 2,
                    priority: "urgent".into(),
                    from: "BlueLake".into(),
                    subject: "Blocking issue".into(),
                    thread: "FEAT-1".into(),
                    age: "10m ago".into(),
                    ack_status: "required".into(),
                    importance: "urgent".into(),
                    body_md: None,
                },
            ],
        };
        let mut env = RobotEnvelope::new("robot inbox", OutputFormat::Toon, data);
        env = env.with_alert(
            "warn",
            "1 message ack overdue",
            Some("am mail ack 1".into()),
        );
        env = env.with_action("am mail ack 1");

        // Verify TOON output
        let toon_out = format_output(&env, OutputFormat::Toon).unwrap();
        assert!(!toon_out.is_empty());

        // Verify JSON output
        let json_out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&json_out).unwrap();
        assert_eq!(v["count"], 2);
        let inbox_arr = v["inbox"].as_array().unwrap();
        assert_eq!(inbox_arr.len(), 2);
        assert_eq!(inbox_arr[0]["priority"], "ack-overdue");
        assert_eq!(inbox_arr[1]["priority"], "urgent");
        assert_eq!(v["_alerts"][0]["severity"], "warn");
        assert_eq!(v["_actions"][0], "am mail ack 1");
    }

    #[test]
    fn test_inbox_priority_ordering() {
        // Verify priority labels map correctly
        let labels = [
            "ack-overdue",
            "urgent",
            "ack-required",
            "high",
            "unread",
            "read-unacked",
            "read",
        ];
        for (i, expected) in labels.iter().enumerate() {
            let bucket = (i + 1) as i64;
            let label = match bucket {
                1 => "ack-overdue",
                2 => "urgent",
                3 => "ack-required",
                4 => "high",
                5 => "unread",
                6 => "read-unacked",
                _ => "read",
            };
            assert_eq!(label, *expected, "bucket {bucket} should be {expected}");
        }
    }

    #[test]
    fn test_message_context_with_attachments() {
        let msg = MessageContext {
            id: 201,
            from: "BlueLake".into(),
            from_program: Some("claude-code".into()),
            from_model: Some("opus-4.6".into()),
            to: vec!["RedFox".into(), "GreenCastle".into()],
            subject: "JWT implementation plan".into(),
            body: "Planning JWT with JWKS rotation.".into(),
            thread: "FEAT-123".into(),
            position: 3,
            total_in_thread: 8,
            importance: "high".into(),
            ack_status: "pending".into(),
            created: "2026-02-11T08:30:00Z".into(),
            age: "2h ago".into(),
            previous: Some("#200 RedFox: I'll handle the middleware setup".into()),
            next: Some("#202 RedFox: Sounds good, releasing reservations".into()),
            attachments: vec![AttachmentInfo {
                name: "api_spec.json".into(),
                size: "8KB".into(),
                mime_type: "application/json".into(),
            }],
        };

        // Verify JSON serialization
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["id"], 201);
        assert_eq!(json["from_program"], "claude-code");
        assert_eq!(json["attachments"][0]["name"], "api_spec.json");
        assert_eq!(
            json["previous"],
            "#200 RedFox: I'll handle the middleware setup"
        );
        assert_eq!(json["position"], 3);
        assert_eq!(json["total_in_thread"], 8);

        // Verify markdown rendering
        let env = RobotEnvelope::new("robot message 201", OutputFormat::Markdown, msg);
        let md = format_output_md(&env, OutputFormat::Markdown).unwrap();
        assert!(md.contains("3 of 8"));
        assert!(md.contains("claude-code"));
        assert!(md.contains("api_spec.json"));
        assert!(md.contains("Previous"));
        assert!(md.contains("Next"));
    }

    #[test]
    fn test_search_data_serialization() {
        let data = SearchData {
            query: "auth JWT".into(),
            total_results: 2,
            results: vec![
                SearchResult {
                    id: 201,
                    relevance: 0.95,
                    from: "BlueLake".into(),
                    subject: "JWT plan".into(),
                    thread: "FEAT-123".into(),
                    snippet: "...JWT with JWKS rotation...".into(),
                    age: "2h ago".into(),
                },
                SearchResult {
                    id: 198,
                    relevance: 0.87,
                    from: "RedFox".into(),
                    subject: "Auth review".into(),
                    thread: "FEAT-123".into(),
                    snippet: "...middleware chain for auth...".into(),
                    age: "3h ago".into(),
                },
            ],
            by_thread: vec![FacetEntry {
                value: "FEAT-123".into(),
                count: 2,
            }],
            by_agent: vec![
                FacetEntry {
                    value: "BlueLake".into(),
                    count: 1,
                },
                FacetEntry {
                    value: "RedFox".into(),
                    count: 1,
                },
            ],
            by_importance: vec![FacetEntry {
                value: "high".into(),
                count: 2,
            }],
        };

        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["total_results"], 2);
        assert_eq!(json["results"].as_array().unwrap().len(), 2);
        assert_eq!(json["by_thread"][0]["value"], "FEAT-123");
        assert_eq!(json["by_agent"].as_array().unwrap().len(), 2);

        // TOON output
        let env = RobotEnvelope::new("robot search", OutputFormat::Toon, data);
        let toon = format_output(&env, OutputFormat::Toon).unwrap();
        assert!(!toon.is_empty());
    }

    #[test]
    fn collect_search_rows_with_runner_pages_until_kind_matches_fill_limit() {
        use mcp_agent_mail_db::search_planner::{
            DocKind, SearchQuery, SearchResponse, SearchResult as PlannerSearchResult,
        };

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_search_kind_paging.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];
        conn.query_sync(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create recipients");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind) VALUES
                (1, 10, 1, 'cc'),
                (2, 20, 1, 'to'),
                (3, 30, 1, 'to')",
            &empty,
        )
        .expect("insert recipients");

        let mut query = SearchQuery::messages("needle", 1);
        query.limit = Some(search_page_limit(2, Some("to")));

        let mut call_count = 0usize;
        let rows = collect_search_rows_with_runner(&conn, query, Some("to"), 2, |query| {
            call_count += 1;
            match query.cursor.as_deref() {
                None => Ok(SearchResponse {
                    results: vec![
                        PlannerSearchResult {
                            doc_kind: DocKind::Message,
                            id: 10,
                            project_id: Some(1),
                            title: "first".into(),
                            body: "body".into(),
                            score: Some(0.9),
                            importance: Some("normal".into()),
                            ack_required: Some(false),
                            created_ts: Some(100),
                            thread_id: Some("th-1".into()),
                            from_agent: Some("Alpha".into()),
                            reason_codes: vec![],
                            score_factors: vec![],
                            redacted: false,
                            redaction_reason: None,
                            ..PlannerSearchResult::default()
                        },
                        PlannerSearchResult {
                            doc_kind: DocKind::Message,
                            id: 20,
                            project_id: Some(1),
                            title: "second".into(),
                            body: "body".into(),
                            score: Some(0.8),
                            importance: Some("normal".into()),
                            ack_required: Some(false),
                            created_ts: Some(200),
                            thread_id: Some("th-2".into()),
                            from_agent: Some("Beta".into()),
                            reason_codes: vec![],
                            score_factors: vec![],
                            redacted: false,
                            redaction_reason: None,
                            ..PlannerSearchResult::default()
                        },
                    ],
                    next_cursor: Some("cursor-2".into()),
                    explain: None,
                    assistance: None,
                    guidance: None,
                    audit: vec![],
                }),
                Some("cursor-2") => Ok(SearchResponse {
                    results: vec![PlannerSearchResult {
                        doc_kind: DocKind::Message,
                        id: 30,
                        project_id: Some(1),
                        title: "third".into(),
                        body: "body".into(),
                        score: Some(0.7),
                        importance: Some("normal".into()),
                        ack_required: Some(false),
                        created_ts: Some(300),
                        thread_id: Some("th-3".into()),
                        from_agent: Some("Gamma".into()),
                        reason_codes: vec![],
                        score_factors: vec![],
                        redacted: false,
                        redaction_reason: None,
                        ..PlannerSearchResult::default()
                    }],
                    next_cursor: None,
                    explain: None,
                    assistance: None,
                    guidance: None,
                    audit: vec![],
                }),
                other => panic!("unexpected cursor: {other:?}"),
            }
        })
        .expect("collect kind-filtered rows");

        assert_eq!(
            call_count, 2,
            "should page until matching rows fill the limit"
        );
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![20, 30]
        );
    }

    #[test]
    fn summarize_integrity_probe_uses_latest_check_not_cumulative_failures() {
        let (probe, ok) = summarize_integrity_probe(&mcp_agent_mail_db::IntegrityMetrics {
            last_ok_ts: 1_000,
            last_check_ts: 2_000,
            checks_total: 5,
            failures_total: 2,
        });
        assert!(!ok);
        assert_eq!(probe.status, "warn");
        assert!(probe.detail.contains("last check failed"));

        let (probe, ok) = summarize_integrity_probe(&mcp_agent_mail_db::IntegrityMetrics {
            last_ok_ts: 2_000,
            last_check_ts: 2_000,
            checks_total: 5,
            failures_total: 2,
        });
        assert!(ok);
        assert_eq!(probe.status, "ok");
        assert!(probe.detail.contains("historical failures"));

        let (probe, ok) = summarize_integrity_probe(&mcp_agent_mail_db::IntegrityMetrics {
            last_ok_ts: 0,
            last_check_ts: 0,
            checks_total: 0,
            failures_total: 1,
        });
        assert!(!ok);
        assert_eq!(probe.status, "warn");
        assert!(probe.detail.contains("runtime corruption failures"));
    }

    #[test]
    fn test_reservations_data_with_conflicts() {
        let data = ReservationsData {
            my_reservations: vec![ReservationEntry {
                agent: Some("BlueLake".into()),
                path: "src/auth/**".into(),
                exclusive: true,
                remaining_seconds: 2400,
                remaining: Some("40m".into()),
                granted_at: Some("2h ago".into()),
            }],
            all_active: vec![
                ReservationEntry {
                    agent: Some("BlueLake".into()),
                    path: "src/auth/**".into(),
                    exclusive: true,
                    remaining_seconds: 2400,
                    remaining: Some("40m".into()),
                    granted_at: Some("2h ago".into()),
                },
                ReservationEntry {
                    agent: Some("RedFox".into()),
                    path: "src/auth/jwt.rs".into(),
                    exclusive: true,
                    remaining_seconds: 300,
                    remaining: Some("5m \u{26a0}".into()),
                    granted_at: Some("55m ago".into()),
                },
            ],
            conflicts: vec![ReservationConflict {
                agent_a: "BlueLake".into(),
                path_a: "src/auth/**".into(),
                agent_b: "RedFox".into(),
                path_b: "src/auth/jwt.rs".into(),
            }],
            expiring_soon: vec![ReservationEntry {
                agent: Some("RedFox".into()),
                path: "src/auth/jwt.rs".into(),
                exclusive: true,
                remaining_seconds: 300,
                remaining: Some("5m \u{26a0}".into()),
                granted_at: Some("55m ago".into()),
            }],
        };

        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["all_active"].as_array().unwrap().len(), 2);
        assert_eq!(json["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(json["conflicts"][0]["agent_a"], "BlueLake");
        assert_eq!(json["expiring_soon"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_glob_matches() {
        assert!(glob_matches("src/auth/**", "src/auth/jwt.rs"));
        assert!(glob_matches("src/auth/**", "src/auth/sub/file.rs"));
        assert!(!glob_matches("src/auth/**", "src/middleware/foo.rs"));
        assert!(glob_matches("src/auth/*", "src/auth/jwt.rs"));
        assert!(!glob_matches("src/auth/*", "src/auth/sub/file.rs"));
        assert!(glob_matches("src/auth/jwt.rs", "src/auth/jwt.rs"));
        assert!(!glob_matches("src/auth/jwt.rs", "src/auth/other.rs"));
        assert!(glob_matches("src/**/*.rs", "src/*/main.rs"));
        assert!(!glob_matches("src/**/*.rs", "docs/**/*.md"));
    }

    #[test]
    fn test_format_remaining_warnings() {
        assert!(!format_remaining(700).contains('\u{26a0}'));
        assert!(format_remaining(500).contains('\u{26a0}'));
        assert!(!format_remaining(500).contains("\u{26a0}\u{26a0}"));
        assert!(format_remaining(60).contains("\u{26a0}\u{26a0}"));
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(
            truncate_str("hello world this is long", 10),
            "hello worl..."
        );
    }

    // ── Track 4 Tests: Monitoring & Analytics ────────────────────────────────

    #[test]
    fn test_metric_entry_serialization() {
        let entry = MetricEntry {
            name: "send_message".into(),
            calls: 150,
            errors: 3,
            error_pct: 2.0,
            avg_ms: 45.2,
            p95_ms: 120.0,
            p99_ms: 250.0,
        };
        let v: Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["name"], "send_message");
        assert_eq!(v["calls"], 150);
        assert_eq!(v["errors"], 3);
        assert_eq!(v["error_pct"], 2.0);
        assert_eq!(v["avg_ms"], 45.2);
        assert_eq!(v["p95_ms"], 120.0);
        assert_eq!(v["p99_ms"], 250.0);
    }

    #[test]
    fn test_metric_entry_toon_round_trip() {
        let entries = vec![
            MetricEntry {
                name: "fetch_inbox".into(),
                calls: 500,
                errors: 0,
                error_pct: 0.0,
                avg_ms: 12.3,
                p95_ms: 30.0,
                p99_ms: 55.0,
            },
            MetricEntry {
                name: "send_message".into(),
                calls: 200,
                errors: 10,
                error_pct: 5.0,
                avg_ms: 88.1,
                p95_ms: 200.0,
                p99_ms: 500.0,
            },
        ];
        let json = serde_json::to_string(&entries).unwrap();
        let toon = toon::json_to_toon(&json).unwrap();
        assert!(!toon.is_empty());
        assert!(toon.contains("fetch_inbox"));
        assert!(toon.contains("send_message"));
    }

    #[test]
    fn test_health_probe_serialization() {
        let probe = HealthProbe {
            name: "db_connectivity".into(),
            status: "ok".into(),
            latency_ms: 1.5,
            detail: "SQLite connection healthy".into(),
        };
        let v: Value = serde_json::to_value(&probe).unwrap();
        assert_eq!(v["name"], "db_connectivity");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["latency_ms"], 1.5);
        assert_eq!(v["detail"], "SQLite connection healthy");
    }

    #[test]
    fn test_health_probe_toon_round_trip() {
        let probes = vec![
            HealthProbe {
                name: "db_connectivity".into(),
                status: "ok".into(),
                latency_ms: 1.5,
                detail: "Healthy".into(),
            },
            HealthProbe {
                name: "circuit_breakers".into(),
                status: "degraded".into(),
                latency_ms: 0.0,
                detail: "Circuit open (5 failures)".into(),
            },
            HealthProbe {
                name: "disk".into(),
                status: "warning".into(),
                latency_ms: 0.0,
                detail: "512 MB free".into(),
            },
        ];
        let json = serde_json::to_string(&probes).unwrap();
        let toon = toon::json_to_toon(&json).unwrap();
        assert!(toon.contains("db_connectivity"));
        assert!(toon.contains("degraded"));
        assert!(toon.contains("disk"));
    }

    #[test]
    fn test_anomaly_card_serialization() {
        let card = AnomalyCard {
            severity: "error".into(),
            confidence: 0.95,
            category: "ack_sla".into(),
            headline: "5 messages overdue".into(),
            rationale: "Pending >1h".into(),
            remediation: "am robot inbox --ack-overdue".into(),
        };
        let v: Value = serde_json::to_value(&card).unwrap();
        assert_eq!(v["severity"], "error");
        assert_eq!(v["confidence"], 0.95);
        assert_eq!(v["category"], "ack_sla");
        assert_eq!(v["headline"], "5 messages overdue");
        assert_eq!(v["rationale"], "Pending >1h");
        assert_eq!(v["remediation"], "am robot inbox --ack-overdue");
    }

    #[test]
    fn test_anomaly_card_toon_round_trip() {
        let cards = vec![
            AnomalyCard {
                severity: "warn".into(),
                confidence: 0.85,
                category: "reservation_expiry".into(),
                headline: "3 reservations expiring".into(),
                rationale: "TTL < 15 min".into(),
                remediation: "Renew reservations".into(),
            },
            AnomalyCard {
                severity: "info".into(),
                confidence: 0.70,
                category: "stale_agents".into(),
                headline: "2 agents inactive".into(),
                rationale: "No activity >1h".into(),
                remediation: "Check agent status".into(),
            },
        ];
        let json = serde_json::to_string(&cards).unwrap();
        let toon = toon::json_to_toon(&json).unwrap();
        assert!(toon.contains("reservation_expiry"));
        assert!(toon.contains("stale_agents"));
    }

    #[test]
    fn test_metrics_envelope_no_tools() {
        #[derive(Serialize)]
        struct MetricsData {
            total_calls: u64,
            total_errors: u64,
            error_rate_pct: f64,
            avg_latency_ms: f64,
            tools: Vec<MetricEntry>,
        }

        let env = RobotEnvelope::new(
            "robot metrics",
            OutputFormat::Json,
            MetricsData {
                total_calls: 0,
                total_errors: 0,
                error_rate_pct: 0.0,
                avg_latency_ms: 0.0,
                tools: vec![],
            },
        );

        let json_str = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["total_calls"], 0);
        assert_eq!(v["tools"].as_array().unwrap().len(), 0);
        assert!(v.get("_alerts").is_none());
    }

    #[test]
    fn test_health_envelope_healthy_system() {
        #[derive(Serialize)]
        struct HealthData {
            overall: String,
            health_level: String,
            probes: Vec<HealthProbe>,
        }

        let env = RobotEnvelope::new(
            "robot health",
            OutputFormat::Json,
            HealthData {
                overall: "healthy".into(),
                health_level: "green".into(),
                probes: vec![
                    HealthProbe {
                        name: "db_connectivity".into(),
                        status: "ok".into(),
                        latency_ms: 0.5,
                        detail: "Fast".into(),
                    },
                    HealthProbe {
                        name: "disk".into(),
                        status: "ok".into(),
                        latency_ms: 0.0,
                        detail: "500 GB free".into(),
                    },
                ],
            },
        );

        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["overall"], "healthy");
        assert_eq!(v["health_level"], "green");
        assert_eq!(v["probes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_analytics_envelope_with_anomalies() {
        #[derive(Serialize)]
        struct AnalyticsData {
            anomaly_count: usize,
            anomalies: Vec<AnomalyCard>,
        }

        let mut env = RobotEnvelope::new(
            "robot analytics",
            OutputFormat::Json,
            AnalyticsData {
                anomaly_count: 2,
                anomalies: vec![
                    AnomalyCard {
                        severity: "error".into(),
                        confidence: 0.95,
                        category: "ack_sla".into(),
                        headline: "10 overdue".into(),
                        rationale: "Pending >1h".into(),
                        remediation: "Acknowledge them".into(),
                    },
                    AnomalyCard {
                        severity: "warn".into(),
                        confidence: 0.80,
                        category: "reservation_expiry".into(),
                        headline: "2 expiring".into(),
                        rationale: "TTL < 15m".into(),
                        remediation: "Renew".into(),
                    },
                ],
            },
        );
        env = env.with_action("am robot inbox --ack-overdue");

        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["anomaly_count"], 2);
        assert_eq!(v["anomalies"].as_array().unwrap().len(), 2);
        assert_eq!(v["anomalies"][0]["severity"], "error");
        assert_eq!(v["anomalies"][1]["category"], "reservation_expiry");
        assert_eq!(v["_actions"].as_array().unwrap().len(), 1);
    }

    // ── Track 5 Tests: Entity Views ──────────────────────────────────────────

    #[test]
    fn test_agent_row_serialization() {
        let agent = AgentRow {
            name: "GoldHawk".into(),
            program: "claude-code".into(),
            model: "opus-4.6".into(),
            last_active: "5m ago".into(),
            msg_count: 42,
            status: "active".into(),
        };
        let v: Value = serde_json::to_value(&agent).unwrap();
        assert_eq!(v["name"], "GoldHawk");
        assert_eq!(v["program"], "claude-code");
        assert_eq!(v["msg_count"], 42);
        assert_eq!(v["status"], "active");
    }

    #[test]
    fn test_contact_row_serialization() {
        let contact = ContactRow {
            from: "GoldHawk".into(),
            to: "SilverCove".into(),
            status: "approved".into(),
            policy: "auto".into(),
            reason: "handshake".into(),
            updated: "1h ago".into(),
        };
        let v: Value = serde_json::to_value(&contact).unwrap();
        assert_eq!(v["from"], "GoldHawk");
        assert_eq!(v["to"], "SilverCove");
        assert_eq!(v["status"], "approved");
        assert_eq!(v["policy"], "auto");
    }

    #[test]
    fn test_project_row_serialization() {
        let project = ProjectRow {
            slug: "my-project".into(),
            path: "/data/projects/my-project".into(),
            agents: 5,
            messages: 120,
            reservations: 3,
            created: "2d ago".into(),
        };
        let v: Value = serde_json::to_value(&project).unwrap();
        assert_eq!(v["slug"], "my-project");
        assert_eq!(v["agents"], 5);
        assert_eq!(v["messages"], 120);
        assert_eq!(v["reservations"], 3);
    }

    #[test]
    fn test_attachment_row_serialization() {
        let att = AttachmentRow {
            r#type: "image/webp".into(),
            size: 1024,
            sender: "RedFox".into(),
            subject: "Screenshot".into(),
            message_id: 77,
            project: "my-project".into(),
        };
        let v: Value = serde_json::to_value(&att).unwrap();
        assert_eq!(v["type"], "image/webp");
        assert_eq!(v["size"], 1024);
        assert_eq!(v["sender"], "RedFox");
        assert_eq!(v["message_id"], 77);
    }

    #[test]
    fn test_agents_envelope_toon() {
        #[derive(Serialize)]
        struct AgentsData {
            count: usize,
            agents: Vec<AgentRow>,
        }

        let env = RobotEnvelope::new(
            "robot agents",
            OutputFormat::Toon,
            AgentsData {
                count: 2,
                agents: vec![
                    AgentRow {
                        name: "GoldHawk".into(),
                        program: "claude-code".into(),
                        model: "opus-4.6".into(),
                        last_active: "2m ago".into(),
                        msg_count: 50,
                        status: "active".into(),
                    },
                    AgentRow {
                        name: "SilverCove".into(),
                        program: "codex-cli".into(),
                        model: "gpt-5".into(),
                        last_active: "1h ago".into(),
                        msg_count: 10,
                        status: "idle".into(),
                    },
                ],
            },
        );

        let out = format_output(&env, OutputFormat::Toon).unwrap();
        assert!(out.contains("GoldHawk"));
        assert!(out.contains("SilverCove"));
        assert!(out.contains("active"));
        assert!(out.contains("idle"));
    }

    #[test]
    fn test_projects_envelope_json() {
        #[derive(Serialize)]
        struct ProjectsData {
            count: usize,
            projects: Vec<ProjectRow>,
        }

        let env = RobotEnvelope::new(
            "robot projects",
            OutputFormat::Json,
            ProjectsData {
                count: 1,
                projects: vec![ProjectRow {
                    slug: "test-proj".into(),
                    path: "/tmp/test".into(),
                    agents: 3,
                    messages: 50,
                    reservations: 1,
                    created: "5d ago".into(),
                }],
            },
        );

        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["projects"][0]["slug"], "test-proj");
        assert_eq!(v["projects"][0]["agents"], 3);
    }

    // ── Track 2: Situational Awareness Unit Tests ────────────────────────────

    #[test]
    fn test_status_data_serialization() {
        let data = StatusData {
            health: "ok".into(),
            unread: 5,
            urgent: 1,
            ack_required: 2,
            ack_overdue: 0,
            active_reservations: 5,
            reservations_expiring_soon: 1,
            active_agents: 3,
            recent_messages: 12,
            my_reservations: vec![],
            top_threads: vec![],
            anomalies: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"health\":\"ok\""));
        assert!(json.contains("\"unread\":5"));
        assert!(json.contains("\"active_agents\":3"));
    }

    #[test]
    fn test_status_envelope_with_degraded_health() {
        let data = StatusData {
            health: "degraded".into(),
            unread: 0,
            urgent: 0,
            ack_required: 0,
            ack_overdue: 2,
            active_reservations: 0,
            reservations_expiring_soon: 0,
            active_agents: 1,
            recent_messages: 0,
            my_reservations: vec![],
            top_threads: vec![],
            anomalies: vec![AnomalyCard {
                severity: "warn".into(),
                confidence: 0.9,
                category: "ack_sla".into(),
                headline: "2 acks overdue".into(),
                rationale: "Pending acknowledgements".into(),
                remediation: "am mail ack".into(),
            }],
        };
        let env = RobotEnvelope::new("robot status", OutputFormat::Json, data).with_alert(
            "warn",
            "Health degraded",
            None,
        );
        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["health"], "degraded");
        assert_eq!(v["_alerts"][0]["severity"], "warn");
    }

    #[test]
    fn build_status_uses_latest_thread_subject_and_all_participants() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_status.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                last_active_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                thread_id TEXT,
                subject TEXT NOT NULL,
                created_ts INTEGER NOT NULL,
                ack_required INTEGER NOT NULL DEFAULT 0,
                importance TEXT NOT NULL DEFAULT 'normal'
            )",
            &empty,
        )
        .expect("create messages");
        conn.query_sync(
            "CREATE TABLE message_recipients (
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                read_ts INTEGER,
                ack_ts INTEGER
            )",
            &empty,
        )
        .expect("create recipients");
        conn.query_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                path_pattern TEXT NOT NULL,
                exclusive INTEGER NOT NULL,
                reason TEXT NOT NULL,
                created_ts INTEGER NOT NULL,
                expires_ts INTEGER NOT NULL,
                released_ts INTEGER
            )",
            &empty,
        )
        .expect("create reservations");

        conn.query_sync(
            "INSERT INTO agents (id, project_id, name, last_active_ts) VALUES
                (1, 1, 'Alpha', 1),
                (2, 1, 'Beta', 1),
                (3, 1, 'Gamma', 1)",
            &empty,
        )
        .expect("seed agents");
        conn.query_sync(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, created_ts, ack_required, importance) VALUES
                (1, 1, 1, 'thread-1', 'Alpha subject', 100, 0, 'normal'),
                (2, 1, 1, 'thread-1', 'Latest subject', 200, 0, 'normal')",
            &empty,
        )
        .expect("seed messages");
        conn.query_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES
                (1, 2, 'to', NULL, NULL),
                (2, 2, 'to', NULL, NULL),
                (2, 3, 'cc', NULL, NULL)",
            &empty,
        )
        .expect("seed recipients");

        let (status, _) = build_status(&conn, 1, "demo", None).expect("build status");
        assert_eq!(status.top_threads.len(), 1);
        assert_eq!(status.top_threads[0].subject, "Latest subject");
        assert_eq!(status.top_threads[0].participants, 3);
    }

    #[test]
    fn build_status_numeric_seed_threads_include_root_message_and_participants() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();

        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES
                (210, 1, 1, 'Root subject', NULL, 'normal', 0, 10, 'root', '[]'),
                (211, 1, 2, 'Latest subject', '210', 'normal', 0, 20, 'reply', '[]')",
            &[],
        )
        .expect("insert thread messages");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (210, 210, 2, 'to', NULL, NULL),
                (211, 211, 3, 'cc', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let (status, _) = build_status(&conn, 1, "demo", None).expect("build status");
        assert_eq!(status.top_threads.len(), 1);
        assert_eq!(status.top_threads[0].id, "210");
        assert_eq!(status.top_threads[0].subject, "Latest subject");
        assert_eq!(status.top_threads[0].messages, 2);
        assert_eq!(status.top_threads[0].participants, 3);
    }

    #[test]
    fn build_status_keeps_root_messages_as_distinct_threads() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();

        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES
                (310, 1, 1, 'Root one', NULL, 'normal', 0, 10, 'root one', '[]'),
                (320, 1, 2, 'Root two', NULL, 'normal', 0, 20, 'root two', '[]')",
            &[],
        )
        .expect("insert root messages");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (310, 310, 2, 'to', NULL, NULL),
                (320, 320, 1, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let (status, _) = build_status(&conn, 1, "demo", None).expect("build status");
        let thread_ids: Vec<&str> = status
            .top_threads
            .iter()
            .map(|thread| thread.id.as_str())
            .collect();
        assert_eq!(thread_ids, vec!["320", "310"]);
    }

    #[test]
    fn append_status_anomaly_alerts_keeps_existing_info_alerts() {
        let anomalies = vec![AnomalyCard {
            severity: "warn".into(),
            confidence: 1.0,
            category: "ack_sla".into(),
            headline: "2 acks overdue".into(),
            rationale: "Pending acknowledgements".into(),
            remediation: "am robot inbox --ack-overdue".into(),
        }];
        let status = StatusData {
            health: "ok".into(),
            unread: 0,
            urgent: 0,
            ack_required: 0,
            ack_overdue: 2,
            active_reservations: 0,
            reservations_expiring_soon: 0,
            active_agents: 0,
            recent_messages: 0,
            my_reservations: vec![],
            top_threads: vec![],
            anomalies: anomalies.clone(),
        };
        let env = RobotEnvelope::new("robot status", OutputFormat::Json, status).with_alert(
            "info",
            "Agent not detected",
            Some("am robot status --agent <NAME>".into()),
        );

        let env = append_status_anomaly_alerts(env, &anomalies);

        assert_eq!(env._alerts.len(), 2);
        assert_eq!(env._alerts[0].severity, "info");
        assert_eq!(env._alerts[1].severity, "warn");
        assert_eq!(env._alerts[1].summary, "2 acks overdue");
        assert_eq!(
            env._alerts[1].action.as_deref(),
            Some("am robot inbox --ack-overdue")
        );
    }

    #[test]
    fn test_inbox_entry_serialization_track2() {
        let entry = InboxEntry {
            id: 123,
            priority: "ack-overdue".into(),
            from: "BlueLake".into(),
            subject: "[FEAT-1] Test".into(),
            thread: "FEAT-1".into(),
            age: "5m".into(),
            ack_status: "overdue".into(),
            importance: "high".into(),
            body_md: Some("Message body".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"priority\":\"ack-overdue\""));
        assert!(json.contains("\"from\":\"BlueLake\""));
    }

    #[test]
    fn test_inbox_envelope_priority_buckets() {
        #[derive(Serialize)]
        struct InboxData {
            count: usize,
            messages: Vec<InboxEntry>,
        }
        let messages = vec![
            InboxEntry {
                id: 1,
                priority: "ack-overdue".into(),
                from: "Agent1".into(),
                subject: "Urgent".into(),
                thread: "".into(),
                age: "45m".into(),
                ack_status: "overdue".into(),
                importance: "urgent".into(),
                body_md: None,
            },
            InboxEntry {
                id: 2,
                priority: "urgent".into(),
                from: "Agent2".into(),
                subject: "High".into(),
                thread: "".into(),
                age: "10m".into(),
                ack_status: "none".into(),
                importance: "urgent".into(),
                body_md: None,
            },
        ];
        let env = RobotEnvelope::new(
            "robot inbox",
            OutputFormat::Json,
            InboxData {
                count: messages.len(),
                messages,
            },
        );
        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["count"], 2);
        // First message should be ack-overdue (highest priority)
        assert_eq!(v["messages"][0]["priority"], "ack-overdue");
    }

    #[test]
    fn test_timeline_event_serialization() {
        let event = TimelineEvent {
            seq: 1,
            timestamp: "2026-02-12T10:00:00Z".into(),
            kind: "message".into(),
            summary: "#42 BlueLake: Test subject".into(),
            source: "BlueLake".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"seq\":1"));
        assert!(json.contains("\"kind\":\"message\""));
    }

    #[test]
    fn test_timeline_envelope_toon() {
        #[derive(Serialize)]
        struct TimelineData {
            count: usize,
            events: Vec<TimelineEvent>,
        }
        let events = vec![
            TimelineEvent {
                seq: 1,
                timestamp: "2026-02-12T10:00:00Z".into(),
                kind: "message".into(),
                summary: "New message".into(),
                source: "AgentA".into(),
            },
            TimelineEvent {
                seq: 2,
                timestamp: "2026-02-12T10:05:00Z".into(),
                kind: "reservation".into(),
                summary: "Reserved src/**".into(),
                source: "AgentB".into(),
            },
        ];
        let env = RobotEnvelope::new(
            "robot timeline",
            OutputFormat::Toon,
            TimelineData {
                count: events.len(),
                events,
            },
        );
        let out = format_output(&env, OutputFormat::Toon).unwrap();
        assert!(out.contains("events[2]"));
        assert!(out.contains("message"));
        assert!(out.contains("reservation"));
    }

    #[test]
    fn test_overview_project_serialization() {
        let proj = OverviewProject {
            slug: "backend-api".into(),
            unread: 5,
            urgent: 1,
            ack_overdue: 0,
            reservations: 3,
        };
        let json = serde_json::to_string(&proj).unwrap();
        assert!(json.contains("\"slug\":\"backend-api\""));
        assert!(json.contains("\"unread\":5"));
    }

    #[test]
    fn test_overview_envelope_multi_project() {
        #[derive(Serialize)]
        struct OverviewData {
            project_count: usize,
            projects: Vec<OverviewProject>,
            total_unread: usize,
            total_urgent: usize,
            total_ack_overdue: usize,
        }
        let projects = vec![
            OverviewProject {
                slug: "proj1".into(),
                unread: 3,
                urgent: 1,
                ack_overdue: 0,
                reservations: 2,
            },
            OverviewProject {
                slug: "proj2".into(),
                unread: 2,
                urgent: 0,
                ack_overdue: 1,
                reservations: 0,
            },
        ];
        let env = RobotEnvelope::new(
            "robot overview",
            OutputFormat::Json,
            OverviewData {
                project_count: projects.len(),
                total_unread: 5,
                total_urgent: 1,
                total_ack_overdue: 1,
                projects,
            },
        );
        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["project_count"], 2);
        assert_eq!(v["total_unread"], 5);
    }

    #[test]
    fn test_overview_empty_projects() {
        #[derive(Serialize)]
        struct OverviewData {
            project_count: usize,
            projects: Vec<OverviewProject>,
        }
        let env = RobotEnvelope::new(
            "robot overview",
            OutputFormat::Json,
            OverviewData {
                project_count: 0,
                projects: vec![],
            },
        );
        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["project_count"], 0);
        assert!(v["projects"].as_array().unwrap().is_empty());
    }

    // ── Track 3: Context & Discovery Unit Tests ──────────────────────────────

    #[test]
    fn test_thread_message_serialization() {
        let msg = ThreadMessage {
            position: 1,
            from: "BlueLake".into(),
            to: "RedFox".into(),
            age: "2h".into(),
            importance: "high".into(),
            ack: "required".into(),
            subject: "[FEAT-1] Starting work".into(),
            body: Some("I'm starting on this feature.".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"position\":1"));
        assert!(json.contains("\"from\":\"BlueLake\""));
    }

    #[test]
    fn test_thread_data_serialization() {
        let data = ThreadData {
            thread_id: "FEAT-123".into(),
            subject: "Add authentication".into(),
            message_count: 5,
            participants: vec!["BlueLake".into(), "RedFox".into()],
            last_activity: "10m".into(),
            messages: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"thread_id\":\"FEAT-123\""));
        assert!(json.contains("\"message_count\":5"));
    }

    #[test]
    fn test_thread_envelope_markdown() {
        let data = ThreadData {
            thread_id: "BUG-42".into(),
            subject: "Fix login issue".into(),
            message_count: 2,
            participants: vec!["Alice".into(), "Bob".into()],
            last_activity: "5m".into(),
            messages: vec![ThreadMessage {
                position: 1,
                from: "Alice".into(),
                to: "Bob".into(),
                age: "1h".into(),
                importance: "normal".into(),
                ack: "none".into(),
                subject: "[BUG-42] Login failing".into(),
                body: Some("Users report login failures.".into()),
            }],
        };
        let env = RobotEnvelope::new("robot thread", OutputFormat::Markdown, data);
        let out = format_output(&env, OutputFormat::Markdown).unwrap();
        assert!(out.contains("BUG-42"));
        assert!(out.contains("Alice"));
    }

    #[test]
    fn test_search_result_serialization() {
        let result = SearchResult {
            id: 42,
            relevance: 0.95,
            from: "BlueLake".into(),
            subject: "JWT implementation".into(),
            thread: "AUTH-1".into(),
            snippet: "...using JWT with JWKS...".into(),
            age: "2h".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"relevance\":0.95"));
        assert!(json.contains("\"snippet\""));
    }

    #[test]
    fn test_search_data_facets() {
        let data = SearchData {
            query: "authentication".into(),
            total_results: 10,
            results: vec![],
            by_thread: vec![FacetEntry {
                value: "AUTH-1".into(),
                count: 5,
            }],
            by_agent: vec![FacetEntry {
                value: "BlueLake".into(),
                count: 6,
            }],
            by_importance: vec![FacetEntry {
                value: "high".into(),
                count: 3,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"query\":\"authentication\""));
        assert!(json.contains("\"by_thread\""));
        assert!(json.contains("AUTH-1"));
    }

    #[test]
    fn test_search_empty_results() {
        let data = SearchData {
            query: "nonexistent".into(),
            total_results: 0,
            results: vec![],
            by_thread: vec![],
            by_agent: vec![],
            by_importance: vec![],
        };
        let env = RobotEnvelope::new("robot search", OutputFormat::Json, data);
        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total_results"], 0);
        assert!(v["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_message_context_serialization() {
        let msg = MessageContext {
            id: 201,
            from: "BlueLake".into(),
            from_program: Some("claude-code".into()),
            from_model: Some("opus-4.5".into()),
            to: vec!["RedFox".into()],
            subject: "[FEAT-123] Implementation plan".into(),
            body: "Here is the plan...".into(),
            thread: "FEAT-123".into(),
            position: 3,
            total_in_thread: 8,
            importance: "high".into(),
            ack_status: "required".into(),
            created: "2026-02-12T10:00:00Z".into(),
            age: "2h".into(),
            attachments: vec![],
            previous: None,
            next: Some("RedFox: Sounds good".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"position\":3"));
        assert!(json.contains("\"from_program\":\"claude-code\""));
    }

    #[test]
    fn test_message_first_in_thread_track3() {
        let msg = MessageContext {
            id: 100,
            from: "Starter".into(),
            from_program: None,
            from_model: None,
            to: vec!["Team".into()],
            subject: "Kickoff".into(),
            body: "Starting project".into(),
            thread: "INIT-1".into(),
            position: 1,
            total_in_thread: 3,
            importance: "normal".into(),
            ack_status: "none".into(),
            created: "2026-02-11T10:00:00Z".into(),
            age: "1d".into(),
            attachments: vec![],
            previous: None, // No previous
            next: Some("Response".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        // previous is Option<String> - when None it's omitted, not null
        assert!(!json.contains("\"previous\""));
        assert!(json.contains("\"next\":\"Response\""));
    }

    #[test]
    fn test_message_last_in_thread_track3() {
        let msg = MessageContext {
            id: 300,
            from: "Closer".into(),
            from_program: None,
            from_model: None,
            to: vec!["Team".into()],
            subject: "Done".into(),
            body: "Task completed".into(),
            thread: "DONE-1".into(),
            position: 5,
            total_in_thread: 5,
            importance: "normal".into(),
            ack_status: "done".into(),
            created: "2026-02-12T12:00:00Z".into(),
            age: "10m".into(),
            attachments: vec![],
            previous: Some("Previous msg".into()),
            next: None, // No next
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"previous\":\"Previous msg\""));
        // next is Option<String> with skip_serializing_if - when None it's omitted
        assert!(!json.contains("\"next\""));
    }

    #[test]
    fn test_parse_since_micros_accepts_valid_iso8601() {
        let parsed = parse_since_micros("2026-02-01T12:00:00Z").expect("valid timestamp");
        assert!(parsed > 0);
    }

    #[test]
    fn test_parse_since_micros_rejects_invalid_timestamp() {
        let err = parse_since_micros("definitely-not-a-timestamp").expect_err("invalid timestamp");
        match err {
            CliError::InvalidArgument(message) => {
                assert!(message.contains("invalid --since timestamp"));
            }
            other => panic!("unexpected error variant: {other}"),
        }
    }

    #[test]
    fn test_navigate_result_projects() {
        let result = NavigateResult::Projects {
            projects: vec![ProjectRow {
                slug: "test".into(),
                path: "/data/test".into(),
                agents: 2,
                messages: 10,
                reservations: 1,
                created: "1d".into(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"slug\":\"test\""));
    }

    #[test]
    fn test_navigate_result_agents() {
        let result = NavigateResult::Agents {
            agents: vec![AgentRow {
                name: "TestAgent".into(),
                program: "claude-code".into(),
                model: "opus".into(),
                status: "active".into(),
                msg_count: 5,
                last_active: "5m".into(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"name\":\"TestAgent\""));
    }

    #[test]
    fn test_navigate_envelope_with_uri() {
        #[derive(Serialize)]
        struct NavigateData {
            uri: String,
            #[serde(flatten)]
            result: NavigateResult,
        }
        let env = RobotEnvelope::new(
            "robot navigate",
            OutputFormat::Json,
            NavigateData {
                uri: "resource://projects".into(),
                result: NavigateResult::Projects { projects: vec![] },
            },
        );
        let out = format_output(&env, OutputFormat::Json).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["uri"], "resource://projects");
    }

    #[test]
    fn test_navigate_should_use_canonical_resource_for_mailbox_with_commits() {
        assert!(navigate_should_use_canonical_resource(
            "resource://mailbox-with-commits/BlueLake?project=/tmp/proj"
        ));
    }

    #[test]
    fn test_build_navigate_from_canonical_resource_projects_honors_query_options() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir
            .path()
            .join("robot_navigate_canonical_projects.sqlite3");
        let storage_root = temp_dir.path().join("storage-root");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create projects");
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES
                (1, 'alpha-proj', '/tmp/alpha-proj', 1000),
                (2, 'beta-proj', '/tmp/beta-proj', 2000)",
            &empty,
        )
        .expect("insert projects");

        let database_url = format!("sqlite://{}", db_path.display());
        let storage_root_str = storage_root.display().to_string();
        let (result, scope) = with_navigate_resource_env(&database_url, &storage_root_str, || {
            build_navigate_from_canonical_resource(
                "resource://projects?contains=beta&limit=1",
                None,
            )
        })
        .expect("canonical navigate projects");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "projects");
                let projects = data.as_array().expect("projects array");
                assert_eq!(projects.len(), 1);
                assert_eq!(projects[0]["slug"], "beta-proj");
                assert_eq!(projects[0]["human_key"], "/tmp/beta-proj");
            }
            other => panic!("unexpected canonical navigate result: {other:?}"),
        }
        assert!(scope.is_none());
    }

    #[test]
    fn test_build_navigate_outbox_returns_sent_messages() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_outbox_test.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create projects");
        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                subject TEXT NOT NULL,
                thread_id TEXT,
                importance TEXT NOT NULL,
                ack_required INTEGER NOT NULL,
                created_ts INTEGER NOT NULL,
                body_md TEXT
            )",
            &empty,
        )
        .expect("create messages");
        conn.query_sync(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                read_ts INTEGER,
                ack_ts INTEGER
            )",
            &empty,
        )
        .expect("create message_recipients");

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES (1, 'proj', '/tmp/proj')",
            &empty,
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 1, 'Sender'), (2, 1, 'Recipient')",
            &empty,
        )
        .expect("insert agents");
        conn.query_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md)
             VALUES
                (10, 1, 1, 'sent by sender', 'th-1', 'normal', 0, 1000, 'body a'),
                (20, 1, 2, 'received by sender', 'th-2', 'normal', 0, 2000, 'body b')",
            &empty,
        )
        .expect("insert messages");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (1, 10, 2, 'to', NULL, NULL),
                (2, 20, 1, 'to', NULL, NULL)",
            &empty,
        )
        .expect("insert recipients");

        let (result, _action) =
            build_navigate(&conn, "resource://outbox/Sender", 1, "proj", None).expect("navigate");

        match result {
            NavigateResult::Inbox { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].id, 10);
                assert_eq!(entries[0].subject, "sent by sender");
                assert_eq!(entries[0].from, "Recipient");
                assert_eq!(entries[0].priority, "sent");
            }
            other => panic!("unexpected navigate result: {other:?}"),
        }
    }

    #[test]
    fn test_outbox_partial_ack_shows_fractional_status() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_outbox_ack_test.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL)",
            &empty,
        ).expect("create projects");
        conn.query_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL)",
            &empty,
        ).expect("create agents");
        conn.query_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, subject TEXT NOT NULL, thread_id TEXT, importance TEXT NOT NULL, ack_required INTEGER NOT NULL, created_ts INTEGER NOT NULL, body_md TEXT)",
            &empty,
        ).expect("create messages");
        conn.query_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts INTEGER, ack_ts INTEGER)",
            &empty,
        ).expect("create message_recipients");

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES (1, 'proj', '/tmp/proj')",
            &empty,
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 1, 'Sender'), (2, 1, 'RecipA'), (3, 1, 'RecipB')",
            &empty,
        ).expect("insert agents");

        let now = mcp_agent_mail_db::now_micros();
        // Message with ack_required and 2 recipients, only 1 acked
        conn.query_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md)
             VALUES (10, 1, 1, 'ack test', 'th-1', 'normal', 1, ?, 'body')",
            &[mcp_agent_mail_db::sqlmodel_core::Value::BigInt(now)],
        ).expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (1, 10, 2, 'to', NULL, 999),
                (2, 10, 3, 'to', NULL, NULL)",
            &empty,
        )
        .expect("insert recipients");

        let (result, _action) =
            build_navigate(&conn, "resource://outbox/Sender", 1, "proj", None).expect("navigate");

        match result {
            NavigateResult::Inbox { entries } => {
                assert_eq!(entries.len(), 1, "should have one outbox entry");
                assert_eq!(
                    entries[0].ack_status, "partial (1/2)",
                    "partial ack should show fractional status"
                );
            }
            other => panic!("unexpected navigate result: {other:?}"),
        }
    }

    #[test]
    fn test_build_navigate_honors_project_query_parameter() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_navigate_project_query.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create projects");
        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                subject TEXT NOT NULL,
                thread_id TEXT,
                importance TEXT NOT NULL,
                ack_required INTEGER NOT NULL,
                created_ts INTEGER NOT NULL,
                body_md TEXT
            )",
            &empty,
        )
        .expect("create messages");
        conn.query_sync(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                read_ts INTEGER,
                ack_ts INTEGER
            )",
            &empty,
        )
        .expect("create message_recipients");

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES \
             (1, 'wrong', '/tmp/wrong'), \
             (2, 'proj', '/tmp/proj')",
            &empty,
        )
        .expect("insert projects");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 2, 'Sender'), (2, 2, 'Recipient')",
            &empty,
        )
        .expect("insert agents");
        conn.query_sync(
            "INSERT INTO messages (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md)
             VALUES (10, 2, 1, 'sent by sender', 'th-1', 'normal', 0, 1000, 'body a')",
            &empty,
        )
        .expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (1, 10, 2, 'to', NULL, NULL)",
            &empty,
        )
        .expect("insert recipient");

        let (result, _action) = build_navigate(
            &conn,
            "resource://outbox/Sender?project=/tmp/proj",
            1,
            "wrong",
            None,
        )
        .expect("navigate");

        match result {
            NavigateResult::Inbox { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].id, 10);
                assert_eq!(entries[0].subject, "sent by sender");
            }
            other => panic!("unexpected navigate result: {other:?}"),
        }
    }

    #[test]
    fn test_build_navigate_supports_project_keys_in_path_segments() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_navigate_project_key.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create projects");
        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                model TEXT NOT NULL,
                task_description TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                contact_policy TEXT,
                attachments_policy TEXT,
                last_active_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                sender_id INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create messages");
        conn.query_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                path_pattern TEXT NOT NULL,
                \"exclusive\" INTEGER NOT NULL,
                reason TEXT,
                created_ts INTEGER NOT NULL,
                expires_ts INTEGER NOT NULL,
                released_ts INTEGER
            )",
            &empty,
        )
        .expect("create file reservations");

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'proj', '/tmp/proj', 1000)",
            &empty,
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (
                id, project_id, name, program, model, task_description,
                created_at, updated_at, contact_policy, attachments_policy, last_active_ts
             ) VALUES (
                1, 1, 'Sender', 'codex-cli', 'gpt-5', 'task',
                1000, 1000, 'auto', 'inline', 1000
             )",
            &empty,
        )
        .expect("insert agent");
        conn.query_sync(
            "INSERT INTO file_reservations (
                id, project_id, agent_id, path_pattern, \"exclusive\",
                reason, created_ts, expires_ts, released_ts
             ) VALUES (
                1, 1, 1, 'src/**', 1, 'test', 1000, 2000, NULL
             )",
            &empty,
        )
        .expect("insert reservation");

        let encoded_project_key = "%2Ftmp%2Fproj";

        let (project_result, project_scope) = build_navigate(
            &conn,
            &format!("resource://project/{encoded_project_key}"),
            1,
            "proj",
            None,
        )
        .expect("navigate project by human key");
        match project_result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "project");
                assert_eq!(data["slug"], "proj");
                assert_eq!(data["path"], "/tmp/proj");
            }
            other => panic!("unexpected project result: {other:?}"),
        }
        assert_eq!(project_scope.as_deref(), Some("proj"));

        let (agents_result, agents_scope) = build_navigate(
            &conn,
            &format!("resource://agents/{encoded_project_key}"),
            99,
            "wrong",
            None,
        )
        .expect("navigate agents by human key");
        match agents_result {
            NavigateResult::Agents { agents } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].name, "Sender");
            }
            other => panic!("unexpected agents result: {other:?}"),
        }
        assert_eq!(agents_scope.as_deref(), Some("proj"));

        let (agents_query_result, agents_query_scope) = build_navigate(
            &conn,
            &format!("resource://agents?project={encoded_project_key}"),
            99,
            "wrong",
            None,
        )
        .expect("navigate agents by query-scoped project");
        match agents_query_result {
            NavigateResult::Agents { agents } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].name, "Sender");
            }
            other => panic!("unexpected agents query result: {other:?}"),
        }
        assert_eq!(agents_query_scope.as_deref(), Some("proj"));

        let (reservations_result, reservations_scope) = build_navigate(
            &conn,
            &format!("resource://file_reservations/{encoded_project_key}"),
            99,
            "wrong",
            None,
        )
        .expect("navigate reservations by human key");
        match reservations_result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "file_reservations");
                assert_eq!(data["reservations"].as_array().map(Vec::len), Some(1));
            }
            other => panic!("unexpected reservations result: {other:?}"),
        }
        assert_eq!(reservations_scope.as_deref(), Some("proj"));

        let (reservations_query_result, reservations_query_scope) = build_navigate(
            &conn,
            &format!("resource://file_reservations?project={encoded_project_key}"),
            99,
            "wrong",
            None,
        )
        .expect("navigate query-scoped reservations");
        match reservations_query_result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "file_reservations");
                assert_eq!(data["reservations"].as_array().map(Vec::len), Some(1));
            }
            other => panic!("unexpected query reservations result: {other:?}"),
        }
        assert_eq!(reservations_query_scope.as_deref(), Some("proj"));

        let (reservations_alias_result, reservations_alias_scope) = build_navigate(
            &conn,
            &format!("resource://reservations?project={encoded_project_key}"),
            99,
            "wrong",
            None,
        )
        .expect("navigate reservations alias");
        match reservations_alias_result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "file_reservations");
                assert_eq!(data["reservations"].as_array().map(Vec::len), Some(1));
            }
            other => panic!("unexpected reservations alias result: {other:?}"),
        }
        assert_eq!(reservations_alias_scope.as_deref(), Some("proj"));

        conn.query_sync(
            "UPDATE file_reservations SET released_ts = 3000 WHERE id = 1",
            &empty,
        )
        .expect("release reservation");

        let (active_only_result, _) = build_navigate(
            &conn,
            &format!("resource://file_reservations/{encoded_project_key}"),
            99,
            "wrong",
            None,
        )
        .expect("navigate active-only reservations");
        match active_only_result {
            NavigateResult::Generic { data, .. } => {
                assert_eq!(data["reservations"].as_array().map(Vec::len), Some(0));
            }
            other => panic!("unexpected active-only reservations result: {other:?}"),
        }

        let (all_result, _) = build_navigate(
            &conn,
            &format!("resource://file_reservations/{encoded_project_key}?active_only=false"),
            99,
            "wrong",
            None,
        )
        .expect("navigate all reservations");
        match all_result {
            NavigateResult::Generic { data, .. } => {
                assert_eq!(data["reservations"].as_array().map(Vec::len), Some(1));
                assert_eq!(data["reservations"][0]["released"], true);
            }
            other => panic!("unexpected all reservations result: {other:?}"),
        }
    }

    #[test]
    fn test_build_navigate_file_reservations_uses_release_ledger_history() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir
            .path()
            .join("robot_navigate_reservation_release_ledger.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create projects");
        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                model TEXT NOT NULL,
                task_description TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                contact_policy TEXT,
                attachments_policy TEXT,
                last_active_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                path_pattern TEXT NOT NULL,
                \"exclusive\" INTEGER NOT NULL,
                reason TEXT,
                created_ts INTEGER NOT NULL,
                expires_ts INTEGER NOT NULL,
                released_ts INTEGER
            )",
            &empty,
        )
        .expect("create file reservations");
        conn.query_sync(
            "CREATE TABLE file_reservation_releases (
                reservation_id INTEGER PRIMARY KEY,
                released_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create file reservation releases");

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'proj', '/tmp/proj', 1000)",
            &empty,
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (
                id, project_id, name, program, model, task_description,
                created_at, updated_at, contact_policy, attachments_policy, last_active_ts
             ) VALUES (
                1, 1, 'Sender', 'codex-cli', 'gpt-5', 'task',
                1000, 1000, 'auto', 'inline', 1000
             )",
            &empty,
        )
        .expect("insert agent");
        conn.query_sync(
            "INSERT INTO file_reservations (
                id, project_id, agent_id, path_pattern, \"exclusive\",
                reason, created_ts, expires_ts, released_ts
             ) VALUES (
                1, 1, 1, 'src/**', 1, 'test', 1000, 2000, NULL
             )",
            &empty,
        )
        .expect("insert reservation");
        conn.query_sync(
            "INSERT INTO file_reservation_releases (reservation_id, released_ts)
             VALUES (1, 3000)",
            &empty,
        )
        .expect("insert reservation release");

        let (active_only_result, _) = build_navigate(
            &conn,
            "resource://file_reservations/%2Ftmp%2Fproj",
            99,
            "wrong",
            None,
        )
        .expect("navigate active-only reservations");
        match active_only_result {
            NavigateResult::Generic { data, .. } => {
                assert_eq!(data["reservations"].as_array().map(Vec::len), Some(0));
            }
            other => panic!("unexpected active-only reservations result: {other:?}"),
        }

        let (all_result, _) = build_navigate(
            &conn,
            "resource://file_reservations/%2Ftmp%2Fproj?active_only=false",
            99,
            "wrong",
            None,
        )
        .expect("navigate all reservations");
        match all_result {
            NavigateResult::Generic { data, .. } => {
                assert_eq!(data["reservations"].as_array().map(Vec::len), Some(1));
                assert_eq!(data["reservations"][0]["released"], true);
            }
            other => panic!("unexpected all reservations result: {other:?}"),
        }
    }

    #[test]
    fn active_reservation_helpers_omit_legacy_column_when_schema_lacks_it() {
        let active_with_ledger = active_reservation_filter_sql(true, false, "fr", "rr");
        assert_eq!(active_with_ledger, "rr.reservation_id IS NULL");

        let released_with_ledger = reservation_released_ts_sql(true, false, "fr", "rr");
        assert_eq!(released_with_ledger, "rr.released_ts");

        let active_without_ledger = active_reservation_filter_sql(false, false, "fr", "rr");
        assert_eq!(active_without_ledger, "1 = 1");

        let released_without_ledger = reservation_released_ts_sql(false, false, "fr", "rr");
        assert_eq!(released_without_ledger, "NULL");
    }

    #[test]
    fn test_build_navigate_active_only_file_reservations_supports_release_ledger_without_legacy_released_ts_column()
     {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir
            .path()
            .join("robot_navigate_reservation_active_only_release_ledger.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create projects");
        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                model TEXT NOT NULL,
                task_description TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                contact_policy TEXT,
                attachments_policy TEXT,
                last_active_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                path_pattern TEXT NOT NULL,
                \"exclusive\" INTEGER NOT NULL,
                reason TEXT,
                created_ts INTEGER NOT NULL,
                expires_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create file reservations");
        conn.query_sync(
            "CREATE TABLE file_reservation_releases (
                reservation_id INTEGER PRIMARY KEY,
                released_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create file reservation releases");

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'proj', '/tmp/proj', 1000)",
            &empty,
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (
                id, project_id, name, program, model, task_description,
                created_at, updated_at, contact_policy, attachments_policy, last_active_ts
             ) VALUES (
                1, 1, 'Sender', 'codex-cli', 'gpt-5', 'task',
                1000, 1000, 'auto', 'inline', 1000
             )",
            &empty,
        )
        .expect("insert agent");

        let now = mcp_agent_mail_db::now_micros();
        let active_expiry = now + 3_600_000_000;
        conn.query_sync(
            "INSERT INTO file_reservations (
                id, project_id, agent_id, path_pattern, \"exclusive\",
                reason, created_ts, expires_ts
             ) VALUES (
                1, 1, 1, 'src/**', 1, 'active', 1000, ?
             )",
            &[mcp_agent_mail_db::sqlmodel_core::Value::BigInt(
                active_expiry,
            )],
        )
        .expect("insert active reservation");

        let (active_only_result, active_scope) = build_navigate(
            &conn,
            "resource://file_reservations/%2Ftmp%2Fproj",
            99,
            "wrong",
            None,
        )
        .expect("navigate active-only reservations with release ledger");

        match active_only_result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "file_reservations");
                let reservations = data["reservations"].as_array().expect("reservations array");
                assert_eq!(reservations.len(), 1);
                assert_eq!(reservations[0]["path"], "src/**");
                assert_eq!(reservations[0]["released"], false);
            }
            other => panic!("unexpected active-only reservations result: {other:?}"),
        }
        assert_eq!(active_scope.as_deref(), Some("proj"));
    }

    #[test]
    fn test_build_navigate_preserves_literal_plus_in_path_segments() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_navigate_project_plus.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create projects");

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'proj-plus', '/tmp/proj+plus', 1000)",
            &empty,
        )
        .expect("insert project");

        let (result, project_scope) = build_navigate(
            &conn,
            "resource://project/%2Ftmp%2Fproj+plus",
            99,
            "wrong",
            None,
        )
        .expect("navigate project by plus-containing human key");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "project");
                assert_eq!(data["slug"], "proj-plus");
                assert_eq!(data["path"], "/tmp/proj+plus");
            }
            other => panic!("unexpected project result: {other:?}"),
        }
        assert_eq!(project_scope.as_deref(), Some("proj-plus"));
    }

    #[test]
    fn test_build_navigate_supports_config_environment_resource() {
        let conn = mcp_agent_mail_db::DbConn::open_memory().expect("open memory db");
        let (result, scope) =
            build_navigate(&conn, "resource://config/environment", 99, "wrong", None)
                .expect("navigate config environment");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "config/environment");
                assert!(data["environment"].is_string());
                assert!(data["http"]["path"].is_string());
            }
            other => panic!("unexpected config/environment result: {other:?}"),
        }
        assert!(scope.is_none());
    }

    #[test]
    fn test_build_navigate_supports_identity_resource() {
        let conn = mcp_agent_mail_db::DbConn::open_memory().expect("open memory db");
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let project_ref = temp_dir.path().join("identity-proj");
        std::fs::create_dir_all(&project_ref).expect("create identity project dir");
        let encoded = project_ref.to_string_lossy().replace('/', "%2F");

        let (result, scope) = build_navigate(
            &conn,
            &format!("resource://identity/{encoded}"),
            99,
            "wrong",
            None,
        )
        .expect("navigate identity");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "identity");
                assert_eq!(data["human_key"], project_ref.to_string_lossy().as_ref());
                assert!(data["slug"].is_string());
            }
            other => panic!("unexpected identity result: {other:?}"),
        }
        assert!(scope.is_none());
    }

    #[test]
    fn test_build_navigate_supports_urgent_unread_view() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(301),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Urgent unread".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("VIEW-URGENT".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("urgent".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
            ],
        )
        .expect("insert urgent message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (301, 301, 2, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert urgent recipient");

        let (result, scope) =
            build_navigate(&conn, "resource://views/urgent-unread/Bob", 1, "proj", None)
                .expect("navigate urgent-unread");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "views/urgent-unread");
                assert_eq!(data["project"], "/tmp/proj");
                assert_eq!(data["count"], 1);
                assert_eq!(data["messages"][0]["subject"], "Urgent unread");
                assert_eq!(data["messages"][0]["from"], "Alice");
                assert_eq!(data["messages"][0]["ack_required"], true);
            }
            other => panic!("unexpected urgent-unread result: {other:?}"),
        }
        assert_eq!(scope.as_deref(), Some("proj"));
    }

    #[test]
    fn test_build_navigate_supports_ack_overdue_view_with_ttl_query() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros() - 95 * 60 * 1_000_000;
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(302),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Ack overdue".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("VIEW-ACK".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
            ],
        )
        .expect("insert ack overdue message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (302, 302, 2, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert ack overdue recipient");

        let (result, scope) = build_navigate(
            &conn,
            "resource://views/ack-overdue/Bob?ttl_minutes=90",
            1,
            "proj",
            None,
        )
        .expect("navigate ack-overdue");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "views/ack-overdue");
                assert_eq!(data["project"], "/tmp/proj");
                assert!(data.get("ttl_minutes").is_none());
                assert_eq!(data["count"], 1);
                assert_eq!(data["messages"][0]["subject"], "Ack overdue");
            }
            other => panic!("unexpected ack-overdue result: {other:?}"),
        }
        assert_eq!(scope.as_deref(), Some("proj"));
    }

    #[test]
    fn test_build_navigate_supports_tooling_metrics_resource() {
        let conn = mcp_agent_mail_db::DbConn::open_memory().expect("open memory db");
        let (result, scope) =
            build_navigate(&conn, "resource://tooling/metrics", 99, "wrong", None)
                .expect("navigate tooling metrics");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "tooling/metrics");
                assert!(data["health_level"].is_string());
                assert!(data["tools"].is_array());
            }
            other => panic!("unexpected tooling metrics result: {other:?}"),
        }
        assert!(scope.is_none());
    }

    #[test]
    fn test_build_navigate_without_db_supports_tooling_metrics_resource() {
        let (result, scope) = build_navigate_without_db("resource://tooling/metrics")
            .expect("navigate tooling without db")
            .expect("tooling metrics should bypass db");

        match result {
            NavigateResult::Generic {
                resource_type,
                data,
            } => {
                assert_eq!(resource_type, "tooling/metrics");
                assert!(data["health_level"].is_string());
                assert!(data["tools"].is_array());
            }
            other => panic!("unexpected tooling metrics result: {other:?}"),
        }
        assert!(scope.is_none());
    }

    #[test]
    fn test_build_navigate_without_db_returns_none_for_db_backed_resource() {
        let result =
            build_navigate_without_db("resource://thread/demo-thread").expect("navigate helper");
        assert!(
            result.is_none(),
            "thread resources still require DB-backed navigate"
        );
    }

    #[test]
    fn test_build_navigate_tooling_locks_data_matches_resource_shape() {
        let current_pid = u64::from(std::process::id());
        let data = build_navigate_tooling_locks_data(serde_json::json!({
            "locks": [
                {
                    "path": "/tmp/archive/projects/proj-a/lock.json",
                    "owner": {
                        "pid": current_pid,
                        "created_ts": 1_735_689_600.5
                    }
                },
                {
                    "path": "/tmp/archive/projects/proj-b/lock.json",
                    "owner": {
                        "created_ts": 1_735_689_600.0
                    }
                }
            ]
        }));

        let locks = data["locks"].as_array().expect("locks array");
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0]["project_slug"], "proj-a");
        assert_eq!(locks[0]["holder"], format!("pid:{current_pid}"));
        assert!(locks[0]["acquired_ts"].is_string());
        assert_eq!(data["summary"]["total"], 1);
        assert_eq!(data["summary"]["metadata_missing"], 1);
    }

    #[test]
    fn test_navigate_global_resources_do_not_force_project_meta() {
        let conn = mcp_agent_mail_db::DbConn::open_memory().expect("open memory db");
        let (result, resolved_project) =
            build_navigate(&conn, "resource://config/environment", 99, "proj", None)
                .expect("navigate config environment");

        #[derive(Serialize)]
        struct NavigateData {
            uri: String,
            #[serde(flatten)]
            result: NavigateResult,
        }

        let mut env = RobotEnvelope::new(
            "robot navigate",
            OutputFormat::Json,
            NavigateData {
                uri: "resource://config/environment".to_string(),
                result,
            },
        );
        env._meta.project = resolved_project;

        let output = format_output(&env, OutputFormat::Json).expect("format output");
        let value: Value = serde_json::from_str(&output).expect("json output");
        assert!(value["_meta"].get("project").is_none());
    }

    #[test]
    fn parse_resource_limit_caps_large_values() {
        let value = "50000".to_string();
        assert_eq!(
            parse_resource_limit(Some(&value), 50),
            RESOURCE_URI_LIMIT_MAX
        );
    }

    #[test]
    fn build_agents_deduplicates_case_insensitive_names() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_agents_dedupe.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                model TEXT NOT NULL,
                last_active_ts INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create agents table");
        conn.query_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                sender_id INTEGER NOT NULL
            )",
            &empty,
        )
        .expect("create messages table");

        conn.query_sync(
            "INSERT INTO agents (id, project_id, name, program, model, last_active_ts)
             VALUES
                (1, 1, 'RubyPrairie', 'claude-code', 'opus-4.6', 1000),
                (2, 1, 'rubyprairie', 'codex-cli', 'gpt-5', 2000),
                (3, 1, 'JadePine', 'codex-cli', 'gpt-5', 1500)",
            &empty,
        )
        .expect("insert agents");
        conn.query_sync(
            "INSERT INTO messages (id, sender_id) VALUES (10, 2), (20, 2), (30, 3)",
            &empty,
        )
        .expect("insert messages");

        let rows = build_agents(&conn, 1, false, None).expect("build agents");

        assert_eq!(rows.len(), 2, "duplicate logical agent should be collapsed");
        assert_eq!(rows[0].name, "rubyprairie");
        assert_eq!(rows[0].program, "codex-cli");
        assert_eq!(rows[0].msg_count, 2);
    }

    #[test]
    fn resolve_agent_id_finds_case_insensitive_agent() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_agent_resolve_case.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create agents table");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name) VALUES (1, 1, 'BlueLake')",
            &empty,
        )
        .expect("insert agent");

        let resolved =
            resolve_agent_id(&conn, 1, Some("bluelake")).expect("case-insensitive resolve");
        assert_eq!(resolved, Some((1, "BlueLake".to_string())));
    }

    #[test]
    fn resolve_agent_id_preserves_ambiguous_agent_error() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir
            .path()
            .join("robot_agent_resolve_ambiguous.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create agents table");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name)
             VALUES (1, 1, 'BlueLake'), (2, 1, 'bluelake')",
            &empty,
        )
        .expect("insert duplicate logical agents");

        let err = resolve_agent_id(&conn, 1, Some("BlUeLaKe"))
            .expect_err("ambiguous legacy case-duplicate rows must error");
        assert!(
            err.to_string().contains("ambiguous agent name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_robot_scope_falls_back_to_archive_snapshot_for_missing_explicit_agent() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let local_db_path = temp_dir.path().join("robot_scope_fallback.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(local_db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create projects table");
        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create agents table");
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key)
             VALUES (1, 'demo-project', '/tmp/demo-project')",
            &empty,
        )
        .expect("insert local project");

        let storage_root = tempfile::tempdir().expect("storage root");
        let project_dir = storage_root.path().join("projects").join("demo-project");
        let agent_dir = project_dir.join("agents").join("CoralMarsh");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"demo-project","human_key":"/tmp/demo-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{
                "name": "CoralMarsh",
                "program": "codex-cli",
                "model": "gpt-5",
                "task_description": "archive snapshot",
                "inception_ts": "2026-03-13T21:21:02Z",
                "last_active_ts": "2026-03-13T21:21:02Z"
            }"#,
        )
        .expect("write agent profile");

        let err = resolve_robot_scope_with_handle(
            RobotDbHandle::from_conn(conn),
            Some("/tmp/demo-project"),
            Some("coralmarsh"),
        )
        .err()
        .expect("local db should still miss CoralMarsh");
        assert!(
            should_try_archive_snapshot(
                &err,
                Some("/tmp/demo-project"),
                Some("coralmarsh"),
                storage_root.path(),
            ),
            "archive snapshot should be eligible when the profile exists in the archive"
        );

        let local_conn = mcp_agent_mail_db::DbConn::open_file(local_db_path.display().to_string())
            .expect("reopen sqlite db");
        let scope = resolve_robot_scope_with_archive_fallback(
            RobotDbHandle::from_conn(local_conn),
            storage_root.path(),
            Some("/tmp/demo-project"),
            Some("coralmarsh"),
        )
        .expect("archive fallback should resolve CoralMarsh");

        assert_eq!(scope.project_slug, "demo-project");
        assert_eq!(
            scope.agent.as_ref().map(|(_, name)| name.as_str()),
            Some("CoralMarsh")
        );
    }

    #[test]
    fn soften_implicit_missing_agent_error_drops_missing_env_agent() {
        let result = soften_implicit_missing_agent_error(
            None,
            Err(CliError::InvalidArgument(
                "agent not found: GhostAgent".to_string(),
            )),
        )
        .expect("missing implicit agent should be ignored");
        assert!(result.is_none());
    }

    #[test]
    fn soften_implicit_missing_agent_error_preserves_explicit_missing_agent() {
        let err = soften_implicit_missing_agent_error(
            Some("GhostAgent"),
            Err(CliError::InvalidArgument(
                "agent not found: GhostAgent".to_string(),
            )),
        )
        .expect_err("explicit missing agent should stay an error");
        assert!(
            err.to_string().contains("agent not found: GhostAgent"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn soften_implicit_missing_agent_error_preserves_ambiguous_agent_error() {
        let err = soften_implicit_missing_agent_error(
            None,
            Err(CliError::InvalidArgument(
                "ambiguous agent name 'BlueLake' in project 1".to_string(),
            )),
        )
        .expect_err("ambiguous env agent should stay an error");
        assert!(err.to_string().contains("ambiguous agent name"));
    }

    fn setup_robot_thread_message_test_db() -> (tempfile::TempDir, mcp_agent_mail_db::DbConn) {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_thread_message_test.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create projects");
        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT,
                model TEXT
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                subject TEXT NOT NULL,
                thread_id TEXT,
                importance TEXT NOT NULL,
                ack_required INTEGER NOT NULL,
                created_ts INTEGER NOT NULL,
                body_md TEXT NOT NULL,
                attachments TEXT
            )",
            &empty,
        )
        .expect("create messages");
        conn.query_sync(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                read_ts INTEGER,
                ack_ts INTEGER
            )",
            &empty,
        )
        .expect("create recipients");
        conn.query_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                path_pattern TEXT NOT NULL,
                exclusive INTEGER NOT NULL,
                reason TEXT NOT NULL,
                created_ts INTEGER NOT NULL,
                expires_ts INTEGER NOT NULL,
                released_ts INTEGER
            )",
            &empty,
        )
        .expect("create reservations");
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES (1, 'proj', '/tmp/proj')",
            &empty,
        )
        .expect("insert project");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name, program, model)
             VALUES
                (1, 1, 'Alice', 'claude-code', 'opus'),
                (2, 1, 'Bob', 'codex-cli', 'gpt-5'),
                (3, 1, 'Carol', 'codex-cli', 'gpt-5')",
            &empty,
        )
        .expect("insert agents");

        (temp_dir, conn)
    }

    struct RobotCwdGuard {
        original: PathBuf,
    }

    impl RobotCwdGuard {
        fn chdir(path: &Path) -> Self {
            let original = std::env::current_dir().expect("get cwd");
            std::env::set_current_dir(path).expect("set cwd");
            Self { original }
        }
    }

    impl Drop for RobotCwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn test_build_thread_partial_ack_not_marked_done() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(100),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Ack test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("ACK-THREAD".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
            ],
        )
        .expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (1, 100, 2, 'to', NULL, 123456789),
                (2, 100, 3, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let thread = build_thread(&conn, 1, "ACK-THREAD", Some(10), None, false)
            .expect("build thread should succeed");
        assert_eq!(thread.message_count, 1);
        assert_eq!(thread.messages[0].ack, "partial (1/2)");
    }

    #[test]
    fn test_build_thread_collects_sender_and_recipient_participants() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES
                (110, 1, 1, 'First', 'PARTICIPANTS', 'normal', 0, ?, 'body', '[]'),
                (111, 1, 2, 'Second', 'PARTICIPANTS', 'normal', 0, ?, 'body', '[]')",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts + 1),
            ],
        )
        .expect("insert messages");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (10, 110, 2, 'to', NULL, NULL),
                (11, 110, 3, 'cc', NULL, NULL),
                (12, 111, 1, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let thread =
            build_thread(&conn, 1, "PARTICIPANTS", Some(10), None, false).expect("build thread");
        assert_eq!(
            thread.participants,
            vec!["Alice".to_string(), "Bob".to_string(), "Carol".to_string()]
        );
    }

    #[test]
    fn test_build_thread_to_field_prefers_primary_recipients() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (112, 1, 1, 'Recipient kinds', 'RECIPIENT-KINDS', 'normal', 0, ?, 'body', '[]')",
            &[mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts)],
        )
        .expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (20, 112, 2, 'to', NULL, NULL),
                (21, 112, 3, 'cc', NULL, NULL),
                (22, 112, 1, 'bcc', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let thread =
            build_thread(&conn, 1, "RECIPIENT-KINDS", Some(10), None, false).expect("build thread");
        assert_eq!(thread.messages.len(), 1);
        assert_eq!(thread.messages[0].to, "Bob");
    }

    #[test]
    fn test_build_thread_missing_thread_errors() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let err = build_thread(&conn, 1, "does-not-exist", Some(10), None, false)
            .expect_err("missing thread should error");
        assert!(matches!(err, CliError::InvalidArgument(msg) if msg.contains("does-not-exist")));
    }

    #[test]
    fn test_build_inbox_root_message_uses_numeric_thread_ref() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (120, 1, 1, 'Root inbox message', NULL, 'normal', 0, 10, 'body', '[]')",
            &[],
        )
        .expect("insert inbox message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (120, 120, 2, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert inbox recipient");

        let result = build_inbox(
            &conn, 1, "proj", 2, "Bob", false, false, true, false, 20, false,
        )
        .expect("build inbox");

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].thread, "120");
        assert_eq!(
            result.actions.last().map(String::as_str),
            Some("am robot thread --project proj 120")
        );
    }

    #[test]
    fn find_project_for_cwd_walks_ancestor_directories() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("workspace");
        let nested = root.join("crates/mcp-agent-mail-cli");
        std::fs::create_dir_all(&nested).expect("create nested workspace path");

        let db_path = temp_dir.path().join("project_lookup.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        conn.query_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL
            )",
            &[],
        )
        .expect("create projects");
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key) VALUES (?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("workspace".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text(
                    root.canonicalize()
                        .expect("canonicalize root")
                        .to_string_lossy()
                        .replace('\\', "/"),
                ),
            ],
        )
        .expect("insert project");

        let _cwd = RobotCwdGuard::chdir(&nested);
        let (project_id, slug) =
            find_project_for_cwd(&conn).expect("resolve project from ancestor");
        assert_eq!(project_id, 1);
        assert_eq!(slug, "workspace");
    }

    #[test]
    fn handle_robot_attachments_falls_back_to_best_effort_open_under_reserved_lock() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_attachments_lock.sqlite3");
        let db_url = format!("sqlite:///{}", db_path.display());
        let db_path_str = db_path.to_string_lossy().into_owned();

        let seed_conn = mcp_agent_mail_db::DbConn::open_file(&db_path_str).expect("open seed db");
        let attachments = serde_json::json!([
            {
                "name": "report.txt",
                "size_bytes": 128,
                "media_type": "text/plain"
            }
        ])
        .to_string();
        for stmt in [
            "PRAGMA foreign_keys = OFF",
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at DATETIME NOT NULL)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL, inception_ts DATETIME NOT NULL, last_active_ts DATETIME NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto')",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL, ack_required INTEGER NOT NULL, created_ts DATETIME NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts DATETIME, ack_ts DATETIME, PRIMARY KEY (message_id, agent_id, kind))",
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'robot-lock', '/tmp/robot-lock', '2026-03-12 11:00:00')",
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (1, 1, 'Sender', 'codex-cli', 'test', 'robot', '2026-03-12 11:00:01', '2026-03-12 11:00:02', 'auto', 'auto')",
        ] {
            seed_conn.execute_raw(stmt).expect("seed statement");
        }
        seed_conn
            .query_sync(
                "INSERT INTO messages
                 (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text("br-lock".to_string()),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text(
                        "Attachment message".to_string(),
                    ),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(0),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1_741_779_600_000_000),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text(attachments),
                ],
            )
            .expect("insert message");
        drop(seed_conn);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let lock_path = db_path_str.clone();
        let lock_thread = std::thread::spawn(move || {
            let lock_conn =
                sqlmodel_sqlite::SqliteConnection::open_file(&lock_path).expect("open lock db");
            lock_conn
                .execute_raw("PRAGMA busy_timeout = 1;")
                .expect("set lock busy_timeout");
            lock_conn
                .execute_raw("BEGIN IMMEDIATE")
                .expect("hold reserved sqlite lock");
            ready_tx.send(()).expect("signal lock ready");
            release_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("wait for release");
            lock_conn
                .execute_raw("ROLLBACK")
                .expect("release sqlite lock");
        });

        ready_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("wait for lock thread");

        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let robot_result = with_navigate_resource_env(
            &db_url,
            temp_dir.path().to_string_lossy().as_ref(),
            || -> Result<(), String> {
                let handle_thread = std::thread::spawn(move || {
                    let args = RobotArgs {
                        format: Some(OutputFormat::Json),
                        project: Some("robot-lock".to_string()),
                        agent: None,
                        command: RobotSubcommand::Attachments,
                    };
                    let result = handle_robot(args);
                    result_tx.send(result).expect("send robot result");
                });

                let result = match result_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                    Ok(result) => result,
                    Err(err) => {
                        return Err(format!(
                            "robot attachments should not block on canonical init under reserved lock: {err}"
                        ));
                    }
                };
                handle_thread
                    .join()
                    .map_err(|_| "join robot thread".to_string())?;

                result.map_err(|err| {
                    format!("robot attachments should succeed via best-effort fallback: {err}")
                })
            },
        );

        release_tx.send(()).expect("release lock thread");
        lock_thread.join().expect("join lock thread");
        assert!(robot_result.is_ok(), "{robot_result:?}");
    }

    #[test]
    fn build_reservations_scopes_conflicts_and_expiring_to_selected_agent() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let now_us = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO file_reservations
             (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
             VALUES
                (?, 1, 1, 'src/auth/**', 1, 'a', 0, ?, NULL),
                (?, 1, 2, 'src/auth/jwt.rs', 1, 'b', 0, ?, NULL),
                (?, 1, 1, 'docs/**', 1, 'c', 0, ?, NULL),
                (?, 1, 3, 'docs/readme.md', 1, 'd', 0, ?, NULL)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(now_us + 3_600_000_000),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(2),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(now_us + 300_000_000),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(3),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(now_us + 3_600_000_000),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(4),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(now_us + 300_000_000),
            ],
        )
        .expect("insert reservations");

        let (data, actions) = build_reservations(
            &conn,
            1,
            "proj",
            Some((2, "Bob".to_string())),
            false,
            false,
            Some(10),
        )
        .expect("build reservations");

        assert_eq!(data.all_active.len(), 1);
        assert_eq!(data.all_active[0].agent.as_deref(), Some("Bob"));
        assert_eq!(data.all_active[0].path, "src/auth/jwt.rs");
        assert_eq!(data.conflicts.len(), 1);
        assert_eq!(data.conflicts[0].agent_a, "Alice");
        assert_eq!(data.conflicts[0].agent_b, "Bob");
        assert_eq!(data.expiring_soon.len(), 1);
        assert_eq!(data.expiring_soon[0].agent.as_deref(), Some("Bob"));
        assert_eq!(
            actions,
            vec![
                "am file_reservations renew proj Bob --paths src/auth/jwt.rs --extend-seconds 3600"
                    .to_string()
            ]
        );
    }

    #[test]
    fn test_build_outbox_root_message_uses_numeric_thread_ref() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (121, 1, 1, 'Root outbox message', NULL, 'normal', 0, 10, 'body', '[]')",
            &[],
        )
        .expect("insert outbox message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (121, 121, 2, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert outbox recipient");

        let entries = build_outbox_entries(&conn, 1, 1, 20, false).expect("build outbox");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].thread, "121");
    }

    #[test]
    fn test_build_search_invalid_importance_errors() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let err = build_search(&conn, 1, "auth", None, Some("totally-invalid"), None, 20)
            .expect_err("invalid importance should error");

        assert!(
            matches!(err, CliError::InvalidArgument(msg) if msg.contains("invalid importance filter"))
        );
    }

    #[test]
    fn test_build_message_attachment_parser_handles_current_meta_schema() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        let attachments = serde_json::json!([
            {
                "type": "file",
                "media_type": "image/webp",
                "bytes": 1234,
                "path": "attachments/_webp/abc123.webp"
            },
            {
                "name": "notes.txt",
                "content_type": "text/plain",
                "size": "98"
            }
        ])
        .to_string();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(101),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Attachment test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("ATT-THREAD".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(0),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text(attachments),
            ],
        )
        .expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (3, 101, 2, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipient");

        let message = build_message(&conn, 1, 101).expect("build message should succeed");
        assert_eq!(message.attachments.len(), 2);
        assert_eq!(message.attachments[0].name, "abc123.webp");
        assert_eq!(message.attachments[0].size, "1234");
        assert_eq!(message.attachments[0].mime_type, "image/webp");
        assert_eq!(message.attachments[1].name, "notes.txt");
        assert_eq!(message.attachments[1].size, "98");
        assert_eq!(message.attachments[1].mime_type, "text/plain");
    }

    #[test]
    fn test_load_recipient_display_names_labels_non_to_recipients_when_primary_is_absent() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (30, 101, 3, 'cc', NULL, NULL),
                (31, 101, 1, 'bcc', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let recipients =
            load_recipient_display_names(&conn, 101, "test").expect("load display names");
        assert_eq!(
            recipients,
            vec!["cc: Carol".to_string(), "bcc: Alice".to_string()],
            "fallback recipients should preserve kind labels when no primary recipient exists"
        );
    }

    #[test]
    fn test_load_recipient_display_names_supports_real_schema_without_recipient_id_column() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_recipients_real_schema.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE message_recipients (
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                read_ts INTEGER,
                ack_ts INTEGER,
                PRIMARY KEY (message_id, agent_id)
            )",
            &empty,
        )
        .expect("create recipients");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name) VALUES
                (1, 1, 'Alice'),
                (2, 1, 'Bob'),
                (3, 1, 'Carol')",
            &empty,
        )
        .expect("insert agents");
        conn.query_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES
                (42, 2, 'to', NULL, NULL),
                (42, 3, 'cc', NULL, NULL)",
            &empty,
        )
        .expect("insert recipients");

        let recipients =
            load_recipient_display_names(&conn, 42, "test").expect("load recipient display names");
        assert_eq!(recipients, vec!["Bob".to_string()]);
    }

    #[test]
    fn test_build_outbox_entries_supports_real_schema_without_recipient_id_column() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("robot_outbox_real_schema.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite db");
        let empty: [mcp_agent_mail_db::sqlmodel_core::Value; 0] = [];

        conn.query_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL
            )",
            &empty,
        )
        .expect("create agents");
        conn.query_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                subject TEXT NOT NULL,
                thread_id TEXT,
                importance TEXT NOT NULL,
                ack_required INTEGER NOT NULL,
                created_ts INTEGER NOT NULL,
                body_md TEXT NOT NULL,
                attachments TEXT NOT NULL DEFAULT '[]'
            )",
            &empty,
        )
        .expect("create messages");
        conn.query_sync(
            "CREATE TABLE message_recipients (
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                read_ts INTEGER,
                ack_ts INTEGER,
                PRIMARY KEY (message_id, agent_id, kind)
            )",
            &empty,
        )
        .expect("create recipients");
        conn.query_sync(
            "INSERT INTO agents (id, project_id, name) VALUES
                (1, 1, 'Sender'),
                (2, 1, 'Bob'),
                (3, 1, 'Carol')",
            &empty,
        )
        .expect("insert agents");
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (42, 1, 1, 'Schema-safe outbox', 'OUTBOX-42', 'normal', 1, 100, 'body', '[]')",
            &empty,
        )
        .expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES
                (42, 2, 'to', NULL, 150),
                (42, 3, 'cc', NULL, NULL)",
            &empty,
        )
        .expect("insert recipients");

        let entries = build_outbox_entries(&conn, 1, 1, 10, false).expect("build outbox");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].from, "Bob");
        assert_eq!(entries[0].ack_status, "partial (1/2)");
    }

    #[test]
    fn test_build_message_to_field_only_shows_primary_recipients() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES
                (102, 1, 1, 'Primary recipients', 'RECIPIENTS', 'normal', 0, ?, 'body', '[]'),
                (103, 1, 1, 'Non-primary recipients', 'RECIPIENTS', 'normal', 0, ?, 'body', '[]')",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts + 1),
            ],
        )
        .expect("insert messages");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (30, 102, 2, 'to', NULL, NULL),
                (31, 102, 3, 'cc', NULL, NULL),
                (32, 102, 1, 'bcc', NULL, NULL),
                (33, 103, 3, 'cc', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let primary = build_message(&conn, 1, 102).expect("build primary message");
        assert_eq!(primary.to, vec!["Bob".to_string()]);

        let no_primary = build_message(&conn, 1, 103).expect("build message without to");
        assert!(
            no_primary.to == vec!["cc: Carol".to_string()],
            "cc-only recipients should remain visible with an explicit kind label"
        );
    }

    #[test]
    fn test_attachment_type_only_no_media_type_falls_back_to_octet_stream() {
        // Bug: "auto" type was not filtered, leaking as a mime_type value.
        // All disposition values (file, inline, auto) must fall back to
        // application/octet-stream when no media_type/content_type is present.
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        let attachments = serde_json::json!([
            { "type": "file", "bytes": 100, "name": "a.bin" },
            { "type": "inline", "bytes": 200, "name": "b.bin" },
            { "type": "auto", "bytes": 300, "name": "c.bin" },
            { "type": "image/png", "bytes": 400, "name": "d.png" },
        ])
        .to_string();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(102),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Type filter test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("TF-THREAD".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(0),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text(attachments),
            ],
        )
        .expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (4, 102, 2, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipient");

        let message = build_message(&conn, 1, 102).expect("build message");
        assert_eq!(message.attachments.len(), 4);
        // "file", "inline", "auto" are disposition values → fall back to octet-stream
        assert_eq!(message.attachments[0].mime_type, "application/octet-stream");
        assert_eq!(message.attachments[1].mime_type, "application/octet-stream");
        assert_eq!(message.attachments[2].mime_type, "application/octet-stream");
        // "image/png" in type field is a valid mime_type fallback
        assert_eq!(message.attachments[3].mime_type, "image/png");
    }

    #[test]
    fn test_thread_ack_all_done() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(103),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Ack done test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("ACK-DONE-THREAD".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
            ],
        )
        .expect("insert message");
        // Both recipients have ack_ts set
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (5, 103, 2, 'to', NULL, 111111),
                (6, 103, 3, 'to', NULL, 222222)",
            &[],
        )
        .expect("insert recipients");

        let thread =
            build_thread(&conn, 1, "ACK-DONE-THREAD", Some(10), None, false).expect("build thread");
        assert_eq!(thread.messages[0].ack, "done");
    }

    #[test]
    fn test_thread_ack_required_none_acked() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(104),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Ack needed test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("ACK-REQ-THREAD".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
            ],
        )
        .expect("insert message");
        // Both recipients have ack_ts = NULL
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (7, 104, 2, 'to', NULL, NULL),
                (8, 104, 3, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let thread =
            build_thread(&conn, 1, "ACK-REQ-THREAD", Some(10), None, false).expect("build thread");
        assert_eq!(thread.messages[0].ack, "required");
    }

    #[test]
    fn test_thread_ack_not_required_shows_none() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let created_ts = mcp_agent_mail_db::now_micros();
        conn.query_sync(
            "INSERT INTO messages
             (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(105),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("No ack test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("NO-ACK-THREAD".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(0),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
            ],
        )
        .expect("insert message");
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (9, 105, 2, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipient");

        let thread =
            build_thread(&conn, 1, "NO-ACK-THREAD", Some(10), None, false).expect("build thread");
        assert_eq!(thread.messages[0].ack, "none");
    }

    #[test]
    fn test_build_thread_numeric_seed_includes_root_and_latest_window() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        for (id, subject, thread_id, created_ts) in [
            (
                200,
                "Root",
                mcp_agent_mail_db::sqlmodel_core::Value::Null,
                10_i64,
            ),
            (
                201,
                "Reply one",
                mcp_agent_mail_db::sqlmodel_core::Value::Text("200".to_string()),
                20_i64,
            ),
            (
                202,
                "Reply two",
                mcp_agent_mail_db::sqlmodel_core::Value::Text("200".to_string()),
                30_i64,
            ),
        ] {
            conn.query_sync(
                "INSERT INTO messages
                 (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(id),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text(subject.to_string()),
                    thread_id,
                    mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(0),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text(format!("{subject} body")),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
                ],
            )
            .expect("insert message");
            conn.query_sync(
                "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
                 VALUES (?, ?, ?, 'to', NULL, NULL)",
                &[
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(id),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(id),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(2),
                ],
            )
            .expect("insert recipient");
        }

        let full = build_thread(&conn, 1, "200", Some(10), None, false).expect("build thread");
        assert_eq!(full.message_count, 3);
        assert_eq!(full.messages[0].subject, "Root");
        assert_eq!(full.messages[2].subject, "Reply two");

        let latest_two =
            build_thread(&conn, 1, "200", Some(2), None, false).expect("build limited thread");
        assert_eq!(latest_two.message_count, 2);
        assert_eq!(latest_two.subject, "Root");
        assert_eq!(latest_two.participants, vec!["Alice", "Bob"]);
        assert_eq!(latest_two.messages[0].subject, "Reply one");
        assert_eq!(latest_two.messages[1].subject, "Reply two");
    }

    #[test]
    fn test_build_thread_rejects_unknown_thread() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        let err = build_thread(&conn, 1, "missing-thread", Some(10), None, false)
            .expect_err("unknown thread should error");
        assert!(err.to_string().contains("thread not found: missing-thread"));
    }

    #[test]
    fn test_build_thread_default_window_prefers_latest_messages() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();

        for id in 10_000..10_205 {
            let seq = id - 9_999;
            conn.query_sync(
                "INSERT INTO messages
                 (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
                 VALUES (?, 1, 1, 'LONG-THREAD', ?, '', 'normal', 0, ?, '[]')",
                &[
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(id),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text(format!("Message {seq:03}")),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1_000_000 + seq),
                ],
            )
            .expect("insert long-thread message");
            conn.query_sync(
                "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
                 VALUES (?, ?, 2, 'to', NULL, NULL)",
                &[
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(id),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(id),
                ],
            )
            .expect("insert long-thread recipient");
        }

        let latest_window =
            build_thread(&conn, 1, "LONG-THREAD", None, None, false).expect("build thread");
        assert_eq!(latest_window.message_count, 200);
        assert_eq!(
            latest_window
                .messages
                .first()
                .map(|msg| msg.subject.as_str()),
            Some("Message 006")
        );
        assert_eq!(
            latest_window
                .messages
                .last()
                .map(|msg| msg.subject.as_str()),
            Some("Message 205")
        );
    }

    #[test]
    fn test_build_message_uses_seeded_thread_context_for_root_and_reply() {
        let (_temp_dir, conn) = setup_robot_thread_message_test_db();
        for (id, sender_id, subject, thread_id, created_ts) in [
            (
                300,
                1,
                "Root",
                mcp_agent_mail_db::sqlmodel_core::Value::Null,
                100_i64,
            ),
            (
                301,
                2,
                "Reply",
                mcp_agent_mail_db::sqlmodel_core::Value::Text("300".to_string()),
                200_i64,
            ),
        ] {
            conn.query_sync(
                "INSERT INTO messages
                 (id, project_id, sender_id, subject, thread_id, importance, ack_required, created_ts, body_md, attachments)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(id),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(sender_id),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text(subject.to_string()),
                    thread_id,
                    mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(0),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(created_ts),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text(format!("{subject} body")),
                    mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
                ],
            )
            .expect("insert message");
        }
        conn.query_sync(
            "INSERT INTO message_recipients (id, message_id, agent_id, kind, read_ts, ack_ts)
             VALUES
                (300, 300, 2, 'to', NULL, NULL),
                (301, 301, 1, 'to', NULL, NULL)",
            &[],
        )
        .expect("insert recipients");

        let root = build_message(&conn, 1, 300).expect("build root message");
        assert_eq!(root.thread, "300");
        assert_eq!(root.position, 1);
        assert_eq!(root.total_in_thread, 2);
        assert_eq!(root.next.as_deref(), Some("#301 Bob: Reply"));

        let reply = build_message(&conn, 1, 301).expect("build reply message");
        assert_eq!(reply.thread, "300");
        assert_eq!(reply.position, 2);
        assert_eq!(reply.total_in_thread, 2);
        assert_eq!(reply.previous.as_deref(), Some("#300 Alice: Root"));
    }

    // ── is_prose_command ────────────────────────────────────────────────

    #[test]
    fn is_prose_command_thread_is_true() {
        assert!(is_prose_command("thread"));
    }

    #[test]
    fn is_prose_command_message_is_true() {
        assert!(is_prose_command("message"));
    }

    #[test]
    fn is_prose_command_status_is_false() {
        assert!(!is_prose_command("status"));
    }

    #[test]
    fn is_prose_command_inbox_is_false() {
        assert!(!is_prose_command("inbox"));
    }

    #[test]
    fn is_prose_command_outbox_is_false() {
        assert!(!is_prose_command("outbox"));
    }

    #[test]
    fn is_prose_command_empty_string_is_false() {
        assert!(!is_prose_command(""));
    }

    #[test]
    fn is_prose_command_case_sensitive() {
        assert!(!is_prose_command("Thread"));
        assert!(!is_prose_command("MESSAGE"));
    }

    // ── resolve_format ─────────────────────────────────────────────────

    #[test]
    fn resolve_format_explicit_json_overrides_prose() {
        assert_eq!(
            resolve_format(Some(OutputFormat::Json), "thread"),
            OutputFormat::Json
        );
    }

    #[test]
    fn resolve_format_explicit_toon_overrides_prose() {
        assert_eq!(
            resolve_format(Some(OutputFormat::Toon), "message"),
            OutputFormat::Toon
        );
    }

    #[test]
    fn resolve_format_prose_command_without_explicit_returns_markdown() {
        assert_eq!(resolve_format(None, "thread"), OutputFormat::Markdown);
        assert_eq!(resolve_format(None, "message"), OutputFormat::Markdown);
    }

    #[test]
    fn resolve_format_non_prose_without_explicit_uses_terminal_detection() {
        // In test context stdout is piped → Json
        let fmt = resolve_format(None, "status");
        // When piped (test runner), should be Json; when TTY, Toon.
        // We accept either since we can't control the test runner's TTY status.
        assert!(fmt == OutputFormat::Json || fmt == OutputFormat::Toon);
    }

    #[test]
    fn resolve_format_explicit_markdown_for_non_prose() {
        assert_eq!(
            resolve_format(Some(OutputFormat::Markdown), "status"),
            OutputFormat::Markdown
        );
    }

    // ── format_output ──────────────────────────────────────────────────

    #[test]
    fn format_output_json_produces_valid_json() {
        let data = TestData {
            items: vec!["a".into()],
            count: 1,
        };
        let envelope = RobotEnvelope::new("test", OutputFormat::Json, data);
        let result = format_output(&envelope, OutputFormat::Json).unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 1);
    }

    #[test]
    fn format_output_toon_produces_non_empty_string() {
        let data = TestData {
            items: vec!["x".into()],
            count: 42,
        };
        let envelope = RobotEnvelope::new("test", OutputFormat::Json, data);
        let result = format_output(&envelope, OutputFormat::Toon).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn format_output_markdown_falls_back_to_toon_for_non_renderable() {
        let data = TestData {
            items: vec![],
            count: 0,
        };
        let envelope = RobotEnvelope::new("test", OutputFormat::Json, data);
        // Markdown on a non-MarkdownRenderable type falls through to TOON
        let md_result = format_output(&envelope, OutputFormat::Markdown).unwrap();
        let toon_result = format_output(&envelope, OutputFormat::Toon).unwrap();
        assert_eq!(md_result, toon_result);
    }

    // ── RobotEnvelope builder ──────────────────────────────────────────

    #[test]
    fn envelope_with_alert_populates_alerts_array() {
        let data = TestData {
            items: vec![],
            count: 0,
        };
        let envelope = RobotEnvelope::new("test", OutputFormat::Json, data).with_alert(
            "warn",
            "something happened",
            Some("fix it".into()),
        );
        let json = serde_json::to_value(&envelope).unwrap();
        let alerts = json["_alerts"].as_array().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["severity"], "warn");
        assert_eq!(alerts[0]["summary"], "something happened");
    }

    #[test]
    fn envelope_with_action_populates_actions_array() {
        let data = TestData {
            items: vec![],
            count: 0,
        };
        let envelope =
            RobotEnvelope::new("test", OutputFormat::Json, data).with_action("am robot status");
        let json = serde_json::to_value(&envelope).unwrap();
        let actions = json["_actions"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], "am robot status");
    }

    #[test]
    fn envelope_meta_contains_command() {
        let data = TestData {
            items: vec![],
            count: 0,
        };
        let envelope = RobotEnvelope::new("inbox", OutputFormat::Json, data);
        assert_eq!(envelope._meta.command, "inbox");
    }

    #[test]
    fn envelope_chain_multiple_alerts_and_actions() {
        let data = TestData {
            items: vec![],
            count: 0,
        };
        let envelope = RobotEnvelope::new("test", OutputFormat::Json, data)
            .with_alert("error", "a1", None)
            .with_alert("warn", "a2", Some("fix".into()))
            .with_action("cmd1")
            .with_action("cmd2");
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["_alerts"].as_array().unwrap().len(), 2);
        assert_eq!(json["_actions"].as_array().unwrap().len(), 2);
    }
}
