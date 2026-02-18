//! Two-tier progressive search for semantic similarity.
//!
//! This module implements a progressive search strategy that:
//! 1. Returns instant results using a fast embedding model (potion-128M, ~0ms)
//! 2. Refines rankings in the background using a quality model (`MiniLM`, ~128ms)
//!
//! # Architecture
//!
//! ```text
//! User Query
//!     │
//!     ├──→ [Fast Embedder] ──→ Results in ~1ms (display immediately)
//!     │       (potion-128M)
//!     │
//!     └──→ [Quality Model] ──→ Refined scores in ~130ms
//!              (MiniLM-L6)           │
//!                                    ▼
//!                            Smooth re-rank
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use mcp_agent_mail_search_core::two_tier::{TwoTierConfig, TwoTierIndex, SearchPhase};
//!
//! let config = TwoTierConfig::default();
//! let index = TwoTierIndex::new(&config);
//!
//! for phase in searcher.search("authentication middleware", 10) {
//!     match phase {
//!         SearchPhase::Initial { results, latency_ms } => {
//!             // Display instant results
//!         }
//!         SearchPhase::Refined { results, latency_ms } => {
//!             // Update with refined results
//!         }
//!         SearchPhase::RefinementFailed { error } => {
//!             // Keep showing initial results
//!         }
//!     }
//! }
//! ```

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::Instant;

use half::f16;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::{SearchError, SearchResult};
use crate::metrics::{TwoTierIndexMetrics, TwoTierMetrics, TwoTierSearchMetrics};

// ────────────────────────────────────────────────────────────────────
// Configuration
// ────────────────────────────────────────────────────────────────────

/// Configuration for two-tier search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoTierConfig {
    /// Dimension for fast embeddings (potion-128M = 256).
    pub fast_dimension: usize,
    /// Dimension for quality embeddings (`MiniLM` = 384).
    pub quality_dimension: usize,
    /// Weight for quality scores when blending (default: 0.7).
    /// 0.0 = fast-only, 1.0 = quality-only.
    pub quality_weight: f32,
    /// Maximum documents to refine via quality model (default: 100).
    pub max_refinement_docs: usize,
    /// Whether to skip quality refinement entirely.
    pub fast_only: bool,
    /// Whether to wait for quality results before returning.
    pub quality_only: bool,
}

impl Default for TwoTierConfig {
    fn default() -> Self {
        Self {
            fast_dimension: 256,    // potion-128M dimension
            quality_dimension: 384, // MiniLM-L6-v2 dimension
            quality_weight: 0.7,
            max_refinement_docs: 100,
            fast_only: false,
            quality_only: false,
        }
    }
}

impl TwoTierConfig {
    /// Create config for fast-only mode.
    #[must_use]
    pub fn fast_only() -> Self {
        Self {
            fast_only: true,
            ..Self::default()
        }
    }

    /// Create config for quality-only mode.
    #[must_use]
    pub fn quality_only() -> Self {
        Self {
            quality_only: true,
            ..Self::default()
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Index entry and result types
// ────────────────────────────────────────────────────────────────────

/// Two-tier index entry with both fast and quality embeddings.
#[derive(Debug, Clone)]
pub struct TwoTierEntry {
    /// Document ID (`message_id` from DB).
    pub doc_id: u64,
    /// Document kind.
    pub doc_kind: crate::document::DocKind,
    /// Project ID (for filtering).
    pub project_id: Option<i64>,
    /// Fast embedding (f16 quantized, potion-128M).
    pub fast_embedding: Vec<f16>,
    /// Quality embedding (f16 quantized, `MiniLM`). Optional for incremental adds.
    pub quality_embedding: Vec<f16>,
    /// Whether a real quality embedding was computed (not zero-filled fallback).
    /// Documents without quality embeddings participate in fast search but are
    /// excluded from quality refinement scoring.
    pub has_quality: bool,
}

/// Search result with score and metadata.
#[derive(Debug, Clone)]
pub struct ScoredResult {
    /// Index in the two-tier index.
    pub idx: usize,
    /// Document ID (`message_id`).
    pub doc_id: u64,
    /// Document kind.
    pub doc_kind: crate::document::DocKind,
    /// Project ID.
    pub project_id: Option<i64>,
    /// Similarity score.
    pub score: f32,
}

/// Search phase result for progressive display.
#[derive(Debug, Clone)]
pub enum SearchPhase {
    /// Initial fast results.
    Initial {
        results: Vec<ScoredResult>,
        latency_ms: u64,
    },
    /// Refined quality results.
    Refined {
        results: Vec<ScoredResult>,
        latency_ms: u64,
    },
    /// Refinement failed, keep using initial results.
    RefinementFailed { error: String },
}

// ────────────────────────────────────────────────────────────────────
// Two-tier index
// ────────────────────────────────────────────────────────────────────

/// Index build status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IndexStatus {
    /// Index is being built.
    Building { progress: f32 },
    /// Index is complete.
    Complete {
        fast_latency_ms: u64,
        quality_latency_ms: u64,
    },
    /// Index build failed.
    Failed { error: String },
}

/// Metadata for a two-tier index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoTierMetadata {
    /// Fast embedder ID (e.g., "potion-128m").
    pub fast_embedder_id: String,
    /// Quality embedder ID (e.g., "minilm-384").
    pub quality_embedder_id: String,
    /// Document count.
    pub doc_count: usize,
    /// Index build timestamp (Unix seconds).
    pub built_at: i64,
    /// Index status.
    pub status: IndexStatus,
}

/// Two-tier index for progressive search.
///
/// Stores both fast (potion) and quality (`MiniLM`) embeddings in f16 format
/// for memory efficiency. Uses SIMD-accelerated dot product for search.
#[derive(Debug)]
pub struct TwoTierIndex {
    /// Index metadata.
    pub metadata: TwoTierMetadata,
    /// Fast embeddings (row-major, f16).
    fast_embeddings: Vec<f16>,
    /// Quality embeddings (row-major, f16).
    quality_embeddings: Vec<f16>,
    /// Document IDs in index order.
    doc_ids: Vec<u64>,
    /// Document kinds in index order.
    doc_kinds: Vec<crate::document::DocKind>,
    /// Project IDs in index order.
    project_ids: Vec<Option<i64>>,
    /// Whether each document has a real quality embedding (not zero-filled).
    has_quality_flags: Vec<bool>,
    /// Cached count of documents with real quality embeddings.
    quality_doc_count: usize,
    /// Configuration.
    config: TwoTierConfig,
}

/// Check if an f16 embedding is effectively a zero vector.
///
/// Returns true if all components are zero (or very close to zero),
/// indicating the embedding was filled with zeros as a fallback.
#[inline]
fn is_zero_vector_f16(embedding: &[f16]) -> bool {
    embedding.iter().all(|&v| f32::from(v).abs() < f32::EPSILON)
}

impl TwoTierIndex {
    /// Create a new empty index with the given configuration.
    #[must_use]
    pub fn new(config: &TwoTierConfig) -> Self {
        Self {
            metadata: TwoTierMetadata {
                fast_embedder_id: "potion-128m".to_owned(),
                quality_embedder_id: "minilm-384".to_owned(),
                doc_count: 0,
                built_at: chrono::Utc::now().timestamp(),
                status: IndexStatus::Complete {
                    fast_latency_ms: 0,
                    quality_latency_ms: 0,
                },
            },
            fast_embeddings: Vec::new(),
            quality_embeddings: Vec::new(),
            doc_ids: Vec::new(),
            doc_kinds: Vec::new(),
            project_ids: Vec::new(),
            has_quality_flags: Vec::new(),
            quality_doc_count: 0,
            config: config.clone(),
        }
    }

    /// Build a two-tier index from entries.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding dimensions don't match the config.
    pub fn build(
        fast_embedder_id: impl Into<String>,
        quality_embedder_id: impl Into<String>,
        config: &TwoTierConfig,
        entries: impl IntoIterator<Item = TwoTierEntry>,
    ) -> SearchResult<Self> {
        let entries: Vec<TwoTierEntry> = entries.into_iter().collect();
        let doc_count = entries.len();

        if doc_count == 0 {
            return Ok(Self {
                metadata: TwoTierMetadata {
                    fast_embedder_id: fast_embedder_id.into(),
                    quality_embedder_id: quality_embedder_id.into(),
                    doc_count: 0,
                    built_at: chrono::Utc::now().timestamp(),
                    status: IndexStatus::Complete {
                        fast_latency_ms: 0,
                        quality_latency_ms: 0,
                    },
                },
                fast_embeddings: Vec::new(),
                quality_embeddings: Vec::new(),
                doc_ids: Vec::new(),
                doc_kinds: Vec::new(),
                project_ids: Vec::new(),
                has_quality_flags: Vec::new(),
                quality_doc_count: 0,
                config: config.clone(),
            });
        }

        // Validate dimensions
        for (i, entry) in entries.iter().enumerate() {
            if entry.fast_embedding.len() != config.fast_dimension {
                return Err(SearchError::InvalidQuery(format!(
                    "fast embedding dimension mismatch at index {}: expected {}, got {}",
                    i,
                    config.fast_dimension,
                    entry.fast_embedding.len()
                )));
            }
            if entry.quality_embedding.len() != config.quality_dimension {
                return Err(SearchError::InvalidQuery(format!(
                    "quality embedding dimension mismatch at index {}: expected {}, got {}",
                    i,
                    config.quality_dimension,
                    entry.quality_embedding.len()
                )));
            }
        }

        // Build flat vectors
        let mut fast_embeddings = Vec::with_capacity(doc_count * config.fast_dimension);
        let mut quality_embeddings = Vec::with_capacity(doc_count * config.quality_dimension);
        let mut doc_ids = Vec::with_capacity(doc_count);
        let mut doc_kinds = Vec::with_capacity(doc_count);
        let mut project_ids = Vec::with_capacity(doc_count);
        let mut has_quality_flags = Vec::with_capacity(doc_count);

        for TwoTierEntry {
            doc_id,
            doc_kind,
            project_id,
            mut fast_embedding,
            mut quality_embedding,
            has_quality,
        } in entries
        {
            // Determine has_quality: use explicit flag if set, otherwise detect zero vectors
            let has_quality_flag = has_quality && !is_zero_vector_f16(&quality_embedding);
            fast_embeddings.append(&mut fast_embedding);
            quality_embeddings.append(&mut quality_embedding);
            doc_ids.push(doc_id);
            doc_kinds.push(doc_kind);
            project_ids.push(project_id);
            has_quality_flags.push(has_quality_flag);
        }
        let quality_doc_count = has_quality_flags.iter().filter(|&&v| v).count();

        Ok(Self {
            metadata: TwoTierMetadata {
                fast_embedder_id: fast_embedder_id.into(),
                quality_embedder_id: quality_embedder_id.into(),
                doc_count,
                built_at: chrono::Utc::now().timestamp(),
                status: IndexStatus::Complete {
                    fast_latency_ms: 0,
                    quality_latency_ms: 0,
                },
            },
            fast_embeddings,
            quality_embeddings,
            doc_ids,
            doc_kinds,
            project_ids,
            has_quality_flags,
            quality_doc_count,
            config: config.clone(),
        })
    }

    /// Get the number of documents in the index.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.metadata.doc_count
    }

    /// Check if the index is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.metadata.doc_count == 0
    }

    /// Get document ID at index.
    #[must_use]
    pub fn doc_id(&self, idx: usize) -> Option<u64> {
        self.doc_ids.get(idx).copied()
    }

    /// Check if document at index has a real quality embedding.
    #[must_use]
    pub fn has_quality(&self, idx: usize) -> bool {
        self.has_quality_flags.get(idx).copied().unwrap_or(false)
    }

    /// Get the count of documents with quality embeddings.
    #[must_use]
    pub const fn quality_count(&self) -> usize {
        self.quality_doc_count
    }

    /// Get quality embedding coverage as a ratio (0.0 to 1.0).
    #[must_use]
    pub fn quality_coverage(&self) -> f32 {
        if self.metadata.doc_count == 0 {
            return 1.0; // Empty index has "full" coverage
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.quality_count() as f32 / self.metadata.doc_count as f32
        }
    }

    /// Build index observability metrics.
    #[must_use]
    pub fn metrics(&self) -> TwoTierIndexMetrics {
        let fast_memory_bytes = self.fast_embeddings.len() * std::mem::size_of::<f16>();
        let quality_memory_bytes = self.quality_embeddings.len() * std::mem::size_of::<f16>();
        TwoTierIndexMetrics::from_counts(
            self.metadata.doc_count,
            self.quality_count(),
            fast_memory_bytes,
            quality_memory_bytes,
        )
    }

    /// Detect documents with zero-vector quality embeddings.
    ///
    /// Returns document IDs that have `has_quality=true` but actually contain
    /// zero vectors (indicating data corruption or migration issues).
    #[must_use]
    pub fn detect_zero_quality_docs(&self) -> Vec<u64> {
        self.doc_ids
            .iter()
            .enumerate()
            .filter(|(idx, _)| {
                // Only check docs marked as having quality
                if !self.has_quality(*idx) {
                    return false;
                }
                // Check if their embedding is actually zero
                self.quality_embedding(*idx).is_some_and(is_zero_vector_f16)
            })
            .map(|(_, &id)| id)
            .collect()
    }

    /// Migrate zero-vector quality documents to `has_quality=false`.
    ///
    /// Returns the count of documents migrated.
    pub fn migrate_zero_quality_to_no_quality(&mut self) -> usize {
        let mut count = 0;
        for idx in 0..self.metadata.doc_count {
            if self.has_quality_flags.get(idx).copied().unwrap_or(false)
                && let Some(emb) = self.quality_embedding(idx)
                && is_zero_vector_f16(emb)
            {
                self.has_quality_flags[idx] = false;
                self.quality_doc_count = self.quality_doc_count.saturating_sub(1);
                count += 1;
            }
        }
        if count > 0 {
            debug!(
                migrated = count,
                "Migrated zero-quality docs to has_quality=false"
            );
        }
        count
    }

    /// Return a full copy of index entries suitable for lock-free async probes.
    ///
    /// This is intentionally allocation-heavy and should only be used by
    /// migration/probe paths, not steady-state search.
    ///
    /// # Errors
    ///
    /// Returns an error if internal embedding buffers are inconsistent with
    /// `metadata.doc_count`.
    pub fn entries_snapshot(&self) -> SearchResult<Vec<TwoTierEntry>> {
        let mut entries = Vec::with_capacity(self.metadata.doc_count);

        for idx in 0..self.metadata.doc_count {
            let Some(fast_embedding) = self.fast_embedding(idx) else {
                return Err(SearchError::Internal(format!(
                    "missing fast embedding for two-tier index position {idx}"
                )));
            };
            let Some(quality_embedding) = self.quality_embedding(idx) else {
                return Err(SearchError::Internal(format!(
                    "missing quality embedding for two-tier index position {idx}"
                )));
            };
            let Some(doc_id) = self.doc_ids.get(idx).copied() else {
                return Err(SearchError::Internal(format!(
                    "missing doc_id for two-tier index position {idx}"
                )));
            };

            entries.push(TwoTierEntry {
                doc_id,
                doc_kind: self
                    .doc_kinds
                    .get(idx)
                    .copied()
                    .unwrap_or(crate::document::DocKind::Message),
                project_id: self.project_ids.get(idx).copied().flatten(),
                fast_embedding: fast_embedding.to_vec(),
                quality_embedding: quality_embedding.to_vec(),
                has_quality: self.has_quality(idx),
            });
        }

        Ok(entries)
    }

    /// Get fast embedding at index.
    fn fast_embedding(&self, idx: usize) -> Option<&[f16]> {
        let dim = self.config.fast_dimension;
        let start = idx * dim;
        let end = start + dim;
        if end <= self.fast_embeddings.len() {
            Some(&self.fast_embeddings[start..end])
        } else {
            None
        }
    }

    /// Get quality embedding at index.
    fn quality_embedding(&self, idx: usize) -> Option<&[f16]> {
        let dim = self.config.quality_dimension;
        let start = idx * dim;
        let end = start + dim;
        if end <= self.quality_embeddings.len() {
            Some(&self.quality_embeddings[start..end])
        } else {
            None
        }
    }

    /// Search using fast embeddings only.
    #[must_use]
    pub fn search_fast(&self, query_vec: &[f32], k: usize) -> Vec<ScoredResult> {
        let _span =
            tracing::debug_span!("two_tier.search_fast", query_len = query_vec.len(), k).entered();
        if self.is_empty() || k == 0 {
            return Vec::new();
        }

        let dim = self.config.fast_dimension;
        if query_vec.len() != dim {
            warn!(
                query_dim = query_vec.len(),
                expected_dim = dim,
                "query dimension mismatch for fast search"
            );
            return Vec::new();
        }

        let limit = k.min(self.metadata.doc_count);
        if limit == 1 {
            let mut best: Option<ScoredEntry> = None;
            for (idx, embedding) in self
                .fast_embeddings
                .chunks_exact(dim)
                .take(self.metadata.doc_count)
                .enumerate()
            {
                let entry = ScoredEntry {
                    score: dot_product_f16_simd(embedding, query_vec),
                    idx,
                };
                if best.is_none_or(|current| entry > current) {
                    best = Some(entry);
                }
            }
            return best.map_or_else(Vec::new, |entry| vec![self.scored_entry_to_result(entry)]);
        }
        let mut heap = BinaryHeap::with_capacity(limit.saturating_add(1));

        for (idx, embedding) in self
            .fast_embeddings
            .chunks_exact(dim)
            .take(self.metadata.doc_count)
            .enumerate()
        {
            let score = dot_product_f16_simd(embedding, query_vec);
            heap.push(std::cmp::Reverse(ScoredEntry { score, idx }));
            if heap.len() > limit {
                heap.pop();
            }
        }

        heap.into_sorted_vec()
            .into_iter()
            .map(|std::cmp::Reverse(entry)| self.scored_entry_to_result(entry))
            .collect()
    }

    /// Search using quality embeddings only.
    #[must_use]
    pub fn search_quality(&self, query_vec: &[f32], k: usize) -> Vec<ScoredResult> {
        let _span = tracing::debug_span!("two_tier.search_quality", query_len = query_vec.len(), k)
            .entered();
        if self.is_empty() || k == 0 {
            return Vec::new();
        }

        let dim = self.config.quality_dimension;
        if query_vec.len() != dim {
            warn!(
                query_dim = query_vec.len(),
                expected_dim = dim,
                "query dimension mismatch for quality search"
            );
            return Vec::new();
        }

        let limit = k.min(self.quality_count());
        if limit == 0 {
            return Vec::new();
        }
        if limit == 1 {
            let mut best: Option<ScoredEntry> = None;
            for (idx, (embedding, has_quality)) in self
                .quality_embeddings
                .chunks_exact(dim)
                .zip(self.has_quality_flags.iter().copied())
                .enumerate()
            {
                if !has_quality {
                    continue;
                }
                let entry = ScoredEntry {
                    score: dot_product_f16_simd(embedding, query_vec),
                    idx,
                };
                if best.is_none_or(|current| entry > current) {
                    best = Some(entry);
                }
            }
            return best.map_or_else(Vec::new, |entry| vec![self.scored_entry_to_result(entry)]);
        }
        let mut heap = BinaryHeap::with_capacity(limit.saturating_add(1));

        for (idx, (embedding, has_quality)) in self
            .quality_embeddings
            .chunks_exact(dim)
            .zip(self.has_quality_flags.iter().copied())
            .enumerate()
        {
            if !has_quality {
                continue;
            }
            let score = dot_product_f16_simd(embedding, query_vec);
            heap.push(std::cmp::Reverse(ScoredEntry { score, idx }));
            if heap.len() > limit {
                heap.pop();
            }
        }

        heap.into_sorted_vec()
            .into_iter()
            .map(|std::cmp::Reverse(entry)| self.scored_entry_to_result(entry))
            .collect()
    }

    /// Get quality scores for a set of document indices.
    #[must_use]
    pub fn quality_scores_for_indices(&self, query_vec: &[f32], indices: &[usize]) -> Vec<f32> {
        let dim = self.config.quality_dimension;

        indices
            .iter()
            .map(|&idx| {
                if idx >= self.metadata.doc_count
                    || !self.has_quality_flags.get(idx).copied().unwrap_or(false)
                {
                    return 0.0;
                }

                let Some(start) = idx.checked_mul(dim) else {
                    return 0.0;
                };
                let Some(end) = start.checked_add(dim) else {
                    return 0.0;
                };

                self.quality_embeddings
                    .get(start..end)
                    .map_or(0.0, |emb| dot_product_f16_simd(emb, query_vec))
            })
            .collect()
    }

    /// Add a single entry to the index.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding dimensions don't match.
    pub fn add_entry(&mut self, entry: TwoTierEntry) -> SearchResult<()> {
        let TwoTierEntry {
            doc_id,
            doc_kind,
            project_id,
            mut fast_embedding,
            mut quality_embedding,
            has_quality,
        } = entry;

        if fast_embedding.len() != self.config.fast_dimension {
            return Err(SearchError::InvalidQuery(format!(
                "fast embedding dimension mismatch: expected {}, got {}",
                self.config.fast_dimension,
                fast_embedding.len()
            )));
        }
        if quality_embedding.len() != self.config.quality_dimension {
            return Err(SearchError::InvalidQuery(format!(
                "quality embedding dimension mismatch: expected {}, got {}",
                self.config.quality_dimension,
                quality_embedding.len()
            )));
        }

        // Determine has_quality: use explicit flag if set, otherwise detect zero vectors
        let has_quality_flag = has_quality && !is_zero_vector_f16(&quality_embedding);

        self.fast_embeddings.append(&mut fast_embedding);
        self.quality_embeddings.append(&mut quality_embedding);
        self.doc_ids.push(doc_id);
        self.doc_kinds.push(doc_kind);
        self.project_ids.push(project_id);
        self.has_quality_flags.push(has_quality_flag);
        if has_quality_flag {
            self.quality_doc_count += 1;
        }
        self.metadata.doc_count += 1;

        Ok(())
    }

    #[inline]
    fn scored_entry_to_result(&self, entry: ScoredEntry) -> ScoredResult {
        ScoredResult {
            idx: entry.idx,
            doc_id: self.doc_ids[entry.idx],
            doc_kind: self
                .doc_kinds
                .get(entry.idx)
                .copied()
                .unwrap_or(crate::document::DocKind::Message),
            project_id: self.project_ids.get(entry.idx).copied().flatten(),
            score: entry.score,
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Scored entry for heap-based top-k search
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct ScoredEntry {
    score: f32,
    idx: usize,
}

impl PartialEq for ScoredEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score.total_cmp(&other.score) == Ordering::Equal && self.idx == other.idx
    }
}

impl Eq for ScoredEntry {}

impl PartialOrd for ScoredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.idx.cmp(&other.idx))
    }
}

// ────────────────────────────────────────────────────────────────────
// SIMD-accelerated f16 dot product
// ────────────────────────────────────────────────────────────────────

/// SIMD-accelerated dot product between f16 embedding and f32 query.
///
/// Delegates to `frankensearch::index::simd::dot_product_f16_f32()` for the
/// actual computation. Returns 0.0 on dimension mismatch (matching legacy
/// behavior).
///
/// Note: This module is compiled only when the `semantic` feature is enabled,
/// which always brings in the frankensearch dependency.
#[inline]
#[must_use]
pub fn dot_product_f16_simd(embedding: &[f16], query: &[f32]) -> f32 {
    frankensearch::index::simd::dot_product_f16_f32(embedding, query).unwrap_or(0.0)
}

/// Normalize scores to \[0, 1\] range using min-max scaling.
///
/// Delegates to `frankensearch::fusion::normalize::normalize_scores()`.
#[must_use]
pub fn normalize_scores(scores: &[f32]) -> Vec<f32> {
    frankensearch::fusion::normalize::normalize_scores(scores)
}

/// Blend fast and quality scores with the given weight.
///
/// `quality_weight` controls the blend: 0.0 = fast-only, 1.0 = quality-only.
#[must_use]
pub fn blend_scores(fast: &[f32], quality: &[f32], quality_weight: f32) -> Vec<f32> {
    let fast_norm = normalize_scores(fast);
    let quality_norm = normalize_scores(quality);

    fast_norm
        .iter()
        .zip(quality_norm.iter())
        .map(|(&f, &q)| (1.0 - quality_weight).mul_add(f, quality_weight * q))
        .collect()
}

// ────────────────────────────────────────────────────────────────────
// Two-tier searcher
// ────────────────────────────────────────────────────────────────────

/// Embedder trait for two-tier search.
///
/// This is a simplified version that works with the two-tier system.
pub trait TwoTierEmbedder: Send + Sync {
    /// Embed a query string into a vector.
    fn embed(&self, text: &str) -> SearchResult<Vec<f32>>;

    /// Get the output dimension.
    fn dimension(&self) -> usize;

    /// Get the embedder ID.
    fn id(&self) -> &str;
}

/// Two-tier searcher that coordinates fast and quality search.
pub struct TwoTierSearcher<'a> {
    index: &'a TwoTierIndex,
    fast_embedder: Arc<dyn TwoTierEmbedder>,
    quality_embedder: Option<Arc<dyn TwoTierEmbedder>>,
    config: TwoTierConfig,
    metrics_recorder: Option<Arc<std::sync::Mutex<TwoTierMetrics>>>,
}

impl<'a> TwoTierSearcher<'a> {
    /// Create a new two-tier searcher.
    pub fn new(
        index: &'a TwoTierIndex,
        fast_embedder: Arc<dyn TwoTierEmbedder>,
        quality_embedder: Option<Arc<dyn TwoTierEmbedder>>,
        config: TwoTierConfig,
    ) -> Self {
        Self {
            index,
            fast_embedder,
            quality_embedder,
            config,
            metrics_recorder: None,
        }
    }

    /// Attach a shared metrics recorder.
    #[must_use]
    pub fn with_metrics_recorder(
        mut self,
        metrics_recorder: Arc<std::sync::Mutex<TwoTierMetrics>>,
    ) -> Self {
        self.metrics_recorder = Some(metrics_recorder);
        self
    }

    /// Perform two-tier progressive search.
    ///
    /// Returns an iterator that yields search phases:
    /// 1. Initial results from fast embeddings
    /// 2. Refined results from quality embeddings (if available)
    pub fn search(&self, query: &str, k: usize) -> impl Iterator<Item = SearchPhase> + '_ {
        TwoTierSearchIter::new(
            self,
            query.to_string(),
            k,
            self.metrics_recorder.as_ref().map(Arc::clone),
        )
    }

    /// Perform fast-only search.
    pub fn search_fast_only(&self, query: &str, k: usize) -> SearchResult<Vec<ScoredResult>> {
        let start = Instant::now();
        let query_vec = self.fast_embedder.embed(query)?;
        let results = self.index.search_fast(&query_vec, k);
        debug!(
            query_len = query.len(),
            k = k,
            result_count = results.len(),
            latency_ms = start.elapsed().as_millis(),
            "Fast-only search completed"
        );
        Ok(results)
    }

    /// Perform quality-only search.
    pub fn search_quality_only(&self, query: &str, k: usize) -> SearchResult<Vec<ScoredResult>> {
        let start = Instant::now();

        let quality_embedder = self
            .quality_embedder
            .as_ref()
            .ok_or_else(|| SearchError::ModeUnavailable("quality embedder not available".into()))?;

        let query_vec = quality_embedder.embed(query)?;
        let results = self.index.search_quality(&query_vec, k);
        debug!(
            query_len = query.len(),
            k = k,
            result_count = results.len(),
            latency_ms = start.elapsed().as_millis(),
            "Quality-only search completed"
        );
        Ok(results)
    }
}

/// Iterator for two-tier search phases.
struct TwoTierSearchIter<'a> {
    searcher: &'a TwoTierSearcher<'a>,
    query: String,
    k: usize,
    phase: u8,
    fast_results: Option<Vec<ScoredResult>>,
    fast_order: Vec<u64>,
    metrics_recorder: Option<Arc<std::sync::Mutex<TwoTierMetrics>>>,
    search_metrics: TwoTierSearchMetrics,
    search_span: tracing::Span,
}

impl<'a> TwoTierSearchIter<'a> {
    #[allow(clippy::missing_const_for_fn)]
    fn new(
        searcher: &'a TwoTierSearcher<'a>,
        query: String,
        k: usize,
        metrics_recorder: Option<Arc<std::sync::Mutex<TwoTierMetrics>>>,
    ) -> Self {
        let query_len = query.len();
        Self {
            searcher,
            search_metrics: TwoTierSearchMetrics::for_query(&query),
            search_span: tracing::info_span!("two_tier.search", query_len, limit = k),
            query,
            k,
            phase: 0,
            fast_results: None,
            fast_order: Vec::new(),
            metrics_recorder,
        }
    }

    fn build_refined_results(&mut self, query_vec: &[f32]) -> Vec<ScoredResult> {
        let Some(fast_results) = self.fast_results.as_ref() else {
            // If no fast candidates are available, fall back to full quality search.
            let _score_span =
                tracing::debug_span!("two_tier.score_quality", candidates = 0).entered();
            let score_start = Instant::now();
            let results = self.searcher.index.search_quality(query_vec, self.k);
            self.search_metrics.quality_score_us =
                u64::try_from(score_start.elapsed().as_micros()).unwrap_or(u64::MAX);
            self.search_metrics.refined_count = results.len();
            self.search_metrics.was_refined = true;
            return results;
        };

        if fast_results.is_empty() {
            self.search_metrics.refined_count = 0;
            self.search_metrics.was_refined = true;
            return Vec::new();
        }

        let refinement_limit = self
            .searcher
            .config
            .max_refinement_docs
            .min(fast_results.len());

        // Explicitly allow turning off refinement while still returning fast results.
        if refinement_limit == 0 {
            let mut passthrough = fast_results.clone();
            passthrough.truncate(self.k);
            self.search_metrics.refined_count = 0;
            self.search_metrics.was_refined = false;
            return passthrough;
        }

        let candidates: Vec<usize> = fast_results
            .iter()
            .take(refinement_limit)
            .map(|sr| sr.idx)
            .collect();

        let _score_span =
            tracing::debug_span!("two_tier.score_quality", candidates = candidates.len()).entered();
        let score_start = Instant::now();
        let quality_scores = self
            .searcher
            .index
            .quality_scores_for_indices(query_vec, &candidates);
        self.search_metrics.quality_score_us =
            u64::try_from(score_start.elapsed().as_micros()).unwrap_or(u64::MAX);

        // Blend scores, but only for docs with quality embeddings.
        // Docs without quality use fast score only.
        let _blend_span = tracing::debug_span!("two_tier.blend", refinement_limit).entered();
        let blend_start = Instant::now();
        let weight = self.searcher.config.quality_weight;
        let mut blended: Vec<ScoredResult> = fast_results
            .iter()
            .take(refinement_limit)
            .zip(quality_scores.iter())
            .map(|(fast, &quality)| {
                // Check if this doc has a real quality embedding
                let effective_weight = if self.searcher.index.has_quality(fast.idx) {
                    weight
                } else {
                    // No quality embedding: use fast score only
                    0.0
                };
                ScoredResult {
                    idx: fast.idx,
                    doc_id: fast.doc_id,
                    doc_kind: fast.doc_kind,
                    project_id: fast.project_id,
                    score: (1.0 - effective_weight).mul_add(fast.score, effective_weight * quality),
                }
            })
            .collect();
        self.search_metrics.blend_us =
            u64::try_from(blend_start.elapsed().as_micros()).unwrap_or(u64::MAX);

        // Leave documents outside the refinement budget untouched.
        blended.extend(fast_results.iter().skip(refinement_limit).cloned());

        // Re-sort by blended score.
        let _rerank_span =
            tracing::debug_span!("two_tier.rerank", candidates = blended.len()).entered();
        blended.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        blended.truncate(self.k);
        let refined_order = blended.iter().map(|hit| hit.doc_id).collect::<Vec<_>>();
        let compare_len = self.fast_order.len().min(refined_order.len()).min(self.k);
        self.search_metrics.ranking_changed = compare_len > 0
            && self
                .fast_order
                .iter()
                .take(compare_len)
                .ne(refined_order.iter().take(compare_len));
        self.search_metrics.refined_count = refined_order.len();
        self.search_metrics.was_refined = true;
        blended
    }

    fn run_refinement_phase(&mut self) -> SearchPhase {
        {
            let _search_guard = self.search_span.enter();
        }
        let Some(quality_embedder) = &self.searcher.quality_embedder else {
            return SearchPhase::RefinementFailed {
                error: "quality embedder unavailable".to_string(),
            };
        };

        let start = Instant::now();
        let embed_span =
            tracing::debug_span!("two_tier.embed_quality", query_len = self.query.len()).entered();
        let embed_start = Instant::now();

        match quality_embedder.embed(&self.query) {
            Ok(query_vec) => {
                self.search_metrics.quality_embed_us =
                    u64::try_from(embed_start.elapsed().as_micros()).unwrap_or(u64::MAX);
                drop(embed_span);
                let results = self.build_refined_results(&query_vec);
                #[allow(clippy::cast_possible_truncation)]
                let latency_ms = start.elapsed().as_millis() as u64;
                SearchPhase::Refined {
                    results,
                    latency_ms,
                }
            }
            Err(e) => {
                self.search_metrics.quality_embed_us =
                    u64::try_from(embed_start.elapsed().as_micros()).unwrap_or(u64::MAX);
                SearchPhase::RefinementFailed {
                    error: e.to_string(),
                }
            }
        }
    }
}

impl Iterator for TwoTierSearchIter<'_> {
    type Item = SearchPhase;

    #[allow(clippy::used_underscore_binding)]
    fn next(&mut self) -> Option<Self::Item> {
        match self.phase {
            0 => {
                // Phase 1: Fast search
                self.phase = 1;
                let start = Instant::now();
                let _search_guard = self.search_span.enter();
                let _embed_fast_span =
                    tracing::debug_span!("two_tier.embed_fast", query_len = self.query.len())
                        .entered();
                let fast_embed_start = Instant::now();

                match self.searcher.fast_embedder.embed(&self.query) {
                    Ok(query_vec) => {
                        self.search_metrics.fast_embed_us =
                            u64::try_from(fast_embed_start.elapsed().as_micros())
                                .unwrap_or(u64::MAX);
                        let _search_fast_span =
                            tracing::debug_span!("two_tier.search_fast", limit = self.k).entered();
                        let fast_search_start = Instant::now();
                        let results = self.searcher.index.search_fast(&query_vec, self.k);
                        self.search_metrics.fast_search_us =
                            u64::try_from(fast_search_start.elapsed().as_micros())
                                .unwrap_or(u64::MAX);
                        self.search_metrics.fast_candidate_count = results.len();
                        self.fast_order = results.iter().map(|hit| hit.doc_id).collect();
                        #[allow(clippy::cast_possible_truncation)]
                        let latency_ms = start.elapsed().as_millis() as u64;
                        self.fast_results = Some(results.clone());

                        // If fast-only mode, skip refinement
                        if self.searcher.config.fast_only {
                            self.phase = 2;
                            return Some(SearchPhase::Initial {
                                results,
                                latency_ms,
                            });
                        }

                        // In quality-only mode, do not emit initial results.
                        if self.searcher.config.quality_only {
                            self.phase = 2;
                            drop(_embed_fast_span);
                            drop(_search_guard);
                            return Some(self.run_refinement_phase());
                        }

                        Some(SearchPhase::Initial {
                            results,
                            latency_ms,
                        })
                    }
                    Err(e) => {
                        self.search_metrics.fast_embed_us =
                            u64::try_from(fast_embed_start.elapsed().as_micros())
                                .unwrap_or(u64::MAX);
                        warn!(error = %e, "Fast embedding failed");

                        if self.searcher.config.quality_only {
                            self.phase = 2;
                            drop(_embed_fast_span);
                            drop(_search_guard);
                            return Some(self.run_refinement_phase());
                        }

                        self.phase = 2;
                        None
                    }
                }
            }
            1 => {
                // Phase 2: Quality refinement
                self.phase = 2;
                Some(self.run_refinement_phase())
            }
            _ => None,
        }
    }
}

impl Drop for TwoTierSearchIter<'_> {
    fn drop(&mut self) {
        let Some(recorder) = self.metrics_recorder.as_ref() else {
            return;
        };

        let should_record = self.search_metrics.fast_embed_us > 0
            || self.search_metrics.fast_search_us > 0
            || self.search_metrics.quality_embed_us > 0
            || self.search_metrics.quality_score_us > 0
            || self.search_metrics.blend_us > 0
            || self.search_metrics.fast_candidate_count > 0
            || self.search_metrics.refined_count > 0;
        if !should_record {
            return;
        }

        if let Ok(mut metrics) = recorder.lock() {
            metrics.record_search(self.search_metrics.clone());
        } else {
            warn!("two-tier metrics recorder lock poisoned");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[allow(clippy::cast_precision_loss)]
    fn make_test_entries(count: usize, config: &TwoTierConfig) -> Vec<TwoTierEntry> {
        (0..count)
            .map(|i| TwoTierEntry {
                doc_id: i as u64,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: (0..config.fast_dimension)
                    .map(|j| f16::from_f32((i + j) as f32 * 0.01))
                    .collect(),
                quality_embedding: (0..config.quality_dimension)
                    .map(|j| f16::from_f32((i + j) as f32 * 0.01))
                    .collect(),
                has_quality: true,
            })
            .collect()
    }

    struct StubEmbedder {
        embedder_id: &'static str,
        vector: Vec<f32>,
    }

    impl StubEmbedder {
        fn new(embedder_id: &'static str, vector: Vec<f32>) -> Self {
            Self {
                embedder_id,
                vector,
            }
        }
    }

    impl TwoTierEmbedder for StubEmbedder {
        fn embed(&self, _text: &str) -> SearchResult<Vec<f32>> {
            Ok(self.vector.clone())
        }

        fn dimension(&self) -> usize {
            self.vector.len()
        }

        fn id(&self) -> &str {
            self.embedder_id
        }
    }

    fn axis_f16_embedding(value: f32, dim: usize) -> Vec<f16> {
        let mut embedding = vec![f16::from_f32(0.0); dim];
        if let Some(first) = embedding.first_mut() {
            *first = f16::from_f32(value);
        }
        embedding
    }

    fn axis_query(dim: usize) -> Vec<f32> {
        let mut query = vec![0.0; dim];
        if let Some(first) = query.first_mut() {
            *first = 1.0;
        }
        query
    }

    fn doc_ids(results: &[ScoredResult]) -> Vec<u64> {
        results.iter().map(|hit| hit.doc_id).collect()
    }

    #[test]
    fn two_tier_search_records_metrics_when_recorder_attached() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                has_quality: true,
            })
            .expect("entry should be accepted");

        let recorder = Arc::new(Mutex::new(TwoTierMetrics::default()));
        let searcher = TwoTierSearcher::new(
            &index,
            Arc::new(StubEmbedder::new("fast", vec![1.0, 0.0])),
            Some(Arc::new(StubEmbedder::new("quality", vec![1.0, 0.0]))),
            config,
        )
        .with_metrics_recorder(Arc::clone(&recorder));

        let _ = searcher.search("hello", 1).collect::<Vec<_>>();
        let snapshot = recorder.lock().expect("metrics lock poisoned").snapshot();
        assert_eq!(snapshot.aggregated.total_searches, 1);
        assert!(snapshot.search.is_some());
    }

    #[test]
    fn test_two_tier_index_creation() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(10, &config);

        let index = TwoTierIndex::build("potion-128m", "minilm-384", &config, entries).unwrap();

        assert_eq!(index.len(), 10);
        assert!(!index.is_empty());
        assert!(matches!(
            index.metadata.status,
            IndexStatus::Complete { .. }
        ));
    }

    #[test]
    fn test_empty_index() {
        let config = TwoTierConfig::default();
        let entries: Vec<TwoTierEntry> = Vec::new();

        let index = TwoTierIndex::build("potion-128m", "minilm-384", &config, entries).unwrap();

        assert_eq!(index.len(), 0);
        assert!(index.is_empty());
    }

    #[test]
    fn test_dimension_mismatch_fast() {
        let config = TwoTierConfig::default();
        let entries = vec![TwoTierEntry {
            doc_id: 1,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); 128], // Wrong dimension
            quality_embedding: vec![f16::from_f32(1.0); config.quality_dimension],
            has_quality: true,
        }];

        let result = TwoTierIndex::build("fast", "quality", &config, entries);
        assert!(result.is_err());
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn test_fast_search() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(100, &config);
        let index = TwoTierIndex::build("potion-128m", "minilm-384", &config, entries).unwrap();

        let query: Vec<f32> = (0..config.fast_dimension)
            .map(|i| i as f32 * 0.01)
            .collect();
        let results = index.search_fast(&query, 10);

        assert_eq!(results.len(), 10);
        // Results should be sorted by score descending
        for window in results.windows(2) {
            assert!(window[0].score >= window[1].score);
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn test_quality_search() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(100, &config);
        let index = TwoTierIndex::build("potion-128m", "minilm-384", &config, entries).unwrap();

        let query: Vec<f32> = (0..config.quality_dimension)
            .map(|i| i as f32 * 0.01)
            .collect();
        let results = index.search_quality(&query, 10);

        assert_eq!(results.len(), 10);
        // Results should be sorted by score descending
        for window in results.windows(2) {
            assert!(window[0].score >= window[1].score);
        }
    }

    #[test]
    fn test_score_normalization() {
        let scores = vec![0.8, 0.6, 0.4, 0.2];
        let normalized = normalize_scores(&scores);

        assert!((normalized[0] - 1.0).abs() < 0.001);
        assert!((normalized[3] - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_score_normalization_constant() {
        let scores = vec![0.5, 0.5, 0.5];
        let normalized = normalize_scores(&scores);

        // All same value: frankensearch maps degenerate inputs to 0.5 (neutral).
        // This is more appropriate than 1.0 since all scores are equally ranked.
        for n in &normalized {
            assert!((n - 0.5).abs() < 0.001);
        }
    }

    #[test]
    fn test_score_normalization_empty() {
        let scores: Vec<f32> = vec![];
        let normalized = normalize_scores(&scores);
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_blend_scores() {
        let fast = vec![0.8, 0.6, 0.4];
        let quality = vec![0.4, 0.8, 0.6];
        let blended = blend_scores(&fast, &quality, 0.5);

        assert_eq!(blended.len(), 3);
        // With 0.5 weight, blended should be average of normalized scores
    }

    #[test]
    fn test_config_defaults() {
        let config = TwoTierConfig::default();
        assert_eq!(config.fast_dimension, 256);
        assert_eq!(config.quality_dimension, 384);
        assert!((config.quality_weight - 0.7).abs() < 0.001);
        assert_eq!(config.max_refinement_docs, 100);
        assert!(!config.fast_only);
        assert!(!config.quality_only);
    }

    #[test]
    fn test_config_fast_only() {
        let config = TwoTierConfig::fast_only();
        assert!(config.fast_only);
        assert!(!config.quality_only);
    }

    #[test]
    fn test_config_quality_only() {
        let config = TwoTierConfig::quality_only();
        assert!(!config.fast_only);
        assert!(config.quality_only);
    }

    #[test]
    fn test_dot_product_f16_basic() {
        let a: Vec<f16> = vec![f16::from_f32(1.0); 8];
        let b: Vec<f32> = vec![1.0; 8];
        let result = dot_product_f16_simd(&a, &b);
        assert!((result - 8.0).abs() < 0.01);
    }

    #[test]
    fn test_dot_product_f16_with_remainder() {
        let a: Vec<f16> = vec![f16::from_f32(1.0); 10];
        let b: Vec<f32> = vec![1.0; 10];
        let result = dot_product_f16_simd(&a, &b);
        assert!((result - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_dot_product_f16_empty() {
        let a: Vec<f16> = vec![];
        let b: Vec<f32> = vec![];
        let result = dot_product_f16_simd(&a, &b);
        assert!(result.abs() < f32::EPSILON);
    }

    #[test]
    fn test_add_entry() {
        let config = TwoTierConfig::default();
        let mut index = TwoTierIndex::new(&config);

        let entry = TwoTierEntry {
            doc_id: 42,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); config.fast_dimension],
            quality_embedding: vec![f16::from_f32(1.0); config.quality_dimension],
            has_quality: true,
        };

        index.add_entry(entry).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.doc_id(0), Some(42));
        assert!(index.has_quality(0));
    }

    #[test]
    fn test_has_quality_flag() {
        let config = TwoTierConfig::default();
        let mut index = TwoTierIndex::new(&config);

        // Add entry with quality
        let entry_with_quality = TwoTierEntry {
            doc_id: 1,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); config.fast_dimension],
            quality_embedding: vec![f16::from_f32(1.0); config.quality_dimension],
            has_quality: true,
        };
        index.add_entry(entry_with_quality).unwrap();

        // Add entry without quality (zero vector)
        let entry_without_quality = TwoTierEntry {
            doc_id: 2,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); config.fast_dimension],
            quality_embedding: vec![f16::from_f32(0.0); config.quality_dimension],
            has_quality: false,
        };
        index.add_entry(entry_without_quality).unwrap();

        assert_eq!(index.len(), 2);
        assert!(index.has_quality(0));
        assert!(!index.has_quality(1));
        assert_eq!(index.quality_count(), 1);
        assert!((index.quality_coverage() - 0.5).abs() < 0.01);
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn test_zero_quality_coverage() {
        let config = TwoTierConfig::default();
        let mut index = TwoTierIndex::new(&config);

        for i in 0..10_u64 {
            index
                .add_entry(TwoTierEntry {
                    doc_id: i,
                    doc_kind: crate::document::DocKind::Message,
                    project_id: Some(1),
                    fast_embedding: vec![f16::from_f32(0.1 * i as f32); config.fast_dimension],
                    quality_embedding: vec![f16::from_f32(0.0); config.quality_dimension],
                    has_quality: false,
                })
                .expect("entry insertion should succeed");
        }

        assert!((index.quality_coverage() - 0.0).abs() < 0.001);
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn test_full_quality_coverage() {
        let config = TwoTierConfig::default();
        let mut index = TwoTierIndex::new(&config);

        for i in 0..10_u64 {
            #[allow(clippy::cast_precision_loss)]
            let value = 0.1 * (i + 1) as f32;
            index
                .add_entry(TwoTierEntry {
                    doc_id: i,
                    doc_kind: crate::document::DocKind::Message,
                    project_id: Some(1),
                    fast_embedding: vec![f16::from_f32(value); config.fast_dimension],
                    quality_embedding: vec![f16::from_f32(value); config.quality_dimension],
                    has_quality: true,
                })
                .expect("entry insertion should succeed");
        }

        assert!((index.quality_coverage() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_zero_vector_detection() {
        let config = TwoTierConfig::default();
        let mut index = TwoTierIndex::new(&config);

        // Add entry marked as having quality but with zero vector (corruption case)
        let entry = TwoTierEntry {
            doc_id: 99,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); config.fast_dimension],
            quality_embedding: vec![f16::from_f32(0.0); config.quality_dimension],
            has_quality: true, // Marked true but embedding is zero
        };
        index.add_entry(entry).unwrap();

        // The add_entry should detect zero vector and set has_quality=false
        assert!(!index.has_quality(0));
        assert_eq!(index.quality_count(), 0);
    }

    #[test]
    fn test_migrate_zero_quality() {
        let config = TwoTierConfig::default();

        // Build index with a mix of real and zero-vector quality embeddings
        // Note: build() also detects zero vectors, so we test migration on
        // an index where has_quality_flags were manually set incorrectly
        let mut index = TwoTierIndex::new(&config);

        // Manually add entries to simulate pre-migration state
        index
            .fast_embeddings
            .extend(vec![f16::from_f32(1.0); config.fast_dimension]);
        index
            .quality_embeddings
            .extend(vec![f16::from_f32(0.0); config.quality_dimension]);
        index.doc_ids.push(1);
        index.doc_kinds.push(crate::document::DocKind::Message);
        index.project_ids.push(Some(1));
        index.has_quality_flags.push(true); // Incorrectly marked as having quality
        index.quality_doc_count = 1;
        index.metadata.doc_count = 1;

        // Before migration: incorrectly marked
        assert!(index.has_quality_flags[0]);
        assert_eq!(index.quality_count(), 1);

        // Run migration
        let migrated = index.migrate_zero_quality_to_no_quality();
        assert_eq!(migrated, 1);

        // After migration: correctly marked
        assert!(!index.has_quality(0));
        assert_eq!(index.quality_count(), 0);
    }

    #[test]
    fn test_progressive_search_emits_initial_then_refined_and_can_rerank() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            quality_weight: 1.0,
            ..TwoTierConfig::default()
        };

        let entries = vec![
            TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(3.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(0.1, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 2,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(2.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(5.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 3,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(1.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(4.0, config.quality_dimension),
                has_quality: true,
            },
        ];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 3).collect();
        assert_eq!(phases.len(), 2, "should emit initial and refined phases");

        let (initial_results, initial_latency_ms) = match &phases[0] {
            SearchPhase::Initial {
                results,
                latency_ms,
            } => (results, *latency_ms),
            _ => panic!("phase 0 should be Initial"),
        };
        let (refined_results, refinement_latency_ms) = match &phases[1] {
            SearchPhase::Refined {
                results,
                latency_ms,
            } => (results, *latency_ms),
            _ => panic!("phase 1 should be Refined"),
        };

        assert_eq!(initial_results[0].doc_id, 1);
        assert_eq!(
            refined_results[0].doc_id, 2,
            "quality refinement should be able to rerank top hit"
        );
        assert!(
            refinement_latency_ms >= initial_latency_ms,
            "refinement phase should not be faster than initial phase in this deterministic setup"
        );
    }

    #[test]
    fn test_fast_only_search_emits_initial_phase_only() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            fast_only: true,
            ..TwoTierConfig::default()
        };

        let entries = vec![
            TwoTierEntry {
                doc_id: 10,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(2.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(2.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 11,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(1.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(1.0, config.quality_dimension),
                has_quality: true,
            },
        ];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 10).collect();
        assert_eq!(phases.len(), 1, "fast-only should emit only Initial phase");
        assert!(matches!(phases[0], SearchPhase::Initial { .. }));
    }

    #[test]
    fn test_progressive_search_limit_zero_returns_empty_results() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            ..TwoTierConfig::default()
        };

        let entries = vec![TwoTierEntry {
            doc_id: 1,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: axis_f16_embedding(1.0, config.fast_dimension),
            quality_embedding: axis_f16_embedding(1.0, config.quality_dimension),
            has_quality: true,
        }];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 0).collect();
        assert_eq!(phases.len(), 2);
        for phase in &phases {
            match phase {
                SearchPhase::Initial { results, .. } | SearchPhase::Refined { results, .. } => {
                    assert!(results.is_empty());
                }
                SearchPhase::RefinementFailed { .. } => {}
            }
        }
    }

    #[test]
    fn test_progressive_search_limit_exceeds_doc_count_caps_results() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            ..TwoTierConfig::default()
        };

        let entries = vec![
            TwoTierEntry {
                doc_id: 21,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(3.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(1.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 22,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(2.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(2.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 23,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(1.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(3.0, config.quality_dimension),
                has_quality: true,
            },
        ];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 1000).collect();
        assert_eq!(phases.len(), 2);
        let initial_count = match &phases[0] {
            SearchPhase::Initial { results, .. } => results.len(),
            _ => panic!("phase 0 should be Initial"),
        };
        let refined_count = match &phases[1] {
            SearchPhase::Refined { results, .. } => results.len(),
            _ => panic!("phase 1 should be Refined"),
        };
        assert_eq!(initial_count, 3);
        assert_eq!(refined_count, 3);
    }

    #[test]
    fn test_refinement_uses_fast_scores_for_docs_without_quality_embeddings() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            quality_weight: 0.7,
            ..TwoTierConfig::default()
        };

        let entries = vec![
            TwoTierEntry {
                doc_id: 101,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(0.8, config.fast_dimension),
                quality_embedding: axis_f16_embedding(0.3, config.quality_dimension),
                has_quality: true,
            },
            // Intentionally large quality vector, but has_quality=false means
            // refinement must ignore this vector and keep fast-only scoring.
            TwoTierEntry {
                doc_id: 202,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(0.6, config.fast_dimension),
                quality_embedding: axis_f16_embedding(50.0, config.quality_dimension),
                has_quality: false,
            },
        ];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 10).collect();
        assert_eq!(phases.len(), 2);
        let SearchPhase::Refined {
            results: refined, ..
        } = &phases[1]
        else {
            panic!("phase 1 should be Refined");
        };
        assert_eq!(refined.len(), 2);
        assert_eq!(
            refined[0].doc_id, 202,
            "doc without quality embedding should keep fast score and outrank doc 101"
        );
    }

    #[test]
    fn test_quality_only_search_emits_refined_phase_first() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            quality_only: true,
            ..TwoTierConfig::default()
        };

        let entries = vec![
            TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(2.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(2.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 2,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(1.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(1.0, config.quality_dimension),
                has_quality: true,
            },
        ];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 2).collect();
        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0], SearchPhase::Refined { .. }));
    }

    #[test]
    fn test_max_refinement_docs_zero_returns_fast_results_unchanged() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            quality_weight: 1.0,
            max_refinement_docs: 0,
            ..TwoTierConfig::default()
        };

        let entries = vec![
            TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(3.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(1.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 2,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(2.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(2.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 3,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(1.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(3.0, config.quality_dimension),
                has_quality: true,
            },
        ];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 3).collect();
        assert_eq!(phases.len(), 2);
        assert!(matches!(phases[0], SearchPhase::Initial { .. }));
        assert!(matches!(phases[1], SearchPhase::Refined { .. }));
        let initial_ids = if let SearchPhase::Initial { results, .. } = &phases[0] {
            doc_ids(results)
        } else {
            Vec::new()
        };
        let refined_ids = if let SearchPhase::Refined { results, .. } = &phases[1] {
            doc_ids(results)
        } else {
            Vec::new()
        };
        assert_eq!(refined_ids, initial_ids);
    }

    #[test]
    fn test_max_refinement_docs_limits_refinement_scope() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            quality_weight: 1.0,
            max_refinement_docs: 1,
            ..TwoTierConfig::default()
        };

        let entries = vec![
            TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(3.0, config.fast_dimension),
                quality_embedding: axis_f16_embedding(0.1, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 2,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(0.2, config.fast_dimension),
                quality_embedding: axis_f16_embedding(200.0, config.quality_dimension),
                has_quality: true,
            },
            TwoTierEntry {
                doc_id: 3,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: axis_f16_embedding(0.05, config.fast_dimension),
                quality_embedding: axis_f16_embedding(300.0, config.quality_dimension),
                has_quality: true,
            },
        ];

        let index = TwoTierIndex::build("fast", "quality", &config, entries).unwrap();
        let fast_embedder = Arc::new(StubEmbedder::new("fast", axis_query(config.fast_dimension)));
        let quality_embedder = Arc::new(StubEmbedder::new(
            "quality",
            axis_query(config.quality_dimension),
        ));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 3).collect();
        assert_eq!(phases.len(), 2);
        assert!(matches!(phases[1], SearchPhase::Refined { .. }));
        let refined_ids = if let SearchPhase::Refined { results, .. } = &phases[1] {
            doc_ids(results)
        } else {
            Vec::new()
        };

        // With refinement capped to 1 candidate, doc 3 must not jump to rank 1.
        assert_eq!(refined_ids[0], 2);
    }

    // ────────────────────────────────────────────────────────────────
    // TC8: Concurrent read/write (search while adding documents)
    // ────────────────────────────────────────────────────────────────

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn test_concurrent_search_while_adding_documents() {
        use std::sync::{Barrier, RwLock};
        use std::thread;

        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let index = Arc::new(RwLock::new(TwoTierIndex::new(&config)));
        let barrier = Arc::new(Barrier::new(2));

        // Writer thread: adds 50 documents
        let writer_index = Arc::clone(&index);
        let writer_barrier = Arc::clone(&barrier);
        let fast_dim = config.fast_dimension;
        let quality_dim = config.quality_dimension;
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            let mut success_count = 0_u32;
            for i in 0..50_u64 {
                let value = 0.1 * (i + 1) as f32;
                let entry = TwoTierEntry {
                    doc_id: i,
                    doc_kind: crate::document::DocKind::Message,
                    project_id: Some(1),
                    fast_embedding: vec![f16::from_f32(value); fast_dim],
                    quality_embedding: vec![f16::from_f32(value); quality_dim],
                    has_quality: true,
                };
                let mut guard = writer_index.write().expect("write lock");
                if guard.add_entry(entry).is_ok() {
                    success_count += 1;
                }
                drop(guard);
                // Small yield to interleave with reader
                thread::yield_now();
            }
            success_count
        });

        // Reader thread: searches repeatedly while writer adds docs
        let reader_index = Arc::clone(&index);
        let reader_barrier = Arc::clone(&barrier);
        let reader = thread::spawn(move || {
            reader_barrier.wait();
            let query = vec![1.0, 0.0, 0.0, 0.0];
            let mut search_count = 0_u32;
            for _ in 0..100 {
                let guard = reader_index.read().expect("read lock");
                let _results = guard.search_fast(&query, 10);
                search_count += 1;
                drop(guard);
                thread::yield_now();
            }
            search_count
        });

        let write_count = writer.join().expect("writer thread should not panic");
        let read_count = reader.join().expect("reader thread should not panic");

        assert_eq!(write_count, 50, "all 50 documents should be added");
        assert_eq!(read_count, 100, "all 100 searches should complete");

        // Verify final index state
        let final_len = index.read().expect("read lock").len();
        assert_eq!(final_len, 50, "index should contain all 50 documents");
    }

    // ────────────────────────────────────────────────────────────────
    // TC9: Multiple concurrent searches
    // ────────────────────────────────────────────────────────────────

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn test_concurrent_searches_return_deterministic_results() {
        use std::sync::Barrier;
        use std::thread;

        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };

        // Build index with test data
        let mut index = TwoTierIndex::new(&config);
        for i in 0..100_u64 {
            let value = 0.01 * (i + 1) as f32;
            index
                .add_entry(TwoTierEntry {
                    doc_id: i,
                    doc_kind: crate::document::DocKind::Message,
                    project_id: Some(1),
                    fast_embedding: vec![f16::from_f32(value); config.fast_dimension],
                    quality_embedding: vec![f16::from_f32(value); config.quality_dimension],
                    has_quality: true,
                })
                .expect("add_entry should succeed");
        }

        let index = Arc::new(index);
        let thread_count = 10;
        let barrier = Arc::new(Barrier::new(thread_count));

        #[allow(clippy::needless_collect)] // collect required: barrier needs all threads spawned
        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let idx = Arc::clone(&index);
                let bar = Arc::clone(&barrier);
                thread::spawn(move || {
                    bar.wait();
                    let query = vec![1.0, 0.0, 0.0, 0.0];
                    idx.search_fast(&query, 10)
                })
            })
            .collect();

        let all_results: Vec<Vec<ScoredResult>> = handles
            .into_iter()
            .map(|h| h.join().expect("search thread should not panic"))
            .collect();

        // All threads should return results
        for (i, results) in all_results.iter().enumerate() {
            assert!(
                !results.is_empty(),
                "thread {i} should return search results"
            );
            assert_eq!(
                results.len(),
                10,
                "thread {i} should return exactly 10 results"
            );
        }

        // All threads should return identical results (deterministic)
        let first_ids: Vec<u64> = all_results[0].iter().map(|r| r.doc_id).collect();
        for (i, results) in all_results.iter().enumerate().skip(1) {
            let ids: Vec<u64> = results.iter().map(|r| r.doc_id).collect();
            assert_eq!(
                ids, first_ids,
                "thread {i} results should match thread 0 results"
            );
        }
    }

    // ────────────────────────────────────────────────────────────────
    // TC6: Embedder failure handling during search
    // ────────────────────────────────────────────────────────────────

    struct FailingEmbedder;

    impl TwoTierEmbedder for FailingEmbedder {
        fn embed(&self, _text: &str) -> SearchResult<Vec<f32>> {
            Err(SearchError::ModeUnavailable(
                "simulated embedder failure".into(),
            ))
        }

        fn dimension(&self) -> usize {
            4
        }

        #[allow(clippy::unnecessary_literal_bound)]
        fn id(&self) -> &str {
            "failing-embedder"
        }
    }

    #[test]
    fn test_search_with_failing_fast_embedder_returns_none() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };

        let index = TwoTierIndex::new(&config);
        let fast_embedder: Arc<dyn TwoTierEmbedder> = Arc::new(FailingEmbedder);
        let searcher = TwoTierSearcher::new(&index, fast_embedder, None, config);

        // With failing fast embedder in normal mode, iterator yields nothing
        assert_eq!(
            searcher.search("query", 10).count(),
            0,
            "failing fast embedder should yield no phases"
        );
    }

    #[test]
    fn test_search_with_failing_quality_embedder_returns_refinement_failed() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };

        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0); 4],
                quality_embedding: vec![f16::from_f32(1.0); 4],
                has_quality: true,
            })
            .unwrap();

        let fast_embedder: Arc<dyn TwoTierEmbedder> =
            Arc::new(StubEmbedder::new("fast", vec![1.0, 0.0, 0.0, 0.0]));
        let quality_embedder: Arc<dyn TwoTierEmbedder> = Arc::new(FailingEmbedder);
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 10).collect();
        assert_eq!(phases.len(), 2, "should yield initial + refinement failed");
        assert!(
            matches!(phases[0], SearchPhase::Initial { .. }),
            "first phase should be initial results"
        );
        assert!(
            matches!(phases[1], SearchPhase::RefinementFailed { .. }),
            "second phase should be refinement failed"
        );
    }

    #[test]
    fn test_quality_only_with_failing_fast_falls_back_to_quality() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            quality_only: true,
            ..TwoTierConfig::default()
        };

        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0); 4],
                quality_embedding: vec![f16::from_f32(1.0); 4],
                has_quality: true,
            })
            .unwrap();

        let fast_embedder: Arc<dyn TwoTierEmbedder> = Arc::new(FailingEmbedder);
        let quality_embedder: Arc<dyn TwoTierEmbedder> =
            Arc::new(StubEmbedder::new("quality", vec![1.0, 0.0, 0.0, 0.0]));
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(quality_embedder), config);

        let phases: Vec<SearchPhase> = searcher.search("query", 10).collect();
        // In quality_only mode with failing fast, should still try quality refinement
        assert_eq!(phases.len(), 1, "quality_only should yield one phase");
        assert!(
            matches!(phases[0], SearchPhase::Refined { .. }),
            "should get refined results even with failing fast embedder"
        );
    }

    #[test]
    fn test_no_quality_embedder_yields_refinement_failed() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };

        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0); 4],
                quality_embedding: vec![f16::from_f32(1.0); 4],
                has_quality: true,
            })
            .unwrap();

        let fast_embedder: Arc<dyn TwoTierEmbedder> =
            Arc::new(StubEmbedder::new("fast", vec![1.0, 0.0, 0.0, 0.0]));
        // No quality embedder provided
        let searcher = TwoTierSearcher::new(&index, fast_embedder, None, config);

        let phases: Vec<SearchPhase> = searcher.search("query", 10).collect();
        assert_eq!(phases.len(), 2);
        assert!(matches!(phases[0], SearchPhase::Initial { .. }));
        assert!(
            matches!(&phases[1], SearchPhase::RefinementFailed { error } if error.contains("unavailable")),
            "should report quality embedder unavailable"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // TC8b: High-contention concurrent read/write stress test
    // ────────────────────────────────────────────────────────────────

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn test_high_contention_concurrent_read_write() {
        use std::sync::{Barrier, RwLock};
        use std::thread;

        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let index = Arc::new(RwLock::new(TwoTierIndex::new(&config)));

        let writer_count = 3;
        let reader_count = 7;
        let total = writer_count + reader_count;
        let barrier = Arc::new(Barrier::new(total));

        let mut handles = Vec::with_capacity(total);

        // Spawn writer threads
        for w in 0..writer_count {
            let idx = Arc::clone(&index);
            let bar = Arc::clone(&barrier);
            let cfg = config.clone();
            handles.push(thread::spawn(move || {
                bar.wait();
                let mut count = 0_u32;
                for i in 0..20_u64 {
                    let doc_id = (w as u64) * 100 + i;
                    let value = 0.01 * (doc_id + 1) as f32;
                    let entry = TwoTierEntry {
                        doc_id,
                        doc_kind: crate::document::DocKind::Message,
                        project_id: Some(1),
                        fast_embedding: vec![f16::from_f32(value); cfg.fast_dimension],
                        quality_embedding: vec![f16::from_f32(value); cfg.quality_dimension],
                        has_quality: true,
                    };
                    let mut guard = idx.write().expect("write lock");
                    if guard.add_entry(entry).is_ok() {
                        count += 1;
                    }
                    drop(guard);
                    thread::yield_now();
                }
                count
            }));
        }

        // Spawn reader threads
        for _ in 0..reader_count {
            let idx = Arc::clone(&index);
            let bar = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                bar.wait();
                let query = vec![1.0, 0.0, 0.0, 0.0];
                let mut count = 0_u32;
                for _ in 0..50 {
                    let guard = idx.read().expect("read lock");
                    let _results = guard.search_fast(&query, 5);
                    count += 1;
                    drop(guard);
                    thread::yield_now();
                }
                count
            }));
        }

        let results: Vec<u32> = handles
            .into_iter()
            .map(|h| h.join().expect("thread should not panic"))
            .collect();

        // Verify writer results
        for (i, &count) in results.iter().take(writer_count).enumerate() {
            assert_eq!(count, 20, "writer {i} should add all 20 docs");
        }

        // Verify reader results
        for (i, &count) in results.iter().skip(writer_count).enumerate() {
            assert_eq!(count, 50, "reader {i} should complete all 50 searches");
        }

        // Verify final state
        let final_len = index.read().expect("read lock").len();
        assert_eq!(
            final_len, 60,
            "index should contain 60 docs (3 writers x 20)"
        );
    }

    // ── Trait coverage and edge case tests ─────────────────────────

    #[test]
    fn two_tier_config_serde_roundtrip() {
        let config = TwoTierConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: TwoTierConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.fast_dimension, 256);
        assert_eq!(restored.quality_dimension, 384);
        assert!((restored.quality_weight - 0.7).abs() < 0.001);
        assert_eq!(restored.max_refinement_docs, 100);
        assert!(!restored.fast_only);
        assert!(!restored.quality_only);
    }

    #[test]
    fn two_tier_metadata_serde_roundtrip() {
        let meta = TwoTierMetadata {
            fast_embedder_id: "fast".to_owned(),
            quality_embedder_id: "quality".to_owned(),
            doc_count: 42,
            built_at: 1_700_000_000,
            status: IndexStatus::Complete {
                fast_latency_ms: 1,
                quality_latency_ms: 10,
            },
        };
        let json = serde_json::to_string(&meta).unwrap();
        let restored: TwoTierMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.doc_count, 42);
        assert_eq!(restored.fast_embedder_id, "fast");
    }

    #[test]
    fn index_status_serde_all_variants() {
        let variants: Vec<IndexStatus> = vec![
            IndexStatus::Building { progress: 0.5 },
            IndexStatus::Complete {
                fast_latency_ms: 1,
                quality_latency_ms: 10,
            },
            IndexStatus::Failed {
                error: "boom".to_owned(),
            },
        ];
        for status in &variants {
            let json = serde_json::to_string(status).unwrap();
            let restored: IndexStatus = serde_json::from_str(&json).unwrap();
            let debug = format!("{restored:?}");
            assert!(!debug.is_empty());
        }
    }

    #[test]
    fn doc_id_out_of_bounds_returns_none() {
        let config = TwoTierConfig::default();
        let index = TwoTierIndex::new(&config);
        assert!(index.doc_id(0).is_none());
        assert!(index.doc_id(100).is_none());
    }

    #[test]
    fn has_quality_out_of_bounds_returns_false() {
        let config = TwoTierConfig::default();
        let index = TwoTierIndex::new(&config);
        assert!(!index.has_quality(0));
        assert!(!index.has_quality(usize::MAX));
    }

    #[test]
    fn quality_coverage_empty_is_one() {
        let config = TwoTierConfig::default();
        let index = TwoTierIndex::new(&config);
        assert!((index.quality_coverage() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_scores_negative_values() {
        let scores = vec![-1.0, 0.0, 1.0];
        let normalized = normalize_scores(&scores);
        assert!((normalized[0] - 0.0).abs() < 0.001); // min → 0
        assert!((normalized[1] - 0.5).abs() < 0.001); // mid → 0.5
        assert!((normalized[2] - 1.0).abs() < 0.001); // max → 1
    }

    #[test]
    fn blend_scores_zero_weight_fast_only() {
        let fast = vec![0.8, 0.2];
        let quality = vec![0.2, 0.8];
        let blended = blend_scores(&fast, &quality, 0.0);
        // weight=0.0 means fast-only after normalization
        assert!((blended[0] - 1.0).abs() < 0.001);
        assert!((blended[1] - 0.0).abs() < 0.001);
    }

    #[test]
    fn blend_scores_full_weight_quality_only() {
        let fast = vec![0.8, 0.2];
        let quality = vec![0.2, 0.8];
        let blended = blend_scores(&fast, &quality, 1.0);
        // weight=1.0 means quality-only after normalization
        assert!((blended[0] - 0.0).abs() < 0.001);
        assert!((blended[1] - 1.0).abs() < 0.001);
    }

    #[test]
    fn search_fast_k_zero_returns_empty() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0); 4],
                quality_embedding: vec![f16::from_f32(1.0); 4],
                has_quality: true,
            })
            .unwrap();
        let results = index.search_fast(&[1.0, 0.0, 0.0, 0.0], 0);
        assert!(results.is_empty());
    }

    #[test]
    fn search_fast_k_larger_than_doc_count_returns_all_docs() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 10,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(2.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(0.0), f16::from_f32(0.0)],
                has_quality: false,
            })
            .unwrap();
        index
            .add_entry(TwoTierEntry {
                doc_id: 20,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(0.0), f16::from_f32(0.0)],
                has_quality: false,
            })
            .unwrap();

        let results = index.search_fast(&[1.0, 0.0], 100);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].doc_id, 10);
        assert_eq!(results[1].doc_id, 20);
    }

    #[test]
    fn search_quality_k_larger_than_quality_count_returns_quality_docs_only() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 100,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(0.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(2.0), f16::from_f32(0.0)],
                has_quality: true,
            })
            .unwrap();
        index
            .add_entry(TwoTierEntry {
                doc_id: 200,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(0.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                has_quality: true,
            })
            .unwrap();
        index
            .add_entry(TwoTierEntry {
                doc_id: 300,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(0.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(0.0), f16::from_f32(0.0)],
                has_quality: false,
            })
            .unwrap();

        let results = index.search_quality(&[1.0, 0.0], 100);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].doc_id, 100);
        assert_eq!(results[1].doc_id, 200);
    }

    #[test]
    fn search_fast_wrong_dimension_returns_empty() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0); 4],
                quality_embedding: vec![f16::from_f32(1.0); 4],
                has_quality: true,
            })
            .unwrap();
        // Query with wrong dimension (2 instead of 4)
        let results = index.search_fast(&[1.0, 0.0], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn search_quality_wrong_dimension_returns_empty() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: Some(1),
                fast_embedding: vec![f16::from_f32(1.0); 4],
                quality_embedding: vec![f16::from_f32(1.0); 4],
                has_quality: true,
            })
            .unwrap();
        let results = index.search_quality(&[1.0], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn add_entry_wrong_quality_dimension_error() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        let result = index.add_entry(TwoTierEntry {
            doc_id: 1,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); 4],
            quality_embedding: vec![f16::from_f32(1.0); 2], // wrong dim
            has_quality: true,
        });
        assert!(result.is_err());
    }

    #[test]
    fn add_entry_wrong_fast_dimension_error() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);
        let result = index.add_entry(TwoTierEntry {
            doc_id: 1,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); 2], // wrong dim
            quality_embedding: vec![f16::from_f32(1.0); 4],
            has_quality: true,
        });
        assert!(result.is_err());
    }

    #[test]
    fn detect_zero_quality_docs_empty_index() {
        let config = TwoTierConfig::default();
        let index = TwoTierIndex::new(&config);
        assert!(index.detect_zero_quality_docs().is_empty());
    }

    #[test]
    fn dimension_mismatch_quality_in_build() {
        let config = TwoTierConfig::default();
        let entries = vec![TwoTierEntry {
            doc_id: 1,
            doc_kind: crate::document::DocKind::Message,
            project_id: Some(1),
            fast_embedding: vec![f16::from_f32(1.0); config.fast_dimension],
            quality_embedding: vec![f16::from_f32(1.0); 2], // wrong quality dim
            has_quality: true,
        }];
        let result = TwoTierIndex::build("fast", "quality", &config, entries);
        assert!(result.is_err());
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn scored_result_debug_clone() {
        let sr = ScoredResult {
            idx: 0,
            doc_id: 42,
            doc_kind: crate::document::DocKind::Agent,
            project_id: None,
            score: 0.99,
        };
        let debug = format!("{sr:?}");
        assert!(debug.contains("42"));
        let cloned = sr.clone();
        assert_eq!(cloned.doc_id, 42);
        assert!(cloned.project_id.is_none());
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn search_phase_debug_clone() {
        let phase = SearchPhase::Initial {
            results: vec![],
            latency_ms: 5,
        };
        let debug = format!("{phase:?}");
        assert!(debug.contains("Initial"));
        let cloned = phase.clone();
        assert!(matches!(cloned, SearchPhase::Initial { latency_ms: 5, .. }));

        let failed = SearchPhase::RefinementFailed {
            error: "test".to_owned(),
        };
        let debug2 = format!("{failed:?}");
        assert!(debug2.contains("RefinementFailed"));
    }

    #[test]
    fn is_zero_vector_f16_mixed() {
        // Not zero — should return false
        let non_zero = vec![f16::from_f32(0.0), f16::from_f32(0.001)];
        assert!(!is_zero_vector_f16(&non_zero));

        // All zero — should return true
        let zero = vec![f16::from_f32(0.0), f16::from_f32(0.0)];
        assert!(is_zero_vector_f16(&zero));

        // Empty — should return true (all elements are zero, vacuously)
        assert!(is_zero_vector_f16(&[]));
    }

    #[test]
    fn two_tier_index_debug() {
        let config = TwoTierConfig::default();
        let index = TwoTierIndex::new(&config);
        let debug = format!("{index:?}");
        assert!(debug.contains("TwoTierIndex"));
    }

    #[test]
    fn quality_scores_for_indices_out_of_bounds() {
        let config = TwoTierConfig {
            fast_dimension: 4,
            quality_dimension: 4,
            ..TwoTierConfig::default()
        };
        let index = TwoTierIndex::new(&config);
        // Out of bounds indices should return 0.0
        let scores = index.quality_scores_for_indices(&[1.0, 0.0, 0.0, 0.0], &[0, 1, 999]);
        assert_eq!(scores.len(), 3);
        for &s in &scores {
            assert!(s.abs() < f32::EPSILON);
        }
    }

    #[test]
    fn quality_scores_ignore_docs_without_quality_embeddings() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);

        index
            .add_entry(TwoTierEntry {
                doc_id: 1,
                doc_kind: crate::document::DocKind::Message,
                project_id: None,
                fast_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                has_quality: true,
            })
            .expect("entry with quality should be accepted");

        // This row has a non-zero quality vector but is explicitly marked as no-quality.
        index
            .add_entry(TwoTierEntry {
                doc_id: 2,
                doc_kind: crate::document::DocKind::Message,
                project_id: None,
                fast_embedding: vec![f16::from_f32(0.0), f16::from_f32(1.0)],
                quality_embedding: vec![f16::from_f32(50.0), f16::from_f32(50.0)],
                has_quality: false,
            })
            .expect("entry without quality should be accepted");

        let scores = index.quality_scores_for_indices(&[1.0, 0.0], &[0, 1]);
        assert_eq!(scores.len(), 2);
        assert!(scores[0] > 0.9);
        assert!(scores[1].abs() < f32::EPSILON);
    }

    #[test]
    fn quality_search_skips_docs_without_quality_embeddings() {
        let config = TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            ..TwoTierConfig::default()
        };
        let mut index = TwoTierIndex::new(&config);

        index
            .add_entry(TwoTierEntry {
                doc_id: 10,
                doc_kind: crate::document::DocKind::Message,
                project_id: None,
                fast_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                quality_embedding: vec![f16::from_f32(1.0), f16::from_f32(0.0)],
                has_quality: true,
            })
            .expect("quality entry should be accepted");

        index
            .add_entry(TwoTierEntry {
                doc_id: 11,
                doc_kind: crate::document::DocKind::Message,
                project_id: None,
                fast_embedding: vec![f16::from_f32(0.0), f16::from_f32(1.0)],
                quality_embedding: vec![f16::from_f32(100.0), f16::from_f32(100.0)],
                has_quality: false,
            })
            .expect("no-quality entry should be accepted");

        let results = index.search_quality(&[1.0, 0.0], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 10);
    }
}
