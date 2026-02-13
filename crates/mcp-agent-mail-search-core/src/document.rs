//! Document model for the search index
//!
//! Documents are the unit of indexing. Each document represents a searchable
//! entity (message, agent profile, project metadata, etc.).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unique identifier for a document in the search index
pub type DocId = i64;

/// The kind of document (maps to different index schemas)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// A message (subject + body)
    Message,
    /// An agent profile
    Agent,
    /// A project
    Project,
    /// A thread (aggregated from messages)
    Thread,
}

impl std::fmt::Display for DocKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message => write!(f, "message"),
            Self::Agent => write!(f, "agent"),
            Self::Project => write!(f, "project"),
            Self::Thread => write!(f, "thread"),
        }
    }
}

/// A document to be indexed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Unique ID within the document kind
    pub id: DocId,
    /// What kind of entity this document represents
    pub kind: DocKind,
    /// Primary text content (e.g., message body, agent description)
    pub body: String,
    /// Secondary text content (e.g., message subject, agent name)
    pub title: String,
    /// The project this document belongs to (for scoping)
    pub project_id: Option<i64>,
    /// Timestamp in microseconds since epoch
    pub created_ts: i64,
    /// Structured metadata for faceted search
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Describes a change to a document for incremental index updates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocChange {
    /// A new document was created or an existing one was updated
    Upsert(Document),
    /// A document was deleted
    Delete {
        /// The ID of the deleted document
        id: DocId,
        /// The kind of the deleted document
        kind: DocKind,
    },
}

impl DocChange {
    /// Returns the document ID affected by this change
    #[must_use]
    pub const fn doc_id(&self) -> DocId {
        match self {
            Self::Upsert(doc) => doc.id,
            Self::Delete { id, .. } => *id,
        }
    }

    /// Returns the document kind affected by this change
    #[must_use]
    pub const fn doc_kind(&self) -> DocKind {
        match self {
            Self::Upsert(doc) => doc.kind,
            Self::Delete { kind, .. } => *kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> Document {
        Document {
            id: 1,
            kind: DocKind::Message,
            body: "Hello world".to_owned(),
            title: "Greetings".to_owned(),
            project_id: Some(42),
            created_ts: 1_700_000_000_000_000,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn doc_kind_display() {
        assert_eq!(DocKind::Message.to_string(), "message");
        assert_eq!(DocKind::Agent.to_string(), "agent");
        assert_eq!(DocKind::Project.to_string(), "project");
        assert_eq!(DocKind::Thread.to_string(), "thread");
    }

    #[test]
    fn doc_kind_serde_roundtrip() {
        for kind in [
            DocKind::Message,
            DocKind::Agent,
            DocKind::Project,
            DocKind::Thread,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let kind2: DocKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, kind2);
        }
    }

    #[test]
    fn doc_kind_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&DocKind::Message).unwrap(),
            "\"message\""
        );
        assert_eq!(serde_json::to_string(&DocKind::Agent).unwrap(), "\"agent\"");
    }

    #[test]
    fn document_serde_roundtrip() {
        let doc = sample_doc();
        let json = serde_json::to_string(&doc).unwrap();
        let doc2: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(doc2.id, doc.id);
        assert_eq!(doc2.kind, doc.kind);
        assert_eq!(doc2.body, doc.body);
        assert_eq!(doc2.title, doc.title);
        assert_eq!(doc2.project_id, doc.project_id);
        assert_eq!(doc2.created_ts, doc.created_ts);
    }

    #[test]
    fn doc_change_upsert_accessors() {
        let change = DocChange::Upsert(sample_doc());
        assert_eq!(change.doc_id(), 1);
        assert_eq!(change.doc_kind(), DocKind::Message);
    }

    #[test]
    fn doc_change_delete_accessors() {
        let change = DocChange::Delete {
            id: 99,
            kind: DocKind::Agent,
        };
        assert_eq!(change.doc_id(), 99);
        assert_eq!(change.doc_kind(), DocKind::Agent);
    }

    #[test]
    fn doc_change_serde_roundtrip() {
        let upsert = DocChange::Upsert(sample_doc());
        let json = serde_json::to_string(&upsert).unwrap();
        let upsert2: DocChange = serde_json::from_str(&json).unwrap();
        assert_eq!(upsert2.doc_id(), 1);

        let delete = DocChange::Delete {
            id: 7,
            kind: DocKind::Thread,
        };
        let json2 = serde_json::to_string(&delete).unwrap();
        let delete2: DocChange = serde_json::from_str(&json2).unwrap();
        assert_eq!(delete2.doc_id(), 7);
        assert_eq!(delete2.doc_kind(), DocKind::Thread);
    }

    // ── DocKind Hash ────────────────────────────────────────────────────

    #[test]
    fn doc_kind_hash_distinct_variants() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(DocKind::Message);
        set.insert(DocKind::Agent);
        set.insert(DocKind::Project);
        set.insert(DocKind::Thread);
        assert_eq!(set.len(), 4);
    }

    // ── Document metadata ───────────────────────────────────────────────

    #[test]
    fn document_with_metadata_serde() {
        let mut doc = sample_doc();
        doc.metadata
            .insert("sender".to_owned(), serde_json::json!("AgentX"));
        doc.metadata
            .insert("importance".to_owned(), serde_json::json!("high"));
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back.metadata["sender"], "AgentX");
        assert_eq!(back.metadata["importance"], "high");
    }

    #[test]
    fn document_project_id_none() {
        let mut doc = sample_doc();
        doc.project_id = None;
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert!(back.project_id.is_none());
    }

    // ── Document Clone + Debug ──────────────────────────────────────────

    #[test]
    fn document_clone() {
        let doc = sample_doc();
        let cloned = doc.clone();
        assert_eq!(cloned.id, doc.id);
        assert_eq!(cloned.kind, doc.kind);
        assert_eq!(cloned.body, doc.body);
    }

    #[test]
    fn document_debug() {
        let doc = sample_doc();
        let debug = format!("{doc:?}");
        assert!(debug.contains("Document"));
    }

    // ── DocChange Clone + Debug ─────────────────────────────────────────

    #[test]
    fn doc_change_clone() {
        let change = DocChange::Upsert(sample_doc());
        let cloned = change.clone();
        assert_eq!(cloned.doc_id(), change.doc_id());
    }

    #[test]
    fn doc_change_debug() {
        let change = DocChange::Delete {
            id: 5,
            kind: DocKind::Agent,
        };
        let debug = format!("{change:?}");
        assert!(debug.contains("Delete"));
        assert!(debug.contains("Agent"));
    }

    // ── DocChange all kinds ─────────────────────────────────────────────

    #[test]
    fn doc_change_delete_all_kinds() {
        for kind in [
            DocKind::Message,
            DocKind::Agent,
            DocKind::Project,
            DocKind::Thread,
        ] {
            let change = DocChange::Delete { id: 1, kind };
            assert_eq!(change.doc_kind(), kind);
        }
    }

    // ── DocKind trait coverage ─────────────────────────────────────────

    #[test]
    fn doc_kind_debug() {
        let debug = format!("{:?}", DocKind::Message);
        assert!(debug.contains("Message"));
    }

    #[test]
    fn doc_kind_clone_copy_eq() {
        let a = DocKind::Agent;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(a, DocKind::Project);
    }

    // ── Document edge cases ───────────────────────────────────────────

    #[test]
    fn document_empty_fields() {
        let doc = Document {
            id: 0,
            kind: DocKind::Thread,
            body: String::new(),
            title: String::new(),
            project_id: None,
            created_ts: 0,
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 0);
        assert!(back.body.is_empty());
        assert!(back.title.is_empty());
    }

    #[test]
    fn document_negative_timestamp() {
        let doc = Document {
            id: 1,
            kind: DocKind::Message,
            body: "pre-epoch".to_owned(),
            title: "old".to_owned(),
            project_id: None,
            created_ts: -1_000_000,
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back.created_ts, -1_000_000);
    }

    // ── DocChange upsert for all kinds ────────────────────────────────

    #[test]
    fn doc_change_upsert_all_kinds() {
        for kind in [
            DocKind::Message,
            DocKind::Agent,
            DocKind::Project,
            DocKind::Thread,
        ] {
            let doc = Document {
                id: 42,
                kind,
                body: String::new(),
                title: String::new(),
                project_id: None,
                created_ts: 0,
                metadata: HashMap::new(),
            };
            let change = DocChange::Upsert(doc);
            assert_eq!(change.doc_id(), 42);
            assert_eq!(change.doc_kind(), kind);
        }
    }

    // ── DocId type alias ──────────────────────────────────────────────

    #[test]
    fn doc_id_is_i64() {
        let id: DocId = -1;
        assert_eq!(id, -1_i64);
    }
}
