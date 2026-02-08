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
