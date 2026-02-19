//! Robot output types for agent-optimized CLI commands.
//!
//! Provides the `OutputFormat` selector, `RobotEnvelope<T>` response wrapper,
//! and the `format_output()` dispatcher used by all `am robot *` commands.

#![allow(clippy::module_name_repetitions)]

use chrono::Utc;
use clap::{Args, Subcommand};
use serde::Serialize;
use sqlmodel_core::Value;
use std::io::IsTerminal;

use crate::CliError;

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
        } else if std::io::stdout().is_terminal() {
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
    if std::io::stdout().is_terminal() {
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

    /// Project key (absolute path or slug). Auto-detected from CWD if omitted.
    #[arg(long, global = true)]
    pub project: Option<String>,

    /// Agent name. Auto-detected from AGENT_NAME env var if omitted.
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
        /// Show all reservations (including expired).
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
        }
    }
}

// ── DB helpers ──────────────────────────────────────────────────────────────

use mcp_agent_mail_db::DbConn;

/// Resolve a project by slug or human_key.
///
/// Note: Uses LIKE instead of = for string comparison due to a bug in sqlmodel_sqlite
/// where parameterized = queries don't match strings correctly.
fn resolve_project_sync(conn: &DbConn, key: &str) -> Result<(i64, String), CliError> {
    // Try slug first (using LIKE for workaround)
    let rows = conn
        .query_sync(
            "SELECT id, slug FROM projects WHERE slug LIKE ?",
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
    // Try human_key (using LIKE for workaround)
    let rows = conn
        .query_sync(
            "SELECT id, slug FROM projects WHERE human_key LIKE ?",
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

/// Find the project for the current working directory.
fn find_project_for_cwd(conn: &DbConn) -> Result<(i64, String), CliError> {
    let cwd =
        std::env::current_dir().map_err(|e| CliError::Other(format!("cannot get CWD: {e}")))?;
    let cwd_str = cwd.to_string_lossy().to_string();
    resolve_project_sync(conn, &cwd_str)
}

/// Resolve project from --project flag or CWD.
fn resolve_project(conn: &DbConn, flag: Option<&str>) -> Result<(i64, String), CliError> {
    if let Some(key) = flag {
        resolve_project_sync(conn, key)
    } else {
        find_project_for_cwd(conn)
    }
}

/// Resolve agent ID from --agent flag or AGENT_NAME env.
///
/// Note: Uses LIKE instead of = for string comparison due to a bug in sqlmodel_sqlite
/// where parameterized = queries don't match strings correctly.
fn resolve_agent_id(
    conn: &DbConn,
    project_id: i64,
    flag: Option<&str>,
) -> Option<(i64, String)> {
    let name = flag
        .map(String::from)
        .or_else(|| std::env::var("AGENT_NAME").ok())?;
    let rows = conn
        .query_sync(
            "SELECT id, name FROM agents WHERE project_id = ? AND name LIKE ?",
            &[Value::BigInt(project_id), Value::Text(name)],
        )
        .ok()?;
    let row = rows.first()?;
    let id: i64 = row.get_as(0).ok()?;
    let agent_name: String = row.get_as(1).ok()?;
    if id > 0 { Some((id, agent_name)) } else { None }
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

// ── Status command implementation ───────────────────────────────────────────

fn build_status(
    conn: &DbConn,
    project_id: i64,
    project_slug: &str,
    agent: Option<(i64, String)>,
) -> Result<(StatusData, Vec<String>), CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    let _now_s = now_us / 1_000_000;

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
        // ack_overdue threshold: 30 minutes = 30*60*1_000_000 micros
        let threshold = now_us - 30 * 60 * 1_000_000;
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
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get_named::<i64>("cnt").ok()))
        .unwrap_or(0) as usize;

    // 3. Recent messages (last hour)
    let hour_ago = now_us - 3600 * 1_000_000;
    let recent_messages = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM messages WHERE project_id = ? AND created_ts > ?",
            &[Value::BigInt(project_id), Value::BigInt(hour_ago)],
        )
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get_named::<i64>("cnt").ok()))
        .unwrap_or(0) as usize;

    // 4. File reservations (active = not released and not expired)
    let active_reservations = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM file_reservations
             WHERE project_id = ? AND released_ts IS NULL AND expires_ts > ?",
            &[Value::BigInt(project_id), Value::BigInt(now_us)],
        )
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get_named::<i64>("cnt").ok()))
        .unwrap_or(0) as usize;

    // Reservations expiring soon (within 5 minutes)
    let expiring_threshold = now_us + 5 * 60 * 1_000_000;
    let reservations_expiring_soon = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM file_reservations
             WHERE project_id = ? AND released_ts IS NULL
             AND expires_ts > ? AND expires_ts < ?",
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us),
                Value::BigInt(expiring_threshold),
            ],
        )
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get_named::<i64>("cnt").ok()))
        .unwrap_or(0) as usize;

    // 5. My reservations (agent-specific)
    let my_reservations = if let Some((agent_id, _)) = &agent {
        conn.query_sync(
            "SELECT path_pattern, exclusive, expires_ts FROM file_reservations
             WHERE project_id = ? AND agent_id = ? AND released_ts IS NULL AND expires_ts > ?
             ORDER BY expires_ts ASC",
            &[
                Value::BigInt(project_id),
                Value::BigInt(*agent_id),
                Value::BigInt(now_us),
            ],
        )
        .unwrap_or_default()
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
            "SELECT thread_id,
                    COUNT(*) AS msg_count,
                    MAX(created_ts) AS last_ts,
                    MIN(subject) AS subject
             FROM messages
             WHERE project_id = ? AND thread_id IS NOT NULL
             GROUP BY thread_id
             ORDER BY last_ts DESC
             LIMIT 3",
            &[Value::BigInt(project_id)],
        )
        .unwrap_or_default();

    let top_threads: Vec<ThreadSummary> = top_threads_rows
        .iter()
        .map(|r| {
            let thread_id: String = r.get_named("thread_id").unwrap_or_default();
            let msg_count: i64 = r.get_named("msg_count").unwrap_or(0);
            let last_ts: i64 = r.get_named("last_ts").unwrap_or(0);
            let subject: String = r.get_named("subject").unwrap_or_default();
            // Count distinct participants for this thread
            let participants = conn
                .query_sync(
                    "SELECT COUNT(DISTINCT sender_id) AS cnt FROM messages
                     WHERE project_id = ? AND thread_id = ?",
                    &[Value::BigInt(project_id), Value::Text(thread_id.clone())],
                )
                .ok()
                .and_then(|rows| rows.first().and_then(|r2| r2.get_named::<i64>("cnt").ok()))
                .unwrap_or(1) as usize;
            let age_seconds = (now_us - last_ts) / 1_000_000;
            ThreadSummary {
                id: thread_id,
                subject,
                participants,
                messages: msg_count as usize,
                last_activity: format_age(age_seconds),
            }
        })
        .collect();

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
            "am mail ack --project {project_slug} --agent {name}"
        ));
    }
    if let Some(top) = top_threads.first() {
        actions.push(format!("am robot thread {}", top.id));
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
    _unread_only: bool,
    show_all: bool,
    limit: usize,
    include_bodies: bool,
) -> Result<InboxResult, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();
    // ack_overdue threshold: 30 minutes
    let ack_threshold = now_us - 30 * 60 * 1_000_000;

    // Build WHERE filter based on flags
    let bucket_filter = if ack_overdue_only {
        "AND priority_bucket = 1"
    } else if urgent_only {
        "AND priority_bucket <= 2"
    } else if show_all {
        "" // no filter
    } else {
        "AND priority_bucket <= 5" // default (and unread_only): unread only
    };

    let sql = format!(
        "SELECT sub.*, a_sender.name AS sender_name
         FROM (
             SELECT m.id, m.subject, m.thread_id, m.importance, m.ack_required,
                    m.created_ts, m.sender_id, mr.read_ts, mr.ack_ts, m.body_md,
                    CASE
                        WHEN m.ack_required = 1 AND mr.ack_ts IS NULL AND m.created_ts < ? THEN 1
                        WHEN m.importance IN ('urgent','high') AND mr.read_ts IS NULL THEN 2
                        WHEN m.ack_required = 1 AND mr.ack_ts IS NULL AND mr.read_ts IS NULL THEN 3
                        WHEN m.importance = 'high' AND mr.read_ts IS NULL THEN 4
                        WHEN mr.read_ts IS NULL THEN 5
                        WHEN m.ack_required = 1 AND mr.ack_ts IS NULL THEN 6
                        ELSE 7
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
                Value::BigInt(limit as i64),
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
        let importance: String = row.get_named("importance").unwrap_or_default();
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
        let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
        let ack_ts: Option<i64> = row.get_named("ack_ts").ok();
        let read_ts: Option<i64> = row.get_named("read_ts").ok();

        let priority_label = match bucket {
            1 => "ack-overdue",
            2 => "urgent",
            3 => "ack-required",
            4 => "high",
            5 => "unread",
            6 => "read-unacked",
            _ => "read",
        };

        let ack_status = if ack_required == 0 {
            "none".to_string()
        } else if ack_ts.is_some() {
            "acked".to_string()
        } else if bucket == 1 {
            "overdue".to_string()
        } else if read_ts.is_some() {
            "pending".to_string()
        } else {
            "required".to_string()
        };

        let age_seconds = (now_us - created_ts) / 1_000_000;

        if bucket == 1 {
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
            thread: thread_id,
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
        actions.push(format!("am robot thread {}", entry.thread));
    }

    Ok(InboxResult {
        entries,
        alerts,
        actions,
    })
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

    let mut conditions = vec![
        "m.thread_id = ?".to_string(),
        "m.project_id = ?".to_string(),
    ];
    let mut params: Vec<Value> = vec![
        Value::Text(thread_id.to_string()),
        Value::BigInt(project_id),
    ];

    if let Some(since_str) = since {
        let since_us = mcp_agent_mail_db::iso_to_micros(since_str).unwrap_or(0);
        conditions.push("m.created_ts > ?".to_string());
        params.push(Value::BigInt(since_us));
    }

    let limit_val = limit.unwrap_or(200);
    params.push(Value::BigInt(limit_val as i64));

    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT m.id, m.subject, m.body_md, m.importance, m.ack_required, m.created_ts,
                m.sender_id, a_sender.name AS sender_name
         FROM messages m
         JOIN agents a_sender ON a_sender.id = m.sender_id
         WHERE {where_clause}
         ORDER BY m.created_ts ASC
         LIMIT ?"
    );

    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| CliError::Other(format!("thread query failed: {e}")))?;

    let mut messages = Vec::new();
    let mut participants = Vec::new();
    let mut last_ts: i64 = 0;
    let mut thread_subject = String::new();

    for (idx, row) in rows.iter().enumerate() {
        let msg_id: i64 = row.get_named("id").unwrap_or(0);
        let subject: String = row.get_named("subject").unwrap_or_default();
        let body: String = row.get_named("body_md").unwrap_or_default();
        let importance: String = row.get_named("importance").unwrap_or_default();
        let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
        let sender: String = row.get_named("sender_name").unwrap_or_default();

        if idx == 0 {
            thread_subject.clone_from(&subject);
        }
        if created_ts > last_ts {
            last_ts = created_ts;
        }
        if !participants.contains(&sender) {
            participants.push(sender.clone());
        }

        // Get recipients for this message
        let recipients = conn
            .query_sync(
                "SELECT a.name FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap_or_default();
        let to_names: Vec<String> = recipients
            .iter()
            .filter_map(|r| r.get_named::<String>("name").ok())
            .collect();

        // Check ack status
        let ack_status = if ack_required == 0 {
            "none".to_string()
        } else {
            let acked = conn
                .query_sync(
                    "SELECT COUNT(*) AS cnt FROM message_recipients
                     WHERE message_id = ? AND ack_ts IS NOT NULL",
                    &[Value::BigInt(msg_id)],
                )
                .ok()
                .and_then(|r| r.first().and_then(|r2| r2.get_named::<i64>("cnt").ok()))
                .unwrap_or(0);
            if acked > 0 {
                "done".to_string()
            } else {
                "required".to_string()
            }
        };

        let age_seconds = (now_us - created_ts) / 1_000_000;

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
        format_age((now_us - last_ts) / 1_000_000)
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

/// Truncate a string to `max_len` chars, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.min(s.len())])
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

    // Recipients
    let recipient_rows = conn
        .query_sync(
            "SELECT a.name FROM message_recipients mr
             JOIN agents a ON a.id = mr.agent_id
             WHERE mr.message_id = ?",
            &[Value::BigInt(message_id)],
        )
        .unwrap_or_default();
    let to: Vec<String> = recipient_rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

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
            .ok()
            .and_then(|r| r.first().and_then(|r2| r2.get_named::<i64>("cnt").ok()))
            .unwrap_or(0);
        let total_recipients: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients WHERE message_id = ?",
                &[Value::BigInt(message_id)],
            )
            .ok()
            .and_then(|r| r.first().and_then(|r2| r2.get_named::<i64>("cnt").ok()))
            .unwrap_or(0);
        if acked_count >= total_recipients && total_recipients > 0 {
            "done".to_string()
        } else if acked_count > 0 {
            format!("partial ({acked_count}/{total_recipients})")
        } else {
            "pending".to_string()
        }
    };

    // Thread context
    let (position, total_in_thread) = if !thread_id.is_empty() {
        let total: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages WHERE thread_id = ? AND project_id = ?",
                &[Value::Text(thread_id.clone()), Value::BigInt(project_id)],
            )
            .ok()
            .and_then(|r| r.first().and_then(|r2| r2.get_named::<i64>("cnt").ok()))
            .unwrap_or(1);
        let pos: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages
                 WHERE thread_id = ? AND project_id = ? AND created_ts <= ?",
                &[
                    Value::Text(thread_id.clone()),
                    Value::BigInt(project_id),
                    Value::BigInt(created_ts),
                ],
            )
            .ok()
            .and_then(|r| r.first().and_then(|r2| r2.get_named::<i64>("cnt").ok()))
            .unwrap_or(1);
        (pos as usize, total as usize)
    } else {
        (1, 1)
    };

    // Adjacent messages
    let previous = if !thread_id.is_empty() {
        conn.query_sync(
            "SELECT m.id, a.name AS sender, m.subject FROM messages m
             JOIN agents a ON a.id = m.sender_id
             WHERE m.thread_id = ? AND m.project_id = ? AND m.created_ts < ?
             ORDER BY m.created_ts DESC LIMIT 1",
            &[
                Value::Text(thread_id.clone()),
                Value::BigInt(project_id),
                Value::BigInt(created_ts),
            ],
        )
        .ok()
        .and_then(|r| {
            r.first().map(|row| {
                let pid: i64 = row.get_named("id").unwrap_or(0);
                let pname: String = row.get_named("sender").unwrap_or_default();
                let psubj: String = row.get_named("subject").unwrap_or_default();
                format!("#{pid} {pname}: {}", truncate_str(&psubj, 60))
            })
        })
    } else {
        None
    };

    let next = if !thread_id.is_empty() {
        conn.query_sync(
            "SELECT m.id, a.name AS sender, m.subject FROM messages m
             JOIN agents a ON a.id = m.sender_id
             WHERE m.thread_id = ? AND m.project_id = ? AND m.created_ts > ?
             ORDER BY m.created_ts ASC LIMIT 1",
            &[
                Value::Text(thread_id.clone()),
                Value::BigInt(project_id),
                Value::BigInt(created_ts),
            ],
        )
        .ok()
        .and_then(|r| {
            r.first().map(|row| {
                let nid: i64 = row.get_named("id").unwrap_or(0);
                let nname: String = row.get_named("sender").unwrap_or_default();
                let nsubj: String = row.get_named("subject").unwrap_or_default();
                format!("#{nid} {nname}: {}", truncate_str(&nsubj, 60))
            })
        })
    } else {
        None
    };

    // Parse attachments JSON
    let attachments: Vec<AttachmentInfo> =
        serde_json::from_str::<serde_json::Value>(&attachments_json)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        Some(AttachmentInfo {
                            name: a.get("name")?.as_str()?.to_string(),
                            size: a
                                .get("size")
                                .and_then(|s| s.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            mime_type: a
                                .get("type")
                                .or_else(|| a.get("content_type"))
                                .and_then(|s| s.as_str())
                                .unwrap_or("application/octet-stream")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

    let age_seconds = (now_us - created_ts) / 1_000_000;
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
        thread: thread_id,
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
    importance_filter: Option<&str>,
    since: Option<&str>,
    limit: usize,
) -> Result<SearchData, CliError> {
    let now_us = mcp_agent_mail_db::now_micros();

    // Sanitize the FTS query
    let sanitized = match mcp_agent_mail_db::queries::sanitize_fts_query(query) {
        Some(s) => s,
        None => {
            return Ok(SearchData {
                query: query.to_string(),
                total_results: 0,
                results: vec![],
                by_thread: vec![],
                by_agent: vec![],
                by_importance: vec![],
            });
        }
    };
    if sanitized.is_empty() {
        return Ok(SearchData {
            query: query.to_string(),
            total_results: 0,
            results: vec![],
            by_thread: vec![],
            by_agent: vec![],
            by_importance: vec![],
        });
    }

    // Build additional WHERE conditions
    let mut extra_where = String::new();
    let mut params: Vec<Value> = vec![Value::Text(sanitized.clone()), Value::BigInt(project_id)];

    if let Some(imp) = importance_filter {
        extra_where.push_str(" AND m.importance = ?");
        params.push(Value::Text(imp.to_string()));
    }
    if let Some(since_str) = since {
        let since_us = mcp_agent_mail_db::iso_to_micros(since_str).unwrap_or(0);
        extra_where.push_str(" AND m.created_ts > ?");
        params.push(Value::BigInt(since_us));
    }
    params.push(Value::BigInt(limit as i64));

    let sql = format!(
        "SELECT m.id, m.subject, m.thread_id, m.importance, m.created_ts,
                a.name AS sender_name,
                snippet(fts_messages, 1, '»', '«', '…', 20) AS snippet
         FROM fts_messages
         JOIN messages m ON fts_messages.rowid = m.id
         JOIN agents a ON a.id = m.sender_id
         WHERE fts_messages MATCH ? AND m.project_id = ?
         {extra_where}
         ORDER BY bm25(fts_messages)
         LIMIT ?"
    );

    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| CliError::Other(format!("search query failed: {e}")))?;

    // Build results and facets
    let mut results = Vec::new();
    let mut thread_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut agent_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut importance_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for row in &rows {
        let msg_id: i64 = row.get_named("id").unwrap_or(0);
        let subject: String = row.get_named("subject").unwrap_or_default();
        let thread_id: String = row.get_named("thread_id").unwrap_or_default();
        let importance: String = row.get_named("importance").unwrap_or_default();
        let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
        let sender: String = row.get_named("sender_name").unwrap_or_default();
        let snippet: String = row.get_named("snippet").unwrap_or_default();

        // Aggregate facets
        if !thread_id.is_empty() {
            *thread_counts.entry(thread_id.clone()).or_insert(0) += 1;
        }
        *agent_counts.entry(sender.clone()).or_insert(0) += 1;
        *importance_counts.entry(importance.clone()).or_insert(0) += 1;

        let age_seconds = (now_us - created_ts) / 1_000_000;
        results.push(SearchResult {
            id: msg_id,
            relevance: 0.0, // bm25 returns negative scores; normalize later if needed
            from: sender,
            subject,
            thread: thread_id,
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

// ── Reservations command implementation ─────────────────────────────────────

/// Conflict between two reservations.
#[derive(Debug, Serialize)]
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

    // Fetch all active reservations
    let all_rows = conn
        .query_sync(
            "SELECT fr.id, fr.path_pattern, fr.exclusive, fr.created_ts, fr.expires_ts,
                    a.name AS agent_name, a.id AS agent_id
             FROM file_reservations fr
             JOIN agents a ON a.id = fr.agent_id
             WHERE fr.project_id = ? AND fr.released_ts IS NULL AND fr.expires_ts > ?
             ORDER BY fr.expires_ts ASC",
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

        let remaining_seconds = (expires_ts - now_us) / 1_000_000;
        let created_age = (now_us - created_ts) / 1_000_000;

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
                conflicts.push(ReservationConflict {
                    agent_a: a.agent.clone().unwrap_or_default(),
                    path_a: a.path.clone(),
                    agent_b: b.agent.clone().unwrap_or_default(),
                    path_b: b.path.clone(),
                });
            }
        }
    }

    // Build actions
    let mut actions = Vec::new();
    if let Some((_, ref agent_name)) = agent {
        for entry in &expiring_soon {
            if entry.agent.as_deref() == Some(agent_name.as_str()) {
                actions.push(format!(
                    "am file_reservations renew {project_slug} {agent_name} --paths {} --extend 3600",
                    entry.path
                ));
            }
        }
    }

    // Apply filters
    if conflicts_only {
        // Only keep entries involved in conflicts
        let conflict_paths: std::collections::HashSet<String> = conflicts
            .iter()
            .flat_map(|c| vec![c.path_a.clone(), c.path_b.clone()])
            .collect();
        let filtered: Vec<_> = all_active
            .into_iter()
            .filter(|e| conflict_paths.contains(&e.path))
            .collect();
        return Ok((
            ReservationsData {
                my_reservations: vec![],
                all_active: filtered,
                conflicts,
                expiring_soon: vec![],
            },
            actions,
        ));
    }

    if !show_all && agent.is_some() {
        // When not --all, only show my reservations in all_active
        return Ok((
            ReservationsData {
                my_reservations: my_reservations.clone(),
                all_active: my_reservations,
                conflicts,
                expiring_soon,
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

/// Check for glob pattern overlap using the robust implementation from tools.
///
/// Delegates to `mcp_agent_mail_tools::pattern_overlap::CompiledPattern` to ensure
/// consistency with server-side conflict detection. Handles complex globs,
/// normalization, and symmetric matching correctly.
fn glob_matches(pattern: &str, path: &str) -> bool {
    mcp_agent_mail_tools::patterns_overlap(pattern, path)
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
        mcp_agent_mail_db::iso_to_micros(s)
            .ok_or_else(|| CliError::InvalidArgument(format!("invalid --since timestamp: {s}")))?
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
        let res_rows = conn
            .query_sync(
                "SELECT fr.id, fr.path_pattern, fr.created_ts, fr.released_ts, a.name AS agent
                 FROM file_reservations fr
                 JOIN agents a ON a.id = fr.agent_id
                 WHERE fr.project_id = ? AND (fr.created_ts > ? OR (fr.released_ts IS NOT NULL AND fr.released_ts > ?))
                 ORDER BY fr.created_ts ASC",
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
    let now_us = mcp_agent_mail_db::now_micros();

    let rows = conn
        .query_sync("SELECT id, slug FROM projects ORDER BY slug ASC", &[])
        .map_err(|e| CliError::Other(format!("overview projects query: {e}")))?;

    let mut projects = Vec::new();
    for row in &rows {
        let pid: i64 = row.get_named("id").unwrap_or(0);
        let slug: String = row.get_named("slug").unwrap_or_default();

        // Count unread messages across all agents in project
        let unread: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients mr
                 JOIN messages m ON m.id = mr.message_id
                 WHERE m.project_id = ? AND mr.read_ts IS NULL",
                &[Value::BigInt(pid)],
            )
            .unwrap_or_default()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
            .unwrap_or(0);

        // Count urgent messages
        let urgent: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages
                 WHERE project_id = ? AND importance IN ('urgent', 'high')",
                &[Value::BigInt(pid)],
            )
            .unwrap_or_default()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
            .unwrap_or(0);

        // Count ack-overdue
        let ack_overdue: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients mr
                 JOIN messages m ON m.id = mr.message_id
                 WHERE m.project_id = ? AND m.ack_required = 1 AND mr.ack_ts IS NULL",
                &[Value::BigInt(pid)],
            )
            .unwrap_or_default()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
            .unwrap_or(0);

        // Count active reservations
        let reservations: i64 = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM file_reservations
                 WHERE project_id = ? AND released_ts IS NULL AND expires_ts > ?",
                &[Value::BigInt(pid), Value::BigInt(now_us)],
            )
            .unwrap_or_default()
            .first()
            .and_then(|r| r.get_named("cnt").ok())
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

    // Check for ack SLA violations (>1h old unacked)
    let ack_overdue: i64 = conn
        .query_sync(
            "SELECT COUNT(*) AS cnt FROM message_recipients mr
             JOIN messages m ON m.id = mr.message_id
             WHERE m.project_id = ? AND m.ack_required = 1 AND mr.ack_ts IS NULL
               AND m.created_ts < ?",
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us - 3600 * 1_000_000),
            ],
        )
        .unwrap_or_default()
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
    let conflict_rows = conn
        .query_sync(
            "SELECT fr1.path_pattern AS p1, fr2.path_pattern AS p2,
                    a1.name AS agent1, a2.name AS agent2
             FROM file_reservations fr1
             JOIN file_reservations fr2 ON fr1.id < fr2.id
               AND fr1.project_id = fr2.project_id
               AND fr1.agent_id != fr2.agent_id
             JOIN agents a1 ON a1.id = fr1.agent_id
             JOIN agents a2 ON a2.id = fr2.agent_id
             WHERE fr1.project_id = ? AND fr1.exclusive = 1 AND fr2.exclusive = 1
               AND fr1.released_ts IS NULL AND fr2.released_ts IS NULL
               AND fr1.expires_ts > ? AND fr2.expires_ts > ?",
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us),
                Value::BigInt(now_us),
            ],
        )
        .unwrap_or_default();
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
    let expiring_count: i64 = if let Some((agent_id, _)) = &agent {
        conn.query_sync(
            "SELECT COUNT(*) AS cnt FROM file_reservations
             WHERE project_id = ? AND agent_id = ? AND released_ts IS NULL
               AND expires_ts > ? AND expires_ts < ?",
            &[
                Value::BigInt(project_id),
                Value::BigInt(*agent_id),
                Value::BigInt(now_us),
                Value::BigInt(expiring_threshold),
            ],
        )
    } else {
        conn.query_sync(
            "SELECT COUNT(*) AS cnt FROM file_reservations
             WHERE project_id = ? AND released_ts IS NULL
               AND expires_ts > ? AND expires_ts < ?",
            &[
                Value::BigInt(project_id),
                Value::BigInt(now_us),
                Value::BigInt(expiring_threshold),
            ],
        )
    }
    .unwrap_or_default()
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
        .unwrap_or_default();
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
    let now_us = mcp_agent_mail_db::now_micros();
    let active_threshold = now_us - 15 * 60 * 1_000_000; // 15 min
    let idle_threshold = now_us - 4 * 3600 * 1_000_000; // 4 hours

    let rows = conn
        .query_sync(
            "SELECT a.id, a.name, a.program, a.model, a.last_active_ts,
                    (SELECT COUNT(*) FROM messages m WHERE m.sender_id = a.id) AS msg_count
             FROM agents a
             WHERE a.project_id = ?
             ORDER BY a.last_active_ts DESC",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| CliError::Other(format!("agents query: {e}")))?;

    let mut agents: Vec<AgentRow> = Vec::new();
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

        let age_seconds = (now_us - last_active_ts) / 1_000_000;

        agents.push(AgentRow {
            name,
            program,
            model,
            last_active: format_age(age_seconds),
            msg_count: msg_count as usize,
            status: status.to_string(),
        });
    }

    // Sort
    match sort_field {
        Some("name") => agents.sort_by(|a, b| a.name.cmp(&b.name)),
        Some("msg_count") => agents.sort_by_key(|x| std::cmp::Reverse(x.msg_count)),
        _ => {} // Default: already sorted by last_active (DESC from SQL)
    }

    Ok(agents)
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

        let age = (now_us - updated_ts) / 1_000_000;

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

    let rows = conn
        .query_sync(
            "SELECT p.id, p.slug, p.human_key, p.created_at,
                    (SELECT COUNT(*) FROM agents a WHERE a.project_id = p.id) AS agent_count,
                    (SELECT COUNT(*) FROM messages m WHERE m.project_id = p.id) AS msg_count,
                    (SELECT COUNT(*) FROM file_reservations fr
                     WHERE fr.project_id = p.id AND fr.released_ts IS NULL AND fr.expires_ts > ?) AS res_count
             FROM projects p
             ORDER BY p.slug ASC",
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

        let age = (now_us - created_at) / 1_000_000;

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

fn build_navigate(
    conn: &DbConn,
    uri: &str,
    project_id: i64,
    project_slug: &str,
    _agent: Option<(i64, String)>,
) -> Result<(NavigateResult, Option<String>), CliError> {
    let path = uri.strip_prefix("resource://").ok_or_else(|| {
        CliError::InvalidArgument(format!("invalid URI scheme: {uri} (expected resource://)"))
    })?;

    let parts: Vec<&str> = path.split('/').collect();

    match parts.as_slice() {
        ["projects"] => {
            let projects = build_projects(conn)?;
            Ok((NavigateResult::Projects { projects }, None))
        }
        ["project", slug] => {
            // Find project by slug and return its details
            let row = conn
                .query_sync(
                    "SELECT id, slug, human_key, created_at FROM projects WHERE slug = ?",
                    &[Value::Text(slug.to_string())],
                )
                .map_err(|e| CliError::Other(format!("project query: {e}")))?
                .into_iter()
                .next()
                .ok_or_else(|| CliError::Other(format!("project not found: {slug}")))?;

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
                None,
            ))
        }
        ["agents", slug] => {
            // Get project_id for slug
            let pid = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[Value::Text(slug.to_string())],
                )
                .map_err(|e| CliError::Other(format!("project lookup: {e}")))?
                .first()
                .and_then(|r| r.get_named::<i64>("id").ok())
                .ok_or_else(|| CliError::Other(format!("project not found: {slug}")))?;

            let agents = build_agents(conn, pid, false, None)?;
            Ok((NavigateResult::Agents { agents }, None))
        }
        ["inbox", agent_name] => {
            // Resolve agent and get inbox using simplified direct query
            let agent_opt = resolve_agent_id(conn, project_id, Some(agent_name));
            if let Some((agent_id, name)) = agent_opt {
                let result = build_inbox(
                    conn,
                    project_id,
                    project_slug,
                    agent_id,
                    &name,
                    false,
                    false,
                    true,
                    false,
                    50,
                    false,
                )?;
                Ok((
                    NavigateResult::Inbox {
                        entries: result.entries,
                    },
                    None,
                ))
            } else {
                Ok((NavigateResult::Inbox { entries: vec![] }, None))
            }
        }
        ["message", id_str] => {
            let msg_id: i64 = id_str
                .parse()
                .map_err(|_| CliError::InvalidArgument(format!("invalid message id: {id_str}")))?;
            let message = build_message(conn, project_id, msg_id)?;
            Ok((NavigateResult::Message { message }, None))
        }
        ["thread", thread_id] => {
            let thread = build_thread(conn, project_id, thread_id, Some(100), None, false)?;
            Ok((NavigateResult::Thread { thread }, None))
        }
        ["file_reservations", slug] => {
            // Get project_id for slug and return generic data
            let pid = conn
                .query_sync(
                    "SELECT id FROM projects WHERE slug = ?",
                    &[Value::Text(slug.to_string())],
                )
                .map_err(|e| CliError::Other(format!("project lookup: {e}")))?
                .first()
                .and_then(|r| r.get_named::<i64>("id").ok())
                .ok_or_else(|| CliError::Other(format!("project not found: {slug}")))?;

            let rows = conn
                .query_sync(
                    "SELECT fr.path_pattern, a.name, fr.exclusive, fr.expires_ts, fr.released_ts
                     FROM file_reservations fr
                     JOIN agents a ON a.id = fr.agent_id
                     WHERE fr.project_id = ?
                     ORDER BY fr.created_ts DESC LIMIT 50",
                    &[Value::BigInt(pid)],
                )
                .unwrap_or_default();

            let reservations: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "path": r.get_named::<String>("path_pattern").unwrap_or_default(),
                        "agent": r.get_named::<String>("name").unwrap_or_default(),
                        "exclusive": r.get_named::<i64>("exclusive").unwrap_or(0) == 1,
                        "expires_ts": r.get_named::<i64>("expires_ts").unwrap_or(0),
                        "released": r.get_named::<i64>("released_ts").ok().is_some(),
                    })
                })
                .collect();

            Ok((
                NavigateResult::Generic {
                    resource_type: "file_reservations".to_string(),
                    data: serde_json::json!({ "reservations": reservations }),
                },
                None,
            ))
        }
        ["mailbox", agent_name] | ["outbox", agent_name] => {
            let agent_opt = resolve_agent_id(conn, project_id, Some(agent_name));
            if let Some((agent_id, name)) = agent_opt {
                let result = build_inbox(
                    conn,
                    project_id,
                    project_slug,
                    agent_id,
                    &name,
                    false,
                    false,
                    false,
                    true,
                    50,
                    false,
                )?;
                Ok((
                    NavigateResult::Inbox {
                        entries: result.entries,
                    },
                    None,
                ))
            } else {
                Ok((NavigateResult::Inbox { entries: vec![] }, None))
            }
        }
        _ => Err(CliError::InvalidArgument(format!(
            "unsupported resource URI pattern: {uri}\n\
             Supported patterns:\n\
             - resource://projects\n\
             - resource://project/<slug>\n\
             - resource://agents/<slug>\n\
             - resource://inbox/<agent>\n\
             - resource://message/<id>\n\
             - resource://thread/<id>\n\
             - resource://file_reservations/<slug>\n\
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let agent = resolve_agent_id(&conn, project_id, args.agent.as_deref());

            let agent_name = agent.as_ref().map(|(_, n)| n.clone());
            let (data, actions) = build_status(&conn, project_id, &project_slug, agent)?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(project_slug);
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
            // Collect alert data before mutating env (avoids borrow conflict)
            let pending_alerts: Vec<(String, String, Option<String>)> = env
                .data
                .anomalies
                .iter()
                .filter(|a| a.severity == "warn")
                .map(|anomaly| {
                    let action_hint = match anomaly.category.as_str() {
                        "ack_sla" => Some("am robot inbox --ack-overdue".to_string()),
                        "reservation_expiry" => {
                            Some("am robot reservations --expiring=5".to_string())
                        }
                        _ => None,
                    };
                    (
                        anomaly.severity.clone(),
                        anomaly.headline.clone(),
                        action_hint,
                    )
                })
                .collect();
            if env._alerts.is_empty() && !pending_alerts.is_empty() {
                for (severity, headline, action) in pending_alerts {
                    env = env.with_alert(severity, headline, action);
                }
            }
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let (agent_id, agent_name_str) =
                resolve_agent_id(&conn, project_id, args.agent.as_deref()).ok_or_else(|| {
                    CliError::InvalidArgument(
                        "agent required for inbox — use --agent or set AGENT_NAME".to_string(),
                    )
                })?;

            let result = build_inbox(
                &conn,
                project_id,
                &project_slug,
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
            env._meta.project = Some(project_slug);
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
            let conn = crate::open_db_sync()?;
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let data = build_message(&conn, project_id, id)?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(project_slug);
            format_output_md(&env, format)?
        }
        RobotSubcommand::Search {
            query,
            kind: _,
            importance,
            since,
        } => {
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let data = build_search(
                &conn,
                project_id,
                &query,
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let agent_flag = agent_override.as_deref().or(args.agent.as_deref());
            let agent = resolve_agent_id(&conn, project_id, agent_flag);
            let (data, actions) = build_reservations(
                &conn,
                project_id,
                &project_slug,
                agent,
                all,
                conflicts,
                expiring,
            )?;
            let mut env = RobotEnvelope::new(cmd_name, format, data);
            env._meta.project = Some(project_slug);
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
                    // Verify with a lightweight query
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
            let circuits_ok = db_health.circuit_state == "closed";
            let circuit_detail = if circuits_ok {
                "All circuits closed".to_string()
            } else {
                format!(
                    "Circuit {} ({} failures)",
                    db_health.circuit_state, db_health.circuit_failures
                )
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
            let integrity_ok = integrity.failures_total == 0;
            let integrity_detail = if integrity.checks_total == 0 {
                "No checks run yet".to_string()
            } else if integrity_ok {
                format!("{} checks, all passed", integrity.checks_total)
            } else {
                format!(
                    "{} failures out of {} checks",
                    integrity.failures_total, integrity.checks_total
                )
            };
            probes.push(HealthProbe {
                name: "integrity".into(),
                status: if integrity_ok { "ok" } else { "warn" }.into(),
                latency_ms: 0.0,
                detail: integrity_detail,
            });

            // 5. Disk space probe
            let config = mcp_agent_mail_core::Config::from_env();
            let disk = mcp_agent_mail_core::disk::sample_disk(&config);
            let disk_status = disk.pressure.label();
            let free_mb = disk
                .effective_free_bytes
                .map(|b| b / (1024 * 1024))
                .unwrap_or(0);
            probes.push(HealthProbe {
                name: "disk".into(),
                status: disk_status.into(),
                latency_ms: 0.0,
                detail: format!("{free_mb} MB free"),
            });

            // Overall health
            let overall = if !db_ok {
                "unhealthy"
            } else if !circuits_ok || disk.pressure != mcp_agent_mail_core::disk::DiskPressure::Ok {
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
                        "{} integrity check failures detected",
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
            if mcp_agent_mail_db::is_full_check_due(24) {
                env = env.with_action("Run full integrity check (last check >24h ago)");
            }

            format_output(&env, format)?
        }
        RobotSubcommand::Timeline {
            since,
            kind,
            source,
        } => {
            let conn = crate::open_db_sync()?;
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
            let conn = crate::open_db_sync()?;
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let agent = resolve_agent_id(&conn, project_id, args.agent.as_deref());
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let agents = build_agents(&conn, project_id, active, sort.as_deref())?;

            #[derive(Serialize)]
            struct AgentsData {
                count: usize,
                agents: Vec<AgentRow>,
            }

            let count = agents.len();
            let mut env = RobotEnvelope::new(cmd_name, format, AgentsData { count, agents });
            env._meta.project = Some(project_slug);
            format_output(&env, format)?
        }
        RobotSubcommand::Contacts => {
            let conn = crate::open_db_sync()?;
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
            let conn = crate::open_db_sync()?;
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;

            let rows = conn
                .query_sync(
                    "SELECT m.id, m.subject, m.attachments, a.name AS sender_name
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

                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&att_json) {
                    for att in arr {
                        let atype = att
                            .get("type")
                            .or_else(|| att.get("content_type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let size = att.get("size").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        attachments.push(AttachmentRow {
                            r#type: atype,
                            size,
                            sender: sender.clone(),
                            subject: truncate_str(&subject, 60).to_string(),
                            message_id: msg_id,
                            project: project_slug.clone(),
                        });
                    }
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
            let conn = crate::open_db_sync()?;
            let (project_id, project_slug) = resolve_project(&conn, args.project.as_deref())?;
            let agent = resolve_agent_id(&conn, project_id, args.agent.as_deref());

            let (result, _action) = build_navigate(&conn, &uri, project_id, &project_slug, agent)?;

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
            env._meta.project = Some(project_slug);
            format_output(&env, format)?
        }
    };

    println!("{out}");
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

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
}
