//! Database models using sqlmodel derive macros
//!
//! These models map directly to `SQLite` tables. All datetime fields use `i64`
//! (microseconds since Unix epoch) for sqlmodel compatibility.

use serde::{Deserialize, Serialize};
use sqlmodel::Model;

use crate::timestamps::{micros_to_naive, now_micros};

// =============================================================================
// Project
// =============================================================================

/// A project represents a working directory where agents coordinate.
///
/// # Constraints
/// - `slug`: Unique, indexed. Computed from `human_key` (lowercased, safe chars).
/// - `human_key`: Indexed. MUST be an absolute directory path.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "projects")]
pub struct ProjectRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    #[sqlmodel(unique)]
    pub slug: String,

    pub human_key: String,

    /// Microseconds since Unix epoch
    pub created_at: i64,
}

impl Default for ProjectRow {
    fn default() -> Self {
        Self {
            id: None,
            slug: String::new(),
            human_key: String::new(),
            created_at: now_micros(),
        }
    }
}

impl ProjectRow {
    /// Create a new project row
    #[must_use]
    pub fn new(slug: String, human_key: String) -> Self {
        Self {
            id: None,
            slug,
            human_key,
            created_at: now_micros(),
        }
    }

    /// Get `created_at` as `NaiveDateTime`
    #[must_use]
    pub fn created_at_naive(&self) -> chrono::NaiveDateTime {
        micros_to_naive(self.created_at)
    }
}

// =============================================================================
// Product
// =============================================================================

/// A product is a logical grouping across multiple repositories/projects.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "products")]
pub struct ProductRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    #[sqlmodel(unique)]
    pub product_uid: String,

    #[sqlmodel(unique)]
    pub name: String,

    pub created_at: i64,
}

impl Default for ProductRow {
    fn default() -> Self {
        Self {
            id: None,
            product_uid: String::new(),
            name: String::new(),
            created_at: now_micros(),
        }
    }
}

// =============================================================================
// ProductProjectLink
// =============================================================================

/// Links products to projects (many-to-many).
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "product_project_links")]
pub struct ProductProjectLinkRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    pub product_id: i64,
    pub project_id: i64,
    pub created_at: i64,
}

// =============================================================================
// Agent
// =============================================================================

/// An agent represents a coding assistant or AI model working on a project.
///
/// # Naming Rules
/// Agent names MUST be adjective+noun combinations (e.g., "`GreenLake`", "`BlueDog`").
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "agents")]
pub struct AgentRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    pub project_id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    pub inception_ts: i64,
    pub last_active_ts: i64,

    /// Attachment policy: "auto" | "inline" | "file"
    #[sqlmodel(default = "'auto'")]
    pub attachments_policy: String,

    /// Contact policy: "open" | "auto" | "`contacts_only`" | "`block_all`"
    #[sqlmodel(default = "'auto'")]
    pub contact_policy: String,
}

impl Default for AgentRow {
    fn default() -> Self {
        let now = now_micros();
        Self {
            id: None,
            project_id: 0,
            name: String::new(),
            program: String::new(),
            model: String::new(),
            task_description: String::new(),
            inception_ts: now,
            last_active_ts: now,
            attachments_policy: "auto".to_string(),
            contact_policy: "auto".to_string(),
        }
    }
}

impl AgentRow {
    /// Create a new agent row
    #[must_use]
    pub fn new(project_id: i64, name: String, program: String, model: String) -> Self {
        let now = now_micros();
        Self {
            id: None,
            project_id,
            name,
            program,
            model,
            task_description: String::new(),
            inception_ts: now,
            last_active_ts: now,
            attachments_policy: "auto".to_string(),
            contact_policy: "auto".to_string(),
        }
    }

    /// Update `last_active` timestamp to now
    pub fn touch(&mut self) {
        self.last_active_ts = now_micros();
    }
}

// =============================================================================
// Message
// =============================================================================

/// A message sent between agents.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "messages")]
pub struct MessageRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    pub project_id: i64,
    pub sender_id: i64,

    #[sqlmodel(nullable)]
    pub thread_id: Option<String>,

    pub subject: String,
    pub body_md: String,

    #[sqlmodel(default = "'normal'")]
    pub importance: String,

    #[sqlmodel(default = "0")]
    pub ack_required: i64, // SQLite doesn't have bool, use 0/1

    pub created_ts: i64,

    /// JSON array of attachment metadata
    #[sqlmodel(default = "'[]'")]
    pub attachments: String,
}

impl Default for MessageRow {
    fn default() -> Self {
        Self {
            id: None,
            project_id: 0,
            sender_id: 0,
            thread_id: None,
            subject: String::new(),
            body_md: String::new(),
            importance: "normal".to_string(),
            ack_required: 0,
            created_ts: now_micros(),
            attachments: "[]".to_string(),
        }
    }
}

impl MessageRow {
    #[must_use]
    pub const fn ack_required_bool(&self) -> bool {
        self.ack_required != 0
    }

    pub fn set_ack_required(&mut self, required: bool) {
        self.ack_required = i64::from(required);
    }
}

// =============================================================================
// MessageRecipient
// =============================================================================

/// Links messages to recipient agents (many-to-many).
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "message_recipients")]
pub struct MessageRecipientRow {
    // Composite primary key: (message_id, agent_id)
    pub message_id: i64,
    pub agent_id: i64,

    /// Recipient kind: "to" | "cc" | "bcc"
    #[sqlmodel(default = "'to'")]
    pub kind: String,

    #[sqlmodel(nullable)]
    pub read_ts: Option<i64>,

    #[sqlmodel(nullable)]
    pub ack_ts: Option<i64>,
}

impl Default for MessageRecipientRow {
    fn default() -> Self {
        Self {
            message_id: 0,
            agent_id: 0,
            kind: "to".to_string(),
            read_ts: None,
            ack_ts: None,
        }
    }
}

// =============================================================================
// FileReservation
// =============================================================================

/// An advisory file lock (lease) on file paths or glob patterns.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "file_reservations")]
pub struct FileReservationRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    pub project_id: i64,
    pub agent_id: i64,
    pub path_pattern: String,

    #[sqlmodel(default = "1")]
    pub exclusive: i64, // SQLite bool as 0/1

    #[sqlmodel(default = "''")]
    pub reason: String,

    pub created_ts: i64,
    pub expires_ts: i64,

    #[sqlmodel(nullable)]
    pub released_ts: Option<i64>,
}

impl Default for FileReservationRow {
    fn default() -> Self {
        let now = now_micros();
        Self {
            id: None,
            project_id: 0,
            agent_id: 0,
            path_pattern: String::new(),
            exclusive: 1,
            reason: String::new(),
            created_ts: now,
            expires_ts: now,
            released_ts: None,
        }
    }
}

impl FileReservationRow {
    #[must_use]
    pub const fn is_exclusive(&self) -> bool {
        self.exclusive != 0
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.released_ts.is_none() && self.expires_ts > now_micros()
    }
}

// =============================================================================
// AgentLink
// =============================================================================

/// A contact link between two agents (possibly cross-project).
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "agent_links")]
pub struct AgentLinkRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    pub a_project_id: i64,
    pub a_agent_id: i64,
    pub b_project_id: i64,
    pub b_agent_id: i64,

    /// Status: "pending" | "approved" | "blocked"
    #[sqlmodel(default = "'pending'")]
    pub status: String,

    #[sqlmodel(default = "''")]
    pub reason: String,

    pub created_ts: i64,
    pub updated_ts: i64,

    #[sqlmodel(nullable)]
    pub expires_ts: Option<i64>,
}

impl Default for AgentLinkRow {
    fn default() -> Self {
        let now = now_micros();
        Self {
            id: None,
            a_project_id: 0,
            a_agent_id: 0,
            b_project_id: 0,
            b_agent_id: 0,
            status: "pending".to_string(),
            reason: String::new(),
            created_ts: now,
            updated_ts: now,
            expires_ts: None,
        }
    }
}

// =============================================================================
// ProjectSiblingSuggestion
// =============================================================================

/// LLM-ranked suggestion for related projects.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "project_sibling_suggestions")]
pub struct ProjectSiblingSuggestionRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    pub project_a_id: i64,
    pub project_b_id: i64,
    pub score: f64,

    /// Status: "suggested" | "confirmed" | "dismissed"
    #[sqlmodel(default = "'suggested'")]
    pub status: String,

    #[sqlmodel(default = "''")]
    pub rationale: String,

    pub created_ts: i64,
    pub evaluated_ts: i64,

    #[sqlmodel(nullable)]
    pub confirmed_ts: Option<i64>,

    #[sqlmodel(nullable)]
    pub dismissed_ts: Option<i64>,
}

impl Default for ProjectSiblingSuggestionRow {
    fn default() -> Self {
        let now = now_micros();
        Self {
            id: None,
            project_a_id: 0,
            project_b_id: 0,
            score: 0.0,
            status: "suggested".to_string(),
            rationale: String::new(),
            created_ts: now,
            evaluated_ts: now,
            confirmed_ts: None,
            dismissed_ts: None,
        }
    }
}

// =============================================================================
// Inbox Stats (materialized aggregate counters)
// =============================================================================

/// Materialized aggregate counters for an agent's inbox.
///
/// Maintained by `SQLite` triggers on `message_recipients` so that inbox
/// stats queries are O(1) instead of scanning the recipients table.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "inbox_stats")]
pub struct InboxStatsRow {
    /// The agent whose inbox these stats describe.
    #[sqlmodel(primary_key)]
    pub agent_id: i64,

    /// Total messages delivered to this agent.
    #[sqlmodel(default = "0")]
    pub total_count: i64,

    /// Messages not yet marked as read.
    #[sqlmodel(default = "0")]
    pub unread_count: i64,

    /// Messages with `ack_required=1` that haven't been acknowledged.
    #[sqlmodel(default = "0")]
    pub ack_pending_count: i64,

    /// Timestamp of the most recent message (microseconds since epoch).
    #[sqlmodel(nullable)]
    pub last_message_ts: Option<i64>,
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default, clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::timestamps::now_micros;

    // ── ProjectRow ──────────────────────────────────────────────────

    #[test]
    fn project_row_default_has_recent_timestamp() {
        let proj = ProjectRow::default();
        assert!(proj.id.is_none());
        assert!(proj.slug.is_empty());
        let now = now_micros();
        assert!((now - proj.created_at).abs() < 1_000_000);
    }

    #[test]
    fn project_row_new_sets_fields() {
        let proj = ProjectRow::new("my-proj".into(), "/data/my-proj".into());
        assert_eq!(proj.slug, "my-proj");
        assert_eq!(proj.human_key, "/data/my-proj");
        assert!(proj.id.is_none());
    }

    #[test]
    fn project_row_created_at_naive_roundtrip() {
        let mut proj = ProjectRow::default();
        proj.created_at = 1_705_320_000_000_000; // 2024-01-15 12:00:00 UTC
        let dt = proj.created_at_naive();
        assert_eq!(dt.and_utc().timestamp(), 1_705_320_000);
    }

    #[test]
    fn project_row_created_at_naive_epoch() {
        let mut proj = ProjectRow::default();
        proj.created_at = 0;
        let dt = proj.created_at_naive();
        assert_eq!(dt.and_utc().timestamp(), 0);
    }

    // ── AgentRow ────────────────────────────────────────────────────

    #[test]
    fn agent_row_default_has_matching_timestamps() {
        let agent = AgentRow::default();
        assert_eq!(agent.inception_ts, agent.last_active_ts);
        assert_eq!(agent.attachments_policy, "auto");
        assert_eq!(agent.contact_policy, "auto");
    }

    #[test]
    fn agent_row_new_sets_fields() {
        let agent = AgentRow::new(
            42,
            "BlueLake".into(),
            "claude-code".into(),
            "opus-4.6".into(),
        );
        assert_eq!(agent.project_id, 42);
        assert_eq!(agent.name, "BlueLake");
        assert_eq!(agent.program, "claude-code");
        assert_eq!(agent.model, "opus-4.6");
        assert!(agent.task_description.is_empty());
    }

    #[test]
    fn agent_row_touch_advances_timestamp() {
        let mut agent = AgentRow::default();
        let original = agent.last_active_ts;
        std::thread::sleep(std::time::Duration::from_millis(1));
        agent.touch();
        assert!(agent.last_active_ts >= original);
    }

    // ── MessageRow ──────────────────────────────────────────────────

    #[test]
    fn message_row_default_values() {
        let msg = MessageRow::default();
        assert!(msg.id.is_none());
        assert_eq!(msg.importance, "normal");
        assert_eq!(msg.ack_required, 0);
        assert_eq!(msg.attachments, "[]");
        assert!(msg.thread_id.is_none());
    }

    #[test]
    fn message_row_ack_required_bool() {
        let mut msg = MessageRow::default();
        assert!(!msg.ack_required_bool());

        msg.ack_required = 1;
        assert!(msg.ack_required_bool());

        msg.ack_required = 42; // any non-zero is true
        assert!(msg.ack_required_bool());
    }

    #[test]
    fn message_row_set_ack_required() {
        let mut msg = MessageRow::default();

        msg.set_ack_required(true);
        assert_eq!(msg.ack_required, 1);
        assert!(msg.ack_required_bool());

        msg.set_ack_required(false);
        assert_eq!(msg.ack_required, 0);
        assert!(!msg.ack_required_bool());
    }

    // ── MessageRecipientRow ─────────────────────────────────────────

    #[test]
    fn message_recipient_default() {
        let recip = MessageRecipientRow::default();
        assert_eq!(recip.kind, "to");
        assert!(recip.read_ts.is_none());
        assert!(recip.ack_ts.is_none());
    }

    // ── FileReservationRow ──────────────────────────────────────────

    #[test]
    fn file_reservation_default_is_exclusive() {
        let resv = FileReservationRow::default();
        assert!(resv.is_exclusive());
        assert!(resv.released_ts.is_none());
    }

    #[test]
    fn file_reservation_is_exclusive_logic() {
        let mut resv = FileReservationRow::default();
        assert!(resv.is_exclusive());

        resv.exclusive = 0;
        assert!(!resv.is_exclusive());

        resv.exclusive = 1;
        assert!(resv.is_exclusive());
    }

    #[test]
    fn file_reservation_is_active_released() {
        let mut resv = FileReservationRow::default();
        resv.expires_ts = now_micros() + 60_000_000;
        assert!(resv.is_active());

        resv.released_ts = Some(now_micros());
        assert!(!resv.is_active());
    }

    #[test]
    fn file_reservation_is_active_expired() {
        let mut resv = FileReservationRow::default();
        resv.expires_ts = now_micros() - 1_000_000;
        assert!(!resv.is_active());
    }

    // ── AgentLinkRow ────────────────────────────────────────────────

    #[test]
    fn agent_link_default() {
        let link = AgentLinkRow::default();
        assert_eq!(link.status, "pending");
        assert!(link.reason.is_empty());
        assert!(link.expires_ts.is_none());
        assert_eq!(link.created_ts, link.updated_ts);
    }

    // ── ProjectSiblingSuggestionRow ─────────────────────────────────

    #[test]
    fn sibling_suggestion_default() {
        let sug = ProjectSiblingSuggestionRow::default();
        assert_eq!(sug.status, "suggested");
        assert_eq!(sug.score, 0.0);
        assert!(sug.confirmed_ts.is_none());
        assert!(sug.dismissed_ts.is_none());
    }

    // ── InboxStatsRow ───────────────────────────────────────────────

    #[test]
    fn inbox_stats_fields() {
        let stats = InboxStatsRow {
            agent_id: 1,
            total_count: 10,
            unread_count: 3,
            ack_pending_count: 2,
            last_message_ts: Some(now_micros()),
        };
        assert_eq!(stats.total_count, 10);
        assert_eq!(stats.unread_count, 3);
        assert_eq!(stats.ack_pending_count, 2);
        assert!(stats.last_message_ts.is_some());
    }

    // ── Serialization roundtrips ────────────────────────────────────

    #[test]
    fn project_row_serde_roundtrip() {
        let proj = ProjectRow::new("test-slug".into(), "/data/test".into());
        let json = serde_json::to_string(&proj).unwrap();
        let proj2: ProjectRow = serde_json::from_str(&json).unwrap();
        assert_eq!(proj.slug, proj2.slug);
        assert_eq!(proj.human_key, proj2.human_key);
        assert_eq!(proj.created_at, proj2.created_at);
    }

    #[test]
    fn agent_row_serde_roundtrip() {
        let agent = AgentRow::new(1, "RedFox".into(), "claude".into(), "opus".into());
        let json = serde_json::to_string(&agent).unwrap();
        let agent2: AgentRow = serde_json::from_str(&json).unwrap();
        assert_eq!(agent.name, agent2.name);
        assert_eq!(agent.program, agent2.program);
        assert_eq!(agent.attachments_policy, agent2.attachments_policy);
    }

    #[test]
    fn message_row_serde_roundtrip() {
        let mut msg = MessageRow::default();
        msg.subject = "Hello".into();
        msg.body_md = "World".into();
        msg.thread_id = Some("TKT-123".into());
        msg.set_ack_required(true);
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: MessageRow = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.subject, msg2.subject);
        assert_eq!(msg.thread_id, msg2.thread_id);
        assert!(msg2.ack_required_bool());
    }

    #[test]
    fn file_reservation_serde_roundtrip() {
        let mut resv = FileReservationRow::default();
        resv.path_pattern = "src/**/*.rs".into();
        resv.reason = "editing".into();
        let json = serde_json::to_string(&resv).unwrap();
        let resv2: FileReservationRow = serde_json::from_str(&json).unwrap();
        assert_eq!(resv.path_pattern, resv2.path_pattern);
        assert!(resv2.is_exclusive());
    }

    // ── ProductRow ──────────────────────────────────────────────────

    #[test]
    fn product_row_default() {
        let prod = ProductRow::default();
        assert!(prod.id.is_none());
        assert!(prod.product_uid.is_empty());
        assert!(prod.name.is_empty());
    }

    // ── Timestamp consistency ───────────────────────────────────────

    #[test]
    fn all_defaults_use_recent_timestamps() {
        let now = now_micros();
        let tolerance: u64 = 2_000_000; // 2 seconds

        let proj = ProjectRow::default();
        assert!((now - proj.created_at).unsigned_abs() < tolerance);

        let agent = AgentRow::default();
        assert!((now - agent.inception_ts).unsigned_abs() < tolerance);

        let msg = MessageRow::default();
        assert!((now - msg.created_ts).unsigned_abs() < tolerance);

        let resv = FileReservationRow::default();
        assert!((now - resv.created_ts).unsigned_abs() < tolerance);

        let link = AgentLinkRow::default();
        assert!((now - link.created_ts).unsigned_abs() < tolerance);
    }
}
