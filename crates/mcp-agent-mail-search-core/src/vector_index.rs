//! Vector index for semantic search.
//!
//! This module provides a vector index implementation with:
//! - In-memory and mmap-backed storage options
//! - Exact cosine similarity search
//! - Metadata filtering during retrieval
//! - Deterministic top-k with stable tie-breaking
//!
//! # Architecture
//!
//! The index stores embedding vectors alongside metadata for filtering.
//! Vectors are stored contiguously for cache-friendly access. Metadata
//! is stored separately for efficient filtering without loading vectors.
//!
//! # Retrieval Modes
//!
//! - **Exact**: Brute-force cosine similarity over all vectors
//! - **Filtered**: Apply metadata filters first, then exact search on subset
//!
//! ANN (Approximate Nearest Neighbors) is not yet implemented but the trait
//! is designed to support it when needed.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;

use crate::document::DocKind;
use crate::error::{SearchError, SearchResult};

// ────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────

/// A scored search hit from vector similarity search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHit {
    /// Document ID
    pub doc_id: i64,
    /// Document kind
    pub doc_kind: DocKind,
    /// Project ID (for scoping)
    pub project_id: Option<i64>,
    /// Cosine similarity score (0.0 to 1.0, higher is better)
    pub score: f32,
    /// Vector index position (for debugging)
    pub index_position: usize,
}

impl VectorHit {
    /// Create a new vector hit.
    #[must_use]
    pub const fn new(
        doc_id: i64,
        doc_kind: DocKind,
        project_id: Option<i64>,
        score: f32,
        index_position: usize,
    ) -> Self {
        Self {
            doc_id,
            doc_kind,
            project_id,
            score,
            index_position,
        }
    }
}

/// Ordering for `VectorHit`: by score descending, then `doc_id` ascending for stability.
impl Ord for VectorHit {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher score first (reversed)
        match other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(Ordering::Equal)
        {
            Ordering::Equal => {
                // Stable tie-breaking by doc_id ascending
                self.doc_id.cmp(&other.doc_id)
            }
            ord => ord,
        }
    }
}

impl PartialOrd for VectorHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for VectorHit {}

impl PartialEq for VectorHit {
    fn eq(&self, other: &Self) -> bool {
        self.doc_id == other.doc_id
            && self.doc_kind == other.doc_kind
            && (self.score - other.score).abs() < f32::EPSILON
    }
}

/// Metadata for filtering during vector search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMetadata {
    /// Document ID
    pub doc_id: i64,
    /// Document kind
    pub doc_kind: DocKind,
    /// Project ID (for scoping)
    pub project_id: Option<i64>,
    /// Model ID that generated the embedding
    pub model_id: String,
    /// Content hash for staleness detection
    pub content_hash: String,
    /// Additional key-value metadata
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

impl Default for VectorMetadata {
    fn default() -> Self {
        Self {
            doc_id: 0,
            doc_kind: DocKind::Message,
            project_id: None,
            model_id: String::new(),
            content_hash: String::new(),
            extra: HashMap::new(),
        }
    }
}

impl VectorMetadata {
    /// Create new metadata.
    #[must_use]
    pub fn new(doc_id: i64, doc_kind: DocKind, model_id: impl Into<String>) -> Self {
        Self {
            doc_id,
            doc_kind,
            project_id: None,
            model_id: model_id.into(),
            content_hash: String::new(),
            extra: HashMap::new(),
        }
    }

    /// Builder: set project ID.
    #[must_use]
    pub const fn with_project(mut self, project_id: i64) -> Self {
        self.project_id = Some(project_id);
        self
    }

    /// Builder: set content hash.
    #[must_use]
    pub fn with_hash(mut self, hash: impl Into<String>) -> Self {
        self.content_hash = hash.into();
        self
    }
}

/// Filter criteria for vector search.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VectorFilter {
    /// Filter by project ID
    pub project_id: Option<i64>,
    /// Filter by document kinds
    pub doc_kinds: Option<Vec<DocKind>>,
    /// Filter by model ID
    pub model_id: Option<String>,
    /// Exclude specific document IDs
    pub exclude_doc_ids: Option<Vec<i64>>,
}

impl VectorFilter {
    /// Create an empty filter (matches everything).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            project_id: None,
            doc_kinds: None,
            model_id: None,
            exclude_doc_ids: None,
        }
    }

    /// Builder: filter by project.
    #[must_use]
    pub const fn with_project(mut self, project_id: i64) -> Self {
        self.project_id = Some(project_id);
        self
    }

    /// Builder: filter by document kinds.
    #[must_use]
    pub fn with_doc_kinds(mut self, kinds: Vec<DocKind>) -> Self {
        self.doc_kinds = Some(kinds);
        self
    }

    /// Builder: filter by model.
    #[must_use]
    pub fn with_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Some(model_id.into());
        self
    }

    /// Builder: exclude specific documents.
    #[must_use]
    pub fn with_exclusions(mut self, doc_ids: Vec<i64>) -> Self {
        self.exclude_doc_ids = Some(doc_ids);
        self
    }

    /// Check if metadata matches this filter.
    #[must_use]
    pub fn matches(&self, meta: &VectorMetadata) -> bool {
        // Project filter
        if let Some(pid) = self.project_id {
            if meta.project_id != Some(pid) {
                return false;
            }
        }

        // Document kind filter
        if let Some(ref kinds) = self.doc_kinds {
            if !kinds.contains(&meta.doc_kind) {
                return false;
            }
        }

        // Model filter
        if let Some(ref mid) = self.model_id {
            if meta.model_id != *mid {
                return false;
            }
        }

        // Exclusion filter
        if let Some(ref excluded) = self.exclude_doc_ids {
            if excluded.contains(&meta.doc_id) {
                return false;
            }
        }

        true
    }

    /// Returns true if no filters are set.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.project_id.is_none()
            && self.doc_kinds.is_none()
            && self.model_id.is_none()
            && self.exclude_doc_ids.is_none()
    }
}

// ────────────────────────────────────────────────────────────────────
// Index entry
// ────────────────────────────────────────────────────────────────────

/// A single entry in the vector index.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    /// The embedding vector (L2 normalized)
    pub vector: Vec<f32>,
    /// Metadata for filtering
    pub metadata: VectorMetadata,
}

impl IndexEntry {
    /// Create a new index entry.
    ///
    /// The vector is automatically L2 normalized.
    #[must_use]
    pub fn new(vector: &[f32], metadata: VectorMetadata) -> Self {
        let normalized = crate::embedder::normalize_l2(vector);
        Self {
            vector: normalized,
            metadata,
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Vector Index
// ────────────────────────────────────────────────────────────────────

/// Configuration for the vector index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexConfig {
    /// Expected embedding dimension
    pub dimension: usize,
    /// Maximum number of vectors to store (0 = unlimited)
    pub max_vectors: usize,
    /// Whether to use memory-mapped storage (not yet implemented)
    pub use_mmap: bool,
}

impl Default for VectorIndexConfig {
    fn default() -> Self {
        Self {
            dimension: 384, // MiniLM default
            max_vectors: 0, // Unlimited
            use_mmap: false,
        }
    }
}

/// In-memory vector index with exact search.
///
/// This is the baseline implementation. For large datasets, consider
/// adding ANN (HNSW, IVF) as an optional optimization path.
#[derive(Debug)]
pub struct VectorIndex {
    config: VectorIndexConfig,
    entries: Vec<IndexEntry>,
    /// Map from (`doc_id`, `doc_kind`) to index position
    doc_index: HashMap<(i64, String), usize>,
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new(VectorIndexConfig::default())
    }
}

impl VectorIndex {
    /// Create a new vector index with the given configuration.
    #[must_use]
    pub fn new(config: VectorIndexConfig) -> Self {
        Self {
            config,
            entries: Vec::new(),
            doc_index: HashMap::new(),
        }
    }

    /// Add or update a vector in the index.
    ///
    /// # Errors
    /// Returns `SearchError::InvalidQuery` if the vector dimension doesn't match.
    pub fn upsert(&mut self, entry: IndexEntry) -> SearchResult<()> {
        if entry.vector.len() != self.config.dimension {
            return Err(SearchError::InvalidQuery(format!(
                "Vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                entry.vector.len()
            )));
        }

        // Check capacity
        if self.config.max_vectors > 0 && self.entries.len() >= self.config.max_vectors {
            return Err(SearchError::Internal(format!(
                "Vector index full (max {} vectors)",
                self.config.max_vectors
            )));
        }

        let key = (entry.metadata.doc_id, entry.metadata.doc_kind.to_string());

        if let Some(&pos) = self.doc_index.get(&key) {
            // Update existing entry
            self.entries[pos] = entry;
        } else {
            // Add new entry
            let pos = self.entries.len();
            self.doc_index.insert(key, pos);
            self.entries.push(entry);
        }

        Ok(())
    }

    /// Remove a vector from the index.
    ///
    /// Returns true if the vector was found and removed.
    pub fn remove(&mut self, doc_id: i64, doc_kind: DocKind) -> bool {
        let key = (doc_id, doc_kind.to_string());
        if let Some(pos) = self.doc_index.remove(&key) {
            // Swap-remove for O(1) removal
            self.entries.swap_remove(pos);

            // Update the index for the swapped entry (if any)
            if pos < self.entries.len() {
                let swapped = &self.entries[pos];
                let swapped_key = (
                    swapped.metadata.doc_id,
                    swapped.metadata.doc_kind.to_string(),
                );
                self.doc_index.insert(swapped_key, pos);
            }

            true
        } else {
            false
        }
    }

    /// Search for the top-k most similar vectors.
    ///
    /// # Arguments
    /// - `query`: The query vector (will be normalized)
    /// - `k`: Maximum number of results
    /// - `filter`: Optional filter criteria
    ///
    /// # Errors
    /// Returns `SearchError::InvalidQuery` if the query dimension doesn't match.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&VectorFilter>,
    ) -> SearchResult<Vec<VectorHit>> {
        if query.len() != self.config.dimension {
            return Err(SearchError::InvalidQuery(format!(
                "Query dimension mismatch: expected {}, got {}",
                self.config.dimension,
                query.len()
            )));
        }

        if self.entries.is_empty() {
            return Ok(Vec::new());
        }

        // Normalize query vector
        let query_normalized = crate::embedder::normalize_l2(query);

        // Compute similarities and collect candidates
        let mut candidates: Vec<VectorHit> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| filter.is_none_or(|f| f.matches(&entry.metadata)))
            .map(|(pos, entry)| {
                // Dot product of normalized vectors = cosine similarity
                let score = dot_product(&query_normalized, &entry.vector);
                VectorHit::new(
                    entry.metadata.doc_id,
                    entry.metadata.doc_kind,
                    entry.metadata.project_id,
                    score,
                    pos,
                )
            })
            .collect();

        // Sort by score descending, then doc_id ascending (deterministic tie-breaking)
        candidates.sort();

        // Take top k
        candidates.truncate(k);

        Ok(candidates)
    }

    /// Get a vector by document reference.
    #[must_use]
    pub fn get(&self, doc_id: i64, doc_kind: DocKind) -> Option<&IndexEntry> {
        let key = (doc_id, doc_kind.to_string());
        self.doc_index.get(&key).map(|&pos| &self.entries[pos])
    }

    /// Check if a document exists in the index.
    #[must_use]
    pub fn contains(&self, doc_id: i64, doc_kind: DocKind) -> bool {
        let key = (doc_id, doc_kind.to_string());
        self.doc_index.contains_key(&key)
    }

    /// Number of vectors in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all vectors from the index.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.doc_index.clear();
    }

    /// Get the configuration.
    #[must_use]
    pub const fn config(&self) -> &VectorIndexConfig {
        &self.config
    }

    /// Get statistics about the index.
    #[must_use]
    pub fn stats(&self) -> VectorIndexStats {
        let mut by_kind: HashMap<String, usize> = HashMap::new();
        let mut by_project: HashMap<i64, usize> = HashMap::new();

        for entry in &self.entries {
            *by_kind
                .entry(entry.metadata.doc_kind.to_string())
                .or_insert(0) += 1;
            if let Some(pid) = entry.metadata.project_id {
                *by_project.entry(pid).or_insert(0) += 1;
            }
        }

        VectorIndexStats {
            total_vectors: self.entries.len(),
            dimension: self.config.dimension,
            by_doc_kind: by_kind,
            by_project,
            memory_bytes: self.estimated_memory(),
        }
    }

    /// Estimate memory usage in bytes.
    #[must_use]
    pub fn estimated_memory(&self) -> usize {
        // Vectors: entries * dimension * 4 bytes per f32
        let vector_bytes = self.entries.len() * self.config.dimension * 4;
        // Metadata: rough estimate (model_id ~20 bytes, hash ~64 bytes, etc.)
        let metadata_bytes = self.entries.len() * 200;
        // Index overhead
        let index_bytes = self.doc_index.len() * 32;
        vector_bytes + metadata_bytes + index_bytes
    }
}

/// Statistics about a vector index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexStats {
    /// Total number of vectors
    pub total_vectors: usize,
    /// Vector dimension
    pub dimension: usize,
    /// Count by document kind
    pub by_doc_kind: HashMap<String, usize>,
    /// Count by project ID
    pub by_project: HashMap<i64, usize>,
    /// Estimated memory usage in bytes
    pub memory_bytes: usize,
}

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

/// Compute dot product of two vectors.
#[inline]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(doc_id: i64, kind: DocKind, vector: &[f32]) -> IndexEntry {
        IndexEntry::new(vector, VectorMetadata::new(doc_id, kind, "test-model"))
    }

    fn make_entry_with_project(
        doc_id: i64,
        kind: DocKind,
        project_id: i64,
        vector: &[f32],
    ) -> IndexEntry {
        IndexEntry::new(
            vector,
            VectorMetadata::new(doc_id, kind, "test-model").with_project(project_id),
        )
    }

    // ── VectorHit ordering ──

    #[test]
    fn vector_hit_ordering_by_score() {
        let a = VectorHit::new(1, DocKind::Message, None, 0.9, 0);
        let b = VectorHit::new(2, DocKind::Message, None, 0.8, 1);
        assert!(a < b); // a has higher score, should come first
    }

    #[test]
    fn vector_hit_ordering_tie_by_doc_id() {
        let a = VectorHit::new(1, DocKind::Message, None, 0.9, 0);
        let b = VectorHit::new(2, DocKind::Message, None, 0.9, 1);
        assert!(a < b); // Same score, lower doc_id first
    }

    // ── VectorFilter ──

    #[test]
    fn filter_empty_matches_all() {
        let filter = VectorFilter::new();
        assert!(filter.is_empty());

        let meta = VectorMetadata::new(1, DocKind::Message, "model");
        assert!(filter.matches(&meta));
    }

    #[test]
    fn filter_by_project() {
        let filter = VectorFilter::new().with_project(42);

        let meta_match = VectorMetadata::new(1, DocKind::Message, "m").with_project(42);
        let meta_no_match = VectorMetadata::new(2, DocKind::Message, "m").with_project(99);
        let meta_no_project = VectorMetadata::new(3, DocKind::Message, "m");

        assert!(filter.matches(&meta_match));
        assert!(!filter.matches(&meta_no_match));
        assert!(!filter.matches(&meta_no_project));
    }

    #[test]
    fn filter_by_doc_kind() {
        let filter = VectorFilter::new().with_doc_kinds(vec![DocKind::Message, DocKind::Agent]);

        let meta_msg = VectorMetadata::new(1, DocKind::Message, "m");
        let meta_agent = VectorMetadata::new(2, DocKind::Agent, "m");
        let meta_project = VectorMetadata::new(3, DocKind::Project, "m");

        assert!(filter.matches(&meta_msg));
        assert!(filter.matches(&meta_agent));
        assert!(!filter.matches(&meta_project));
    }

    #[test]
    fn filter_exclusions() {
        let filter = VectorFilter::new().with_exclusions(vec![1, 2, 3]);

        let meta_excluded = VectorMetadata::new(2, DocKind::Message, "m");
        let meta_included = VectorMetadata::new(99, DocKind::Message, "m");

        assert!(!filter.matches(&meta_excluded));
        assert!(filter.matches(&meta_included));
    }

    // ── VectorIndex basic ops ──

    #[test]
    fn index_upsert_and_get() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        let entry = make_entry(1, DocKind::Message, &[1.0, 0.0, 0.0]);
        index.upsert(entry).unwrap();

        assert_eq!(index.len(), 1);
        assert!(index.contains(1, DocKind::Message));
        assert!(!index.contains(2, DocKind::Message));

        let retrieved = index.get(1, DocKind::Message).unwrap();
        assert_eq!(retrieved.metadata.doc_id, 1);
    }

    #[test]
    fn index_upsert_updates_existing() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        let entry1 = make_entry(1, DocKind::Message, &[1.0, 0.0, 0.0]);
        let entry2 = make_entry(1, DocKind::Message, &[0.0, 1.0, 0.0]);

        index.upsert(entry1).unwrap();
        index.upsert(entry2).unwrap();

        assert_eq!(index.len(), 1); // Still only one entry
        let retrieved = index.get(1, DocKind::Message).unwrap();
        // Vector should be updated (normalized [0, 1, 0])
        assert!(retrieved.vector[1].abs() > 0.9);
    }

    #[test]
    fn index_remove() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        index
            .upsert(make_entry(1, DocKind::Message, &[1.0, 0.0, 0.0]))
            .unwrap();
        index
            .upsert(make_entry(2, DocKind::Message, &[0.0, 1.0, 0.0]))
            .unwrap();

        assert_eq!(index.len(), 2);
        assert!(index.remove(1, DocKind::Message));
        assert_eq!(index.len(), 1);
        assert!(!index.contains(1, DocKind::Message));
        assert!(index.contains(2, DocKind::Message));

        // Remove non-existent
        assert!(!index.remove(999, DocKind::Message));
    }

    #[test]
    fn index_dimension_mismatch() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        let entry = make_entry(1, DocKind::Message, &[1.0, 0.0]); // Wrong dimension
        let result = index.upsert(entry);
        assert!(result.is_err());
    }

    // ── Search ──

    #[test]
    fn search_exact_match() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        index
            .upsert(make_entry(1, DocKind::Message, &[1.0, 0.0, 0.0]))
            .unwrap();
        index
            .upsert(make_entry(2, DocKind::Message, &[0.0, 1.0, 0.0]))
            .unwrap();
        index
            .upsert(make_entry(3, DocKind::Message, &[0.0, 0.0, 1.0]))
            .unwrap();

        // Query matches doc 1 exactly
        let results = index.search(&[1.0, 0.0, 0.0], 10, None).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].doc_id, 1);
        assert!((results[0].score - 1.0).abs() < 0.01); // Cosine similarity ~1.0
    }

    #[test]
    fn search_with_filter() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        index
            .upsert(make_entry_with_project(
                1,
                DocKind::Message,
                42,
                &[1.0, 0.0, 0.0],
            ))
            .unwrap();
        index
            .upsert(make_entry_with_project(
                2,
                DocKind::Message,
                99,
                &[1.0, 0.0, 0.0],
            ))
            .unwrap();

        let filter = VectorFilter::new().with_project(42);
        let results = index.search(&[1.0, 0.0, 0.0], 10, Some(&filter)).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
    }

    #[test]
    fn search_top_k_limit() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        for i in 0..10 {
            index
                .upsert(make_entry(i, DocKind::Message, &[1.0, 0.0, 0.0]))
                .unwrap();
        }

        let results = index.search(&[1.0, 0.0, 0.0], 3, None).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_empty_index() {
        let index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        let results = index.search(&[1.0, 0.0, 0.0], 10, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_deterministic_tie_breaking() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        // All have same similarity to query
        index
            .upsert(make_entry(5, DocKind::Message, &[1.0, 0.0, 0.0]))
            .unwrap();
        index
            .upsert(make_entry(3, DocKind::Message, &[1.0, 0.0, 0.0]))
            .unwrap();
        index
            .upsert(make_entry(7, DocKind::Message, &[1.0, 0.0, 0.0]))
            .unwrap();

        let results = index.search(&[1.0, 0.0, 0.0], 10, None).unwrap();

        // Should be sorted by doc_id ascending for ties
        assert_eq!(results[0].doc_id, 3);
        assert_eq!(results[1].doc_id, 5);
        assert_eq!(results[2].doc_id, 7);
    }

    // ── Stats ──

    #[test]
    fn index_stats() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        index
            .upsert(make_entry_with_project(
                1,
                DocKind::Message,
                42,
                &[1.0, 0.0, 0.0],
            ))
            .unwrap();
        index
            .upsert(make_entry_with_project(
                2,
                DocKind::Agent,
                42,
                &[0.0, 1.0, 0.0],
            ))
            .unwrap();
        index
            .upsert(make_entry_with_project(
                3,
                DocKind::Message,
                99,
                &[0.0, 0.0, 1.0],
            ))
            .unwrap();

        let stats = index.stats();
        assert_eq!(stats.total_vectors, 3);
        assert_eq!(stats.dimension, 3);
        assert_eq!(stats.by_doc_kind.get("message"), Some(&2));
        assert_eq!(stats.by_doc_kind.get("agent"), Some(&1));
        assert_eq!(stats.by_project.get(&42), Some(&2));
        assert_eq!(stats.by_project.get(&99), Some(&1));
    }

    #[test]
    fn index_clear() {
        let mut index = VectorIndex::new(VectorIndexConfig {
            dimension: 3,
            ..Default::default()
        });

        index
            .upsert(make_entry(1, DocKind::Message, &[1.0, 0.0, 0.0]))
            .unwrap();
        index
            .upsert(make_entry(2, DocKind::Message, &[0.0, 1.0, 0.0]))
            .unwrap();

        assert_eq!(index.len(), 2);
        index.clear();
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
    }
}
