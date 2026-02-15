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
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Instant;

use half::f16;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

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

        for entry in entries {
            // Determine has_quality: use explicit flag if set, otherwise detect zero vectors
            let has_quality = entry.has_quality && !is_zero_vector_f16(&entry.quality_embedding);
            fast_embeddings.extend(entry.fast_embedding);
            quality_embeddings.extend(entry.quality_embedding);
            doc_ids.push(entry.doc_id);
            doc_kinds.push(entry.doc_kind);
            project_ids.push(entry.project_id);
            has_quality_flags.push(has_quality);
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
            has_quality_flags,
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
    pub fn quality_count(&self) -> usize {
        self.has_quality_flags.iter().filter(|&&v| v).count()
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
            if self.has_quality_flags.get(idx).copied().unwrap_or(false) {
                if let Some(emb) = self.quality_embedding(idx) {
                    if is_zero_vector_f16(emb) {
                        self.has_quality_flags[idx] = false;
                        count += 1;
                    }
                }
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

        // Determine has_quality: use explicit flag if set, otherwise detect zero vectors
        let has_quality = entry.has_quality && !is_zero_vector_f16(&entry.quality_embedding);

        self.fast_embeddings.extend(entry.fast_embedding);
        self.quality_embeddings.extend(entry.quality_embedding);
        self.doc_ids.push(entry.doc_id);
        self.doc_kinds.push(entry.doc_kind);
        self.project_ids.push(entry.project_id);
        self.has_quality_flags.push(has_quality);
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

/// Separator used when encoding probe document IDs for frankensearch.
const FS_TWO_TIER_DOC_KEY_SEPARATOR: &str = "::";

/// Monotonic counter for unique probe directory names.
static FS_TWO_TIER_PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct ProbeDocMetadata {
    idx: usize,
    doc_kind: crate::document::DocKind,
    project_id: Option<i64>,
}

fn to_fs_probe_doc_key(doc_id: u64, idx: usize) -> String {
    format!("{doc_id}{FS_TWO_TIER_DOC_KEY_SEPARATOR}{idx}")
}

fn parse_fs_probe_doc_id(doc_key: &str) -> Option<u64> {
    doc_key
        .split_once(FS_TWO_TIER_DOC_KEY_SEPARATOR)
        .map_or_else(|| doc_key.parse().ok(), |(doc_id, _)| doc_id.parse().ok())
}

fn fs_two_tier_probe_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    let nonce = FS_TWO_TIER_PROBE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let pid = std::process::id();
    path.push(format!("mcp-agent-mail-two-tier-probe-{pid}-{nonce}"));
    path
}

fn f16_slice_to_f32_vec(values: &[f16]) -> Vec<f32> {
    values.iter().map(|&v| f32::from(v)).collect()
}

fn duration_millis_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn from_fs_scored_result_with_metadata(
    result: &crate::fs_bridge::FsScoredResult,
    metadata_by_doc_key: &HashMap<String, ProbeDocMetadata>,
) -> Option<ScoredResult> {
    let doc_id = parse_fs_probe_doc_id(&result.doc_id)?;
    let metadata = metadata_by_doc_key
        .get(&result.doc_id)
        .copied()
        .unwrap_or(ProbeDocMetadata {
            idx: 0,
            doc_kind: crate::document::DocKind::Message,
            project_id: None,
        });
    Some(ScoredResult {
        idx: metadata.idx,
        doc_id,
        doc_kind: metadata.doc_kind,
        project_id: metadata.project_id,
        score: result.score,
    })
}

fn from_fs_scored_results_with_metadata(
    results: &[crate::fs_bridge::FsScoredResult],
    metadata_by_doc_key: &HashMap<String, ProbeDocMetadata>,
) -> Vec<ScoredResult> {
    results
        .iter()
        .filter_map(|result| from_fs_scored_result_with_metadata(result, metadata_by_doc_key))
        .collect()
}

fn from_fs_phase(
    phase: crate::fs_bridge::FsSearchPhase,
    metadata_by_doc_key: &HashMap<String, ProbeDocMetadata>,
) -> SearchPhase {
    match phase {
        crate::fs_bridge::FsSearchPhase::Initial {
            results, latency, ..
        } => SearchPhase::Initial {
            results: from_fs_scored_results_with_metadata(&results, metadata_by_doc_key),
            latency_ms: duration_millis_u64(latency),
        },
        crate::fs_bridge::FsSearchPhase::Refined {
            results, latency, ..
        } => SearchPhase::Refined {
            results: from_fs_scored_results_with_metadata(&results, metadata_by_doc_key),
            latency_ms: duration_millis_u64(latency),
        },
        crate::fs_bridge::FsSearchPhase::RefinementFailed { error, .. } => {
            SearchPhase::RefinementFailed {
                error: error.to_string(),
            }
        }
    }
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

    fn build_frankensearch_probe_index(
        &self,
        probe_dir: &Path,
    ) -> SearchResult<(
        crate::fs_bridge::FsTwoTierIndex,
        HashMap<String, ProbeDocMetadata>,
    )> {
        let mut builder = crate::fs_bridge::FsTwoTierIndex::create(
            probe_dir,
            crate::fs_bridge::to_fs_config(&self.config),
        )
        .map_err(crate::fs_bridge::map_fs_error)?;

        builder.set_fast_embedder_id(self.fast_embedder.id());
        if let Some(quality_embedder) = &self.quality_embedder {
            builder.set_quality_embedder_id(quality_embedder.id());
        }

        let mut metadata_by_doc_key: HashMap<String, ProbeDocMetadata> =
            HashMap::with_capacity(self.index.doc_ids.len());

        for idx in 0..self.index.metadata.doc_count {
            let doc_id = self.index.doc_ids.get(idx).copied().ok_or_else(|| {
                SearchError::Internal(format!("missing doc_id for two-tier index position {idx}"))
            })?;
            let fast_embedding = self.index.fast_embedding(idx).ok_or_else(|| {
                SearchError::Internal(format!(
                    "missing fast embedding for two-tier index position {idx}"
                ))
            })?;

            let quality_embedding = if self.index.has_quality(idx) {
                self.index.quality_embedding(idx).map(f16_slice_to_f32_vec)
            } else {
                None
            };

            let doc_key = to_fs_probe_doc_key(doc_id, idx);
            let fast_embedding = f16_slice_to_f32_vec(fast_embedding);
            builder
                .add_record(
                    doc_key.clone(),
                    &fast_embedding,
                    quality_embedding.as_deref(),
                )
                .map_err(crate::fs_bridge::map_fs_error)?;

            metadata_by_doc_key.insert(
                doc_key,
                ProbeDocMetadata {
                    idx,
                    doc_kind: self
                        .index
                        .doc_kinds
                        .get(idx)
                        .copied()
                        .unwrap_or(crate::document::DocKind::Message),
                    project_id: self.index.project_ids.get(idx).copied().flatten(),
                },
            );
        }

        let fs_index = builder.finish().map_err(crate::fs_bridge::map_fs_error)?;
        Ok((fs_index, metadata_by_doc_key))
    }

    /// Probe seam: execute search through `frankensearch::TwoTierSearcher`
    /// while preserving the local `SearchPhase` contract.
    ///
    /// This path exists for migration verification and should not replace
    /// the default synchronous iterator without parity evidence.
    ///
    /// # Errors
    ///
    /// Maps frankensearch failures via [`crate::fs_bridge::map_fs_error`].
    pub async fn search_with_frankensearch_probe(
        &self,
        cx: &crate::fs_bridge::FsCx,
        query: &str,
        k: usize,
    ) -> SearchResult<Vec<SearchPhase>> {
        if query.is_empty() || k == 0 || self.index.is_empty() {
            return Ok(Vec::new());
        }

        // Preserve local quality-only semantics: return only refined output.
        if self.config.quality_only {
            let start = Instant::now();
            return match self.search_quality_only(query, k) {
                Ok(results) => Ok(vec![SearchPhase::Refined {
                    results,
                    latency_ms: duration_millis_u64(start.elapsed()),
                }]),
                Err(err) => Ok(vec![SearchPhase::RefinementFailed {
                    error: err.to_string(),
                }]),
            };
        }

        let probe_dir = fs_two_tier_probe_dir();
        std::fs::create_dir_all(&probe_dir).map_err(|error| {
            SearchError::Internal(format!(
                "failed to create frankensearch probe dir {}: {error}",
                probe_dir.display()
            ))
        })?;

        let result = async {
            let (fs_index, metadata_by_doc_key) =
                self.build_frankensearch_probe_index(&probe_dir)?;
            let metadata_by_doc_key = Arc::new(metadata_by_doc_key);

            let mut fs_searcher = crate::fs_bridge::FsTwoTierSearcher::new(
                Arc::new(fs_index),
                Arc::new(crate::fs_bridge::SyncEmbedderAdapter::fast(
                    self.fast_embedder.clone(),
                )),
                crate::fs_bridge::to_fs_config(&self.config),
            );
            if let Some(quality_embedder) = self.quality_embedder.clone() {
                fs_searcher = fs_searcher.with_quality_embedder(Arc::new(
                    crate::fs_bridge::SyncEmbedderAdapter::quality(quality_embedder),
                ));
            }

            let mut phases = Vec::new();
            let phase_metadata = Arc::clone(&metadata_by_doc_key);
            fs_searcher
                .search(
                    cx,
                    query,
                    k,
                    |_| None,
                    |phase| {
                        phases.push(from_fs_phase(phase, phase_metadata.as_ref()));
                    },
                )
                .await
                .map_err(crate::fs_bridge::map_fs_error)?;
            Ok(phases)
        }
        .await;

        if let Err(error) = std::fs::remove_dir_all(&probe_dir) {
            tracing::debug!(
                target: "search.semantic",
                path = %probe_dir.display(),
                error = %error,
                "failed to remove two-tier probe directory"
            );
        }

        result
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

    fn build_refined_results(&self, query_vec: &[f32]) -> Vec<ScoredResult> {
        let Some(fast_results) = self.fast_results.as_ref() else {
            // If no fast candidates are available, fall back to full quality search.
            return self.searcher.index.search_quality(query_vec, self.k);
        };

        if fast_results.is_empty() {
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
            return passthrough;
        }

        let candidates: Vec<usize> = fast_results
            .iter()
            .take(refinement_limit)
            .map(|sr| sr.idx)
            .collect();

        let quality_scores = self
            .searcher
            .index
            .quality_scores_for_indices(query_vec, &candidates);

        // Blend scores, but only for docs with quality embeddings.
        // Docs without quality use fast score only.
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

        // Leave documents outside the refinement budget untouched.
        blended.extend(fast_results.iter().skip(refinement_limit).cloned());

        // Re-sort by blended score.
        blended.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        blended.truncate(self.k);
        blended
    }

    fn run_refinement_phase(&self) -> SearchPhase {
        let Some(quality_embedder) = &self.searcher.quality_embedder else {
            return SearchPhase::RefinementFailed {
                error: "quality embedder unavailable".to_string(),
            };
        };

        let start = Instant::now();

        match quality_embedder.embed(&self.query) {
            Ok(query_vec) => {
                let results = self.build_refined_results(&query_vec);
                #[allow(clippy::cast_possible_truncation)]
                let latency_ms = start.elapsed().as_millis() as u64;
                SearchPhase::Refined {
                    results,
                    latency_ms,
                }
            }
            Err(e) => SearchPhase::RefinementFailed {
                error: e.to_string(),
            },
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
                            return Some(SearchPhase::Initial {
                                results,
                                latency_ms,
                            });
                        }

                        // In quality-only mode, do not emit initial results.
                        if self.searcher.config.quality_only {
                            self.phase = 2;
                            return Some(self.run_refinement_phase());
                        }

                        Some(SearchPhase::Initial {
                            results,
                            latency_ms,
                        })
                    }
                    Err(e) => {
                        warn!(error = %e, "Fast embedding failed");

                        if self.searcher.config.quality_only {
                            self.phase = 2;
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

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
    fn fs_probe_doc_key_roundtrip() {
        let key = to_fs_probe_doc_key(42, 7);
        assert_eq!(key, "42::7");
        assert_eq!(parse_fs_probe_doc_id(&key), Some(42));
        assert_eq!(parse_fs_probe_doc_id("99"), Some(99));
        assert_eq!(parse_fs_probe_doc_id("bad::key"), None);
    }

    #[test]
    fn from_fs_phase_initial_preserves_domain_metadata() {
        use frankensearch::core::types::ScoreSource;

        let mut metadata_by_doc_key = HashMap::new();
        metadata_by_doc_key.insert(
            "10::3".to_string(),
            ProbeDocMetadata {
                idx: 3,
                doc_kind: crate::document::DocKind::Thread,
                project_id: Some(77),
            },
        );

        let phase = crate::fs_bridge::FsSearchPhase::Initial {
            results: vec![crate::fs_bridge::FsScoredResult {
                doc_id: "10::3".to_string(),
                score: 0.91,
                source: ScoreSource::SemanticFast,
                fast_score: Some(0.91),
                quality_score: None,
                lexical_score: None,
                rerank_score: None,
                metadata: None,
            }],
            latency: std::time::Duration::from_millis(9),
            metrics: frankensearch::core::types::PhaseMetrics {
                embedder_id: "probe".to_string(),
                vectors_searched: 1,
                lexical_candidates: 0,
                fused_count: 1,
            },
        };

        match from_fs_phase(phase, &metadata_by_doc_key) {
            SearchPhase::Initial {
                results,
                latency_ms,
            } => {
                assert_eq!(latency_ms, 9);
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].doc_id, 10);
                assert_eq!(results[0].idx, 3);
                assert_eq!(results[0].doc_kind, crate::document::DocKind::Thread);
                assert_eq!(results[0].project_id, Some(77));
            }
            other => panic!("expected initial phase, got {other:?}"),
        }
    }

    #[test]
    fn from_fs_phase_refinement_failed_maps_error_string() {
        let phase = crate::fs_bridge::FsSearchPhase::RefinementFailed {
            initial_results: Vec::new(),
            error: frankensearch::SearchError::ModelNotFound {
                name: "quality-model".to_string(),
            },
            latency: std::time::Duration::from_millis(11),
        };

        match from_fs_phase(phase, &HashMap::new()) {
            SearchPhase::RefinementFailed { error } => {
                assert!(error.contains("quality-model"));
            }
            other => panic!("expected refinement failed phase, got {other:?}"),
        }
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
        index.metadata.doc_count = 1;

        // Before migration: incorrectly marked
        assert!(index.has_quality_flags[0]);

        // Run migration
        let migrated = index.migrate_zero_quality_to_no_quality();
        assert_eq!(migrated, 1);

        // After migration: correctly marked
        assert!(!index.has_quality(0));
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
}
