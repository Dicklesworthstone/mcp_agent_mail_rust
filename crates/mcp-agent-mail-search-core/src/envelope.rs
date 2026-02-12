//! Document envelope model and DB-to-index mapping
//!
//! [`SearchDocumentEnvelope`] wraps a [`Document`] with provenance, visibility,
//! and version metadata required for filtering, scope enforcement, and
//! incremental index updates.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::document::{DocId, DocKind, Document};

/// Version stamp for tracking incremental changes.
///
/// Each envelope carries a monotonically increasing version that the index
/// uses to decide whether a re-index is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DocVersion(pub i64);

impl DocVersion {
    /// Create a new version from a timestamp in microseconds
    #[must_use]
    pub const fn from_micros(ts: i64) -> Self {
        Self(ts)
    }
}

/// Visibility scope for a document.
///
/// Controls which queries can see this document based on project/product context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Visibility {
    /// The project ID this document belongs to
    pub project_id: i64,
    /// Optional product IDs this document is associated with (via project links)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub product_ids: Vec<i64>,
}

/// Provenance metadata tracking where the document originated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// The DB entity type that produced this document
    pub source_kind: DocKind,
    /// The original DB row ID
    pub source_id: DocId,
    /// The agent who created the source entity (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_agent_id: Option<i64>,
    /// The author agent's name (denormalized for display)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_name: Option<String>,
}

/// A document envelope wrapping a [`Document`] with all metadata needed for
/// indexing, filtering, scope enforcement, and incremental updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDocumentEnvelope {
    /// The core document to be indexed
    pub document: Document,
    /// Version stamp for incremental update tracking
    pub version: DocVersion,
    /// Visibility scope for query-time filtering
    pub visibility: Visibility,
    /// Provenance metadata for audit and display
    pub provenance: Provenance,
}

impl SearchDocumentEnvelope {
    /// Returns the deterministic index ID for this document.
    ///
    /// Format: `{kind}:{source_id}` (e.g., `message:42`, `agent:7`)
    #[must_use]
    pub fn index_key(&self) -> String {
        format!("{}:{}", self.document.kind, self.document.id)
    }
}

// =============================================================================
// DB-to-Document mapping helpers
// =============================================================================

/// Input data for mapping a message row to a search document envelope.
///
/// This avoids a direct dependency on `mcp-agent-mail-db` — callers provide
/// the fields from their query results.
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub sender_name: Option<String>,
    pub thread_id: Option<String>,
    pub subject: String,
    pub body_md: String,
    pub importance: String,
    pub ack_required: bool,
    /// Microseconds since epoch
    pub created_ts: i64,
    /// Product IDs this message's project is linked to
    pub product_ids: Vec<i64>,
}

/// Input data for mapping an agent row to a search document envelope.
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    /// Microseconds since epoch
    pub inception_ts: i64,
    /// Microseconds since epoch
    pub last_active_ts: i64,
    /// Product IDs this agent's project is linked to
    pub product_ids: Vec<i64>,
}

/// Input data for mapping a project row to a search document envelope.
#[derive(Debug, Clone)]
pub struct ProjectRow {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    /// Microseconds since epoch
    pub created_at: i64,
    /// Product IDs linked to this project
    pub product_ids: Vec<i64>,
}

/// Map a message DB row to a [`SearchDocumentEnvelope`].
#[must_use]
pub fn message_to_envelope(row: &MessageRow) -> SearchDocumentEnvelope {
    let mut metadata = HashMap::new();
    metadata.insert("importance".to_owned(), serde_json::json!(row.importance));
    metadata.insert(
        "ack_required".to_owned(),
        serde_json::json!(row.ack_required),
    );
    if let Some(ref tid) = row.thread_id {
        metadata.insert("thread_id".to_owned(), serde_json::json!(tid));
    }
    if let Some(ref sender) = row.sender_name {
        metadata.insert("sender".to_owned(), serde_json::json!(sender));
    }
    metadata.insert("sender_id".to_owned(), serde_json::json!(row.sender_id));

    SearchDocumentEnvelope {
        document: Document {
            id: row.id,
            kind: DocKind::Message,
            body: row.body_md.clone(),
            title: row.subject.clone(),
            project_id: Some(row.project_id),
            created_ts: row.created_ts,
            metadata,
        },
        version: DocVersion::from_micros(row.created_ts),
        visibility: Visibility {
            project_id: row.project_id,
            product_ids: row.product_ids.clone(),
        },
        provenance: Provenance {
            source_kind: DocKind::Message,
            source_id: row.id,
            author_agent_id: Some(row.sender_id),
            author_name: row.sender_name.clone(),
        },
    }
}

/// Map an agent DB row to a [`SearchDocumentEnvelope`].
#[must_use]
pub fn agent_to_envelope(row: &AgentRow) -> SearchDocumentEnvelope {
    let mut metadata = HashMap::new();
    metadata.insert("program".to_owned(), serde_json::json!(row.program));
    metadata.insert("model".to_owned(), serde_json::json!(row.model));

    // Agent body combines name, program, model, and task description
    let body = format!(
        "{} ({}/{})\n{}",
        row.name, row.program, row.model, row.task_description
    );

    SearchDocumentEnvelope {
        document: Document {
            id: row.id,
            kind: DocKind::Agent,
            body,
            title: row.name.clone(),
            project_id: Some(row.project_id),
            created_ts: row.inception_ts,
            metadata,
        },
        version: DocVersion::from_micros(row.last_active_ts),
        visibility: Visibility {
            project_id: row.project_id,
            product_ids: row.product_ids.clone(),
        },
        provenance: Provenance {
            source_kind: DocKind::Agent,
            source_id: row.id,
            author_agent_id: None,
            author_name: None,
        },
    }
}

/// Map a project DB row to a [`SearchDocumentEnvelope`].
#[must_use]
pub fn project_to_envelope(row: &ProjectRow) -> SearchDocumentEnvelope {
    let mut metadata = HashMap::new();
    metadata.insert("slug".to_owned(), serde_json::json!(row.slug));

    SearchDocumentEnvelope {
        document: Document {
            id: row.id,
            kind: DocKind::Project,
            body: row.human_key.clone(),
            title: row.slug.clone(),
            project_id: Some(row.id),
            created_ts: row.created_at,
            metadata,
        },
        version: DocVersion::from_micros(row.created_at),
        visibility: Visibility {
            project_id: row.id,
            product_ids: row.product_ids.clone(),
        },
        provenance: Provenance {
            source_kind: DocKind::Project,
            source_id: row.id,
            author_agent_id: None,
            author_name: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message_row() -> MessageRow {
        MessageRow {
            id: 42,
            project_id: 1,
            sender_id: 7,
            sender_name: Some("BlueLake".to_owned()),
            thread_id: Some("br-123".to_owned()),
            subject: "Migration plan".to_owned(),
            body_md: "Here is the plan for DB migration...".to_owned(),
            importance: "high".to_owned(),
            ack_required: true,
            created_ts: 1_700_000_000_000_000,
            product_ids: vec![10, 20],
        }
    }

    fn sample_agent_row() -> AgentRow {
        AgentRow {
            id: 7,
            project_id: 1,
            name: "BlueLake".to_owned(),
            program: "claude-code".to_owned(),
            model: "opus-4.6".to_owned(),
            task_description: "Working on search v3".to_owned(),
            inception_ts: 1_699_000_000_000_000,
            last_active_ts: 1_700_000_000_000_000,
            product_ids: vec![10],
        }
    }

    fn sample_project_row() -> ProjectRow {
        ProjectRow {
            id: 1,
            slug: "my-project".to_owned(),
            human_key: "/data/projects/my-project".to_owned(),
            created_at: 1_698_000_000_000_000,
            product_ids: vec![],
        }
    }

    // ── Message mapping tests ──

    #[test]
    fn message_envelope_basic_fields() {
        let row = sample_message_row();
        let env = message_to_envelope(&row);
        assert_eq!(env.document.id, 42);
        assert_eq!(env.document.kind, DocKind::Message);
        assert_eq!(env.document.title, "Migration plan");
        assert_eq!(env.document.body, "Here is the plan for DB migration...");
        assert_eq!(env.document.project_id, Some(1));
        assert_eq!(env.document.created_ts, 1_700_000_000_000_000);
    }

    #[test]
    fn message_envelope_metadata() {
        let row = sample_message_row();
        let env = message_to_envelope(&row);
        assert_eq!(env.document.metadata["importance"], "high");
        assert_eq!(env.document.metadata["ack_required"], true);
        assert_eq!(env.document.metadata["thread_id"], "br-123");
        assert_eq!(env.document.metadata["sender"], "BlueLake");
        assert_eq!(env.document.metadata["sender_id"], 7);
    }

    #[test]
    fn message_envelope_version() {
        let row = sample_message_row();
        let env = message_to_envelope(&row);
        assert_eq!(env.version, DocVersion::from_micros(1_700_000_000_000_000));
    }

    #[test]
    fn message_envelope_visibility() {
        let row = sample_message_row();
        let env = message_to_envelope(&row);
        assert_eq!(env.visibility.project_id, 1);
        assert_eq!(env.visibility.product_ids, vec![10, 20]);
    }

    #[test]
    fn message_envelope_provenance() {
        let row = sample_message_row();
        let env = message_to_envelope(&row);
        assert_eq!(env.provenance.source_kind, DocKind::Message);
        assert_eq!(env.provenance.source_id, 42);
        assert_eq!(env.provenance.author_agent_id, Some(7));
        assert_eq!(env.provenance.author_name.as_deref(), Some("BlueLake"));
    }

    #[test]
    fn message_envelope_index_key() {
        let row = sample_message_row();
        let env = message_to_envelope(&row);
        assert_eq!(env.index_key(), "message:42");
    }

    #[test]
    fn message_envelope_no_thread() {
        let mut row = sample_message_row();
        row.thread_id = None;
        let env = message_to_envelope(&row);
        assert!(!env.document.metadata.contains_key("thread_id"));
    }

    #[test]
    fn message_envelope_no_sender_name() {
        let mut row = sample_message_row();
        row.sender_name = None;
        let env = message_to_envelope(&row);
        assert!(!env.document.metadata.contains_key("sender"));
    }

    // ── Agent mapping tests ──

    #[test]
    fn agent_envelope_basic_fields() {
        let row = sample_agent_row();
        let env = agent_to_envelope(&row);
        assert_eq!(env.document.id, 7);
        assert_eq!(env.document.kind, DocKind::Agent);
        assert_eq!(env.document.title, "BlueLake");
        assert!(env.document.body.contains("BlueLake"));
        assert!(env.document.body.contains("claude-code"));
        assert!(env.document.body.contains("opus-4.6"));
        assert!(env.document.body.contains("Working on search v3"));
    }

    #[test]
    fn agent_envelope_version_uses_last_active() {
        let row = sample_agent_row();
        let env = agent_to_envelope(&row);
        // Version should be based on last_active_ts (not inception_ts)
        assert_eq!(env.version, DocVersion::from_micros(1_700_000_000_000_000));
    }

    #[test]
    fn agent_envelope_metadata() {
        let row = sample_agent_row();
        let env = agent_to_envelope(&row);
        assert_eq!(env.document.metadata["program"], "claude-code");
        assert_eq!(env.document.metadata["model"], "opus-4.6");
    }

    #[test]
    fn agent_envelope_provenance() {
        let row = sample_agent_row();
        let env = agent_to_envelope(&row);
        assert_eq!(env.provenance.source_kind, DocKind::Agent);
        assert_eq!(env.provenance.source_id, 7);
        assert!(env.provenance.author_agent_id.is_none());
    }

    #[test]
    fn agent_envelope_index_key() {
        let row = sample_agent_row();
        let env = agent_to_envelope(&row);
        assert_eq!(env.index_key(), "agent:7");
    }

    // ── Project mapping tests ──

    #[test]
    fn project_envelope_basic_fields() {
        let row = sample_project_row();
        let env = project_to_envelope(&row);
        assert_eq!(env.document.id, 1);
        assert_eq!(env.document.kind, DocKind::Project);
        assert_eq!(env.document.title, "my-project");
        assert_eq!(env.document.body, "/data/projects/my-project");
    }

    #[test]
    fn project_envelope_metadata() {
        let row = sample_project_row();
        let env = project_to_envelope(&row);
        assert_eq!(env.document.metadata["slug"], "my-project");
    }

    #[test]
    fn project_envelope_provenance() {
        let row = sample_project_row();
        let env = project_to_envelope(&row);
        assert_eq!(env.provenance.source_kind, DocKind::Project);
        assert_eq!(env.provenance.source_id, 1);
        assert!(env.provenance.author_agent_id.is_none());
    }

    #[test]
    fn project_envelope_index_key() {
        let row = sample_project_row();
        let env = project_to_envelope(&row);
        assert_eq!(env.index_key(), "project:1");
    }

    #[test]
    fn project_envelope_empty_products() {
        let row = sample_project_row();
        let env = project_to_envelope(&row);
        assert!(env.visibility.product_ids.is_empty());
    }

    // ── Serde roundtrip tests ──

    #[test]
    fn envelope_serde_roundtrip() {
        let row = sample_message_row();
        let env = message_to_envelope(&row);
        let json = serde_json::to_string(&env).unwrap();
        let env2: SearchDocumentEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env2.document.id, env.document.id);
        assert_eq!(env2.document.kind, env.document.kind);
        assert_eq!(env2.version, env.version);
        assert_eq!(env2.visibility.project_id, env.visibility.project_id);
    }

    #[test]
    fn doc_version_ordering() {
        let v1 = DocVersion::from_micros(100);
        let v2 = DocVersion::from_micros(200);
        assert!(v1 < v2);
        assert_eq!(v1, DocVersion(100));
    }

    #[test]
    fn visibility_empty_products_skipped_in_json() {
        let vis = Visibility {
            project_id: 1,
            product_ids: Vec::new(),
        };
        let json = serde_json::to_string(&vis).unwrap();
        assert!(!json.contains("product_ids"));
    }
}
