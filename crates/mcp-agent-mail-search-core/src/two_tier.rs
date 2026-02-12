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
use wide::f32x8;

use crate::error::{SearchError, SearchResult};

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
    /// Configuration.
    config: TwoTierConfig,
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

        for entry in entries {
            fast_embeddings.extend(entry.fast_embedding);
            quality_embeddings.extend(entry.quality_embedding);
            doc_ids.push(entry.doc_id);
            doc_kinds.push(entry.doc_kind);
            project_ids.push(entry.project_id);
        }

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

        let mut heap = BinaryHeap::with_capacity(k + 1);

        for idx in 0..self.metadata.doc_count {
            if let Some(embedding) = self.fast_embedding(idx) {
                let score = dot_product_f16_simd(embedding, query_vec);
                heap.push(std::cmp::Reverse(ScoredEntry { score, idx }));
                if heap.len() > k {
                    heap.pop();
                }
            }
        }

        heap.into_sorted_vec()
            .into_iter()
            .map(|std::cmp::Reverse(entry)| ScoredResult {
                idx: entry.idx,
                doc_id: self.doc_ids[entry.idx],
                doc_kind: self
                    .doc_kinds
                    .get(entry.idx)
                    .copied()
                    .unwrap_or(crate::document::DocKind::Message),
                project_id: self.project_ids.get(entry.idx).copied().flatten(),
                score: entry.score,
            })
            .collect()
    }

    /// Search using quality embeddings only.
    #[must_use]
    pub fn search_quality(&self, query_vec: &[f32], k: usize) -> Vec<ScoredResult> {
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

        let mut heap = BinaryHeap::with_capacity(k + 1);

        for idx in 0..self.metadata.doc_count {
            if let Some(embedding) = self.quality_embedding(idx) {
                let score = dot_product_f16_simd(embedding, query_vec);
                heap.push(std::cmp::Reverse(ScoredEntry { score, idx }));
                if heap.len() > k {
                    heap.pop();
                }
            }
        }

        heap.into_sorted_vec()
            .into_iter()
            .map(|std::cmp::Reverse(entry)| ScoredResult {
                idx: entry.idx,
                doc_id: self.doc_ids[entry.idx],
                doc_kind: self
                    .doc_kinds
                    .get(entry.idx)
                    .copied()
                    .unwrap_or(crate::document::DocKind::Message),
                project_id: self.project_ids.get(entry.idx).copied().flatten(),
                score: entry.score,
            })
            .collect()
    }

    /// Get quality scores for a set of document indices.
    #[must_use]
    pub fn quality_scores_for_indices(&self, query_vec: &[f32], indices: &[usize]) -> Vec<f32> {
        indices
            .iter()
            .map(|&idx| {
                self.quality_embedding(idx)
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
        if entry.fast_embedding.len() != self.config.fast_dimension {
            return Err(SearchError::InvalidQuery(format!(
                "fast embedding dimension mismatch: expected {}, got {}",
                self.config.fast_dimension,
                entry.fast_embedding.len()
            )));
        }
        if entry.quality_embedding.len() != self.config.quality_dimension {
            return Err(SearchError::InvalidQuery(format!(
                "quality embedding dimension mismatch: expected {}, got {}",
                self.config.quality_dimension,
                entry.quality_embedding.len()
            )));
        }

        self.fast_embeddings.extend(entry.fast_embedding);
        self.quality_embeddings.extend(entry.quality_embedding);
        self.doc_ids.push(entry.doc_id);
        self.doc_kinds.push(entry.doc_kind);
        self.project_ids.push(entry.project_id);
        self.metadata.doc_count += 1;

        Ok(())
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
/// Uses `wide::f32x8` for 8-way SIMD parallelism.
#[inline]
#[must_use]
pub fn dot_product_f16_simd(embedding: &[f16], query: &[f32]) -> f32 {
    debug_assert_eq!(
        embedding.len(),
        query.len(),
        "dot_product_f16_simd: dimension mismatch (embedding={}, query={})",
        embedding.len(),
        query.len()
    );

    // Early return for mismatched lengths in release mode
    if embedding.len() != query.len() {
        return 0.0;
    }

    if embedding.is_empty() {
        return 0.0;
    }

    let chunks = embedding.len() / 8;
    let mut sum = f32x8::ZERO;

    for i in 0..chunks {
        let base = i * 8;
        let emb_f32 = [
            f32::from(embedding[base]),
            f32::from(embedding[base + 1]),
            f32::from(embedding[base + 2]),
            f32::from(embedding[base + 3]),
            f32::from(embedding[base + 4]),
            f32::from(embedding[base + 5]),
            f32::from(embedding[base + 6]),
            f32::from(embedding[base + 7]),
        ];
        // Convert query slice to array
        let q_arr: [f32; 8] = query[base..base + 8]
            .try_into()
            .expect("slice length mismatch in SIMD chunk");
        sum += f32x8::from(emb_f32) * f32x8::from(q_arr);
    }

    let mut result: f32 = sum.reduce_add();

    // Handle remainder
    let remainder_start = chunks * 8;
    for i in remainder_start..embedding.len() {
        result += f32::from(embedding[i]) * query[i];
    }

    result
}

/// Normalize scores to [0, 1] range using min-max scaling.
#[must_use]
pub fn normalize_scores(scores: &[f32]) -> Vec<f32> {
    if scores.is_empty() {
        return Vec::new();
    }

    let min = scores.iter().copied().fold(f32::INFINITY, f32::min);
    let max = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let range = max - min;

    if range.abs() < f32::EPSILON {
        return vec![1.0; scores.len()];
    }

    scores.iter().map(|&s| (s - min) / range).collect()
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
        }
    }

    /// Perform two-tier progressive search.
    ///
    /// Returns an iterator that yields search phases:
    /// 1. Initial results from fast embeddings
    /// 2. Refined results from quality embeddings (if available)
    pub fn search(&self, query: &str, k: usize) -> impl Iterator<Item = SearchPhase> + '_ {
        TwoTierSearchIter::new(self, query.to_string(), k)
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
}

impl<'a> TwoTierSearchIter<'a> {
    #[allow(clippy::missing_const_for_fn)]
    fn new(searcher: &'a TwoTierSearcher<'a>, query: String, k: usize) -> Self {
        Self {
            searcher,
            query,
            k,
            phase: 0,
            fast_results: None,
        }
    }
}

impl Iterator for TwoTierSearchIter<'_> {
    type Item = SearchPhase;

    fn next(&mut self) -> Option<Self::Item> {
        match self.phase {
            0 => {
                // Phase 1: Fast search
                self.phase = 1;
                let start = Instant::now();

                match self.searcher.fast_embedder.embed(&self.query) {
                    Ok(query_vec) => {
                        let results = self.searcher.index.search_fast(&query_vec, self.k);
                        #[allow(clippy::cast_possible_truncation)]
                        let latency_ms = start.elapsed().as_millis() as u64;
                        self.fast_results = Some(results.clone());

                        // If fast-only mode, skip refinement
                        if self.searcher.config.fast_only {
                            self.phase = 2;
                        }

                        Some(SearchPhase::Initial {
                            results,
                            latency_ms,
                        })
                    }
                    Err(e) => {
                        warn!(error = %e, "Fast embedding failed");
                        self.phase = 2;
                        None
                    }
                }
            }
            1 => {
                // Phase 2: Quality refinement
                self.phase = 2;

                let Some(quality_embedder) = &self.searcher.quality_embedder else {
                    return Some(SearchPhase::RefinementFailed {
                        error: "quality embedder unavailable".to_string(),
                    });
                };

                let start = Instant::now();

                match quality_embedder.embed(&self.query) {
                    Ok(query_vec) => {
                        // Get candidate indices from fast results
                        let candidates: Vec<usize> = self
                            .fast_results
                            .as_ref()
                            .map(|r| r.iter().map(|sr| sr.idx).collect())
                            .unwrap_or_default();

                        // If we have fast results, blend scores; otherwise full quality search
                        let results = if candidates.is_empty() {
                            self.searcher.index.search_quality(&query_vec, self.k)
                        } else {
                            let fast_results = self.fast_results.as_ref().unwrap();
                            let quality_scores = self
                                .searcher
                                .index
                                .quality_scores_for_indices(&query_vec, &candidates);

                            // Blend scores
                            let weight = self.searcher.config.quality_weight;
                            let mut blended: Vec<ScoredResult> = fast_results
                                .iter()
                                .zip(quality_scores.iter())
                                .map(|(fast, &quality)| ScoredResult {
                                    idx: fast.idx,
                                    doc_id: fast.doc_id,
                                    doc_kind: fast.doc_kind,
                                    project_id: fast.project_id,
                                    score: (1.0 - weight).mul_add(fast.score, weight * quality),
                                })
                                .collect();

                            // Re-sort by blended score
                            blended.sort_by(|a, b| {
                                b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal)
                            });
                            blended.truncate(self.k);
                            blended
                        };

                        #[allow(clippy::cast_possible_truncation)]
                        let latency_ms = start.elapsed().as_millis() as u64;
                        Some(SearchPhase::Refined {
                            results,
                            latency_ms,
                        })
                    }
                    Err(e) => Some(SearchPhase::RefinementFailed {
                        error: e.to_string(),
                    }),
                }
            }
            _ => None,
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
            })
            .collect()
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

        // All same value should normalize to 1.0
        for n in &normalized {
            assert!((n - 1.0).abs() < 0.001);
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
        };

        index.add_entry(entry).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.doc_id(0), Some(42));
    }
}
