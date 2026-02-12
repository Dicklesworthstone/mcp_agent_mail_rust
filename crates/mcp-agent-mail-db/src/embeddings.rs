//! Embedding persistence layer for semantic search.
//!
//! This module provides schema, models, and queries for storing and retrieving
//! text embeddings. Embeddings are versioned by model ID and content hash to
//! support invalidation when source text changes or models are upgraded.
//!
//! # Schema
//!
//! The `embeddings` table stores:
//! - `doc_id` + `doc_kind`: Reference to the source document
//! - `model_id`: The embedding model that generated this vector
//! - `content_hash`: SHA-256 hash of the canonicalized source text
//! - `vector_blob`: The embedding vector as a BLOB (packed f32 little-endian)
//! - `dimension`: Vector dimension (for validation)
//! - `created_ts`: When the embedding was generated
//!
//! # Invalidation
//!
//! An embedding is invalid if:
//! 1. The source document's content has changed (different `content_hash`)
//! 2. The model has been upgraded (different `model_id`)
//! 3. The model configuration has changed (model version in `model_id`)
//!
//! The [`is_valid`] function checks validity by comparing hashes.
//! The [`invalidate_stale`] function bulk-deletes stale embeddings.

use serde::{Deserialize, Serialize};
use sqlmodel::Model;

use crate::timestamps::now_micros;

// =============================================================================
// Schema SQL
// =============================================================================

/// SQL for creating the embeddings table.
///
/// This should be added to the migration list for databases that enable
/// semantic search.
pub const CREATE_EMBEDDINGS_TABLE_SQL: &str = r"
-- Embeddings table for semantic search vectors
CREATE TABLE IF NOT EXISTS embeddings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Document reference
    doc_id INTEGER NOT NULL,
    doc_kind TEXT NOT NULL,
    project_id INTEGER REFERENCES projects(id),
    -- Model and versioning
    model_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    -- Vector data (packed f32 little-endian)
    vector_blob BLOB NOT NULL,
    dimension INTEGER NOT NULL,
    -- Timestamps
    created_ts INTEGER NOT NULL,
    -- Composite unique: one embedding per (doc, model)
    UNIQUE(doc_id, doc_kind, model_id)
);
CREATE INDEX IF NOT EXISTS idx_embeddings_doc ON embeddings(doc_id, doc_kind);
CREATE INDEX IF NOT EXISTS idx_embeddings_model ON embeddings(model_id);
CREATE INDEX IF NOT EXISTS idx_embeddings_project ON embeddings(project_id);
CREATE INDEX IF NOT EXISTS idx_embeddings_hash ON embeddings(content_hash);
CREATE INDEX IF NOT EXISTS idx_embeddings_created ON embeddings(created_ts);
";

/// SQL for dropping the embeddings table (for testing/reset).
pub const DROP_EMBEDDINGS_TABLE_SQL: &str = r"
DROP TABLE IF EXISTS embeddings;
";

// =============================================================================
// Model
// =============================================================================

/// A stored embedding vector with metadata.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[sqlmodel(table = "embeddings")]
pub struct EmbeddingRow {
    #[sqlmodel(primary_key, auto_increment)]
    pub id: Option<i64>,

    /// ID of the source document (message ID, agent ID, etc.)
    pub doc_id: i64,

    /// Kind of document (message, agent, project, thread)
    pub doc_kind: String,

    /// Project ID (optional, for scoped queries)
    pub project_id: Option<i64>,

    /// Model ID that generated this embedding
    pub model_id: String,

    /// SHA-256 hash of the canonicalized source text
    pub content_hash: String,

    /// Packed embedding vector (f32 little-endian bytes)
    #[serde(skip_serializing, skip_deserializing)]
    #[sqlmodel(sql_type = "BLOB")]
    pub vector_blob: Vec<u8>,

    /// Vector dimension (for validation)
    pub dimension: i64,

    /// When the embedding was created (microseconds since epoch)
    pub created_ts: i64,
}

impl Default for EmbeddingRow {
    fn default() -> Self {
        Self {
            id: None,
            doc_id: 0,
            doc_kind: String::new(),
            project_id: None,
            model_id: String::new(),
            content_hash: String::new(),
            vector_blob: Vec::new(),
            dimension: 0,
            created_ts: now_micros(),
        }
    }
}

impl EmbeddingRow {
    /// Create a new embedding row.
    #[must_use]
    pub fn new(
        doc_id: i64,
        doc_kind: impl Into<String>,
        project_id: Option<i64>,
        model_id: impl Into<String>,
        content_hash: impl Into<String>,
        vector: &[f32],
    ) -> Self {
        Self {
            id: None,
            doc_id,
            doc_kind: doc_kind.into(),
            project_id,
            model_id: model_id.into(),
            content_hash: content_hash.into(),
            vector_blob: pack_vector(vector),
            dimension: vector.len() as i64,
            created_ts: now_micros(),
        }
    }

    /// Unpack the vector blob into f32 values.
    #[must_use]
    pub fn unpack_vector(&self) -> Vec<f32> {
        unpack_vector(&self.vector_blob)
    }

    /// Check if this embedding matches the given content hash.
    ///
    /// Returns `true` if the embedding is still valid (content hasn't changed).
    #[must_use]
    pub fn is_valid(&self, current_hash: &str) -> bool {
        self.content_hash == current_hash
    }

    /// Check if this embedding was generated by the given model.
    #[must_use]
    pub fn is_model(&self, model_id: &str) -> bool {
        self.model_id == model_id
    }
}

// =============================================================================
// Vector packing/unpacking
// =============================================================================

/// Pack a vector of f32 values into a little-endian byte blob.
#[must_use]
pub fn pack_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for &v in vector {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

/// Unpack a byte blob into a vector of f32 values.
#[must_use]
pub fn unpack_vector(blob: &[u8]) -> Vec<f32> {
    if blob.len() % 4 != 0 {
        return Vec::new();
    }
    blob.chunks_exact(4)
        .map(|chunk| {
            let arr: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
            f32::from_le_bytes(arr)
        })
        .collect()
}

// =============================================================================
// Invalidation status
// =============================================================================

/// Status of an embedding relative to current content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmbeddingStatus {
    /// Embedding exists and content hash matches
    Valid,
    /// Embedding exists but content hash differs (stale)
    Stale,
    /// No embedding exists for this document
    Missing,
    /// Model changed (different model_id)
    ModelChanged,
}

impl EmbeddingStatus {
    /// Returns `true` if the embedding needs to be regenerated.
    #[must_use]
    pub const fn needs_update(&self) -> bool {
        !matches!(self, Self::Valid)
    }
}

/// Check the status of an embedding given current content.
///
/// # Arguments
/// - `existing`: The existing embedding row (if any)
/// - `current_hash`: Current content hash
/// - `expected_model_id`: Expected model ID
#[must_use]
pub fn check_status(
    existing: Option<&EmbeddingRow>,
    current_hash: &str,
    expected_model_id: &str,
) -> EmbeddingStatus {
    let Some(row) = existing else {
        return EmbeddingStatus::Missing;
    };

    if row.model_id != expected_model_id {
        return EmbeddingStatus::ModelChanged;
    }

    if row.content_hash != current_hash {
        return EmbeddingStatus::Stale;
    }

    EmbeddingStatus::Valid
}

// =============================================================================
// Batch operations
// =============================================================================

/// A batch of embeddings to insert/upsert.
#[derive(Debug, Clone, Default)]
pub struct EmbeddingBatch {
    pub rows: Vec<EmbeddingRow>,
}

impl EmbeddingBatch {
    /// Create a new empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// Add an embedding to the batch.
    pub fn push(&mut self, row: EmbeddingRow) {
        self.rows.push(row);
    }

    /// Number of embeddings in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Check if the batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Statistics from a batch operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchStats {
    /// Number of embeddings inserted
    pub inserted: usize,
    /// Number of embeddings updated (replaced)
    pub updated: usize,
    /// Number of embeddings deleted
    pub deleted: usize,
    /// Number of errors encountered
    pub errors: usize,
}

impl BatchStats {
    /// Total number of changes made.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.inserted + self.updated + self.deleted
    }
}

// =============================================================================
// Query builders (SQL generation)
// =============================================================================

/// SQL for inserting or replacing an embedding.
///
/// Uses `INSERT OR REPLACE` to upsert on the unique constraint.
pub const UPSERT_EMBEDDING_SQL: &str = r"
INSERT OR REPLACE INTO embeddings
    (doc_id, doc_kind, project_id, model_id, content_hash, vector_blob, dimension, created_ts)
VALUES
    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
";

/// SQL for fetching an embedding by document reference and model.
pub const GET_EMBEDDING_SQL: &str = r"
SELECT id, doc_id, doc_kind, project_id, model_id, content_hash, vector_blob, dimension, created_ts
FROM embeddings
WHERE doc_id = ?1 AND doc_kind = ?2 AND model_id = ?3
";

/// SQL for fetching all embeddings for a document (any model).
pub const GET_DOC_EMBEDDINGS_SQL: &str = r"
SELECT id, doc_id, doc_kind, project_id, model_id, content_hash, vector_blob, dimension, created_ts
FROM embeddings
WHERE doc_id = ?1 AND doc_kind = ?2
";

/// SQL for fetching embeddings by project and model.
pub const GET_PROJECT_EMBEDDINGS_SQL: &str = r"
SELECT id, doc_id, doc_kind, project_id, model_id, content_hash, vector_blob, dimension, created_ts
FROM embeddings
WHERE project_id = ?1 AND model_id = ?2
ORDER BY doc_id
LIMIT ?3 OFFSET ?4
";

/// SQL for deleting an embedding by ID.
pub const DELETE_EMBEDDING_SQL: &str = r"
DELETE FROM embeddings WHERE id = ?1
";

/// SQL for deleting embeddings by document reference.
pub const DELETE_DOC_EMBEDDINGS_SQL: &str = r"
DELETE FROM embeddings WHERE doc_id = ?1 AND doc_kind = ?2
";

/// SQL for deleting stale embeddings (hash mismatch).
///
/// This is used during bulk invalidation when content hashes are provided.
pub const DELETE_STALE_EMBEDDINGS_SQL: &str = r"
DELETE FROM embeddings
WHERE doc_id = ?1 AND doc_kind = ?2 AND content_hash != ?3
";

/// SQL for deleting all embeddings for a model.
pub const DELETE_MODEL_EMBEDDINGS_SQL: &str = r"
DELETE FROM embeddings WHERE model_id = ?1
";

/// SQL for counting embeddings by model.
pub const COUNT_MODEL_EMBEDDINGS_SQL: &str = r"
SELECT COUNT(*) FROM embeddings WHERE model_id = ?1
";

/// SQL for counting embeddings by project.
pub const COUNT_PROJECT_EMBEDDINGS_SQL: &str = r"
SELECT COUNT(*) FROM embeddings WHERE project_id = ?1
";

/// SQL for getting embedding statistics.
pub const EMBEDDING_STATS_SQL: &str = r"
SELECT
    model_id,
    COUNT(*) as count,
    MIN(created_ts) as oldest_ts,
    MAX(created_ts) as newest_ts
FROM embeddings
GROUP BY model_id
";

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let vector = vec![1.0f32, 2.5, -3.7, 0.0, f32::MAX, f32::MIN];
        let packed = pack_vector(&vector);
        let unpacked = unpack_vector(&packed);
        assert_eq!(vector, unpacked);
    }

    #[test]
    fn pack_empty() {
        let packed = pack_vector(&[]);
        assert!(packed.is_empty());
        let unpacked = unpack_vector(&packed);
        assert!(unpacked.is_empty());
    }

    #[test]
    fn unpack_invalid_length() {
        // 5 bytes is not divisible by 4
        let bad = vec![1, 2, 3, 4, 5];
        let unpacked = unpack_vector(&bad);
        assert!(unpacked.is_empty());
    }

    #[test]
    fn embedding_row_new() {
        let row = EmbeddingRow::new(
            42,
            "message",
            Some(1),
            "all-minilm-l6-v2",
            "abc123",
            &[0.1, 0.2, 0.3],
        );
        assert_eq!(row.doc_id, 42);
        assert_eq!(row.doc_kind, "message");
        assert_eq!(row.project_id, Some(1));
        assert_eq!(row.model_id, "all-minilm-l6-v2");
        assert_eq!(row.content_hash, "abc123");
        assert_eq!(row.dimension, 3);
        assert_eq!(row.unpack_vector(), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn embedding_row_is_valid() {
        let row = EmbeddingRow::new(1, "message", None, "model-a", "hash123", &[1.0]);
        assert!(row.is_valid("hash123"));
        assert!(!row.is_valid("hash456"));
    }

    #[test]
    fn embedding_row_is_model() {
        let row = EmbeddingRow::new(1, "message", None, "model-a", "hash", &[1.0]);
        assert!(row.is_model("model-a"));
        assert!(!row.is_model("model-b"));
    }

    #[test]
    fn check_status_missing() {
        let status = check_status(None, "hash", "model");
        assert_eq!(status, EmbeddingStatus::Missing);
        assert!(status.needs_update());
    }

    #[test]
    fn check_status_valid() {
        let row = EmbeddingRow::new(1, "message", None, "model-a", "hash123", &[1.0]);
        let status = check_status(Some(&row), "hash123", "model-a");
        assert_eq!(status, EmbeddingStatus::Valid);
        assert!(!status.needs_update());
    }

    #[test]
    fn check_status_stale() {
        let row = EmbeddingRow::new(1, "message", None, "model-a", "old-hash", &[1.0]);
        let status = check_status(Some(&row), "new-hash", "model-a");
        assert_eq!(status, EmbeddingStatus::Stale);
        assert!(status.needs_update());
    }

    #[test]
    fn check_status_model_changed() {
        let row = EmbeddingRow::new(1, "message", None, "model-a", "hash", &[1.0]);
        let status = check_status(Some(&row), "hash", "model-b");
        assert_eq!(status, EmbeddingStatus::ModelChanged);
        assert!(status.needs_update());
    }

    #[test]
    fn batch_operations() {
        let mut batch = EmbeddingBatch::new();
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);

        batch.push(EmbeddingRow::new(1, "message", None, "model", "hash", &[1.0]));
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn batch_stats_total() {
        let stats = BatchStats {
            inserted: 10,
            updated: 5,
            deleted: 2,
            errors: 1,
        };
        assert_eq!(stats.total(), 17);
    }

    #[test]
    fn embedding_status_display() {
        // Ensure all variants can be serialized
        for status in [
            EmbeddingStatus::Valid,
            EmbeddingStatus::Stale,
            EmbeddingStatus::Missing,
            EmbeddingStatus::ModelChanged,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert!(!json.is_empty());
        }
    }

    #[test]
    fn sql_constants_not_empty() {
        assert!(!CREATE_EMBEDDINGS_TABLE_SQL.is_empty());
        assert!(!DROP_EMBEDDINGS_TABLE_SQL.is_empty());
        assert!(!UPSERT_EMBEDDING_SQL.is_empty());
        assert!(!GET_EMBEDDING_SQL.is_empty());
        assert!(!GET_DOC_EMBEDDINGS_SQL.is_empty());
        assert!(!GET_PROJECT_EMBEDDINGS_SQL.is_empty());
        assert!(!DELETE_EMBEDDING_SQL.is_empty());
        assert!(!DELETE_DOC_EMBEDDINGS_SQL.is_empty());
        assert!(!DELETE_STALE_EMBEDDINGS_SQL.is_empty());
        assert!(!DELETE_MODEL_EMBEDDINGS_SQL.is_empty());
        assert!(!COUNT_MODEL_EMBEDDINGS_SQL.is_empty());
        assert!(!COUNT_PROJECT_EMBEDDINGS_SQL.is_empty());
        assert!(!EMBEDDING_STATS_SQL.is_empty());
    }
}
