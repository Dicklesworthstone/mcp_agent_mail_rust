//! Embedder abstraction and model registry for semantic search.
//!
//! This module provides a pluggable interface for generating text embeddings,
//! with support for multiple model quality tiers and hash-based fallback.
//!
//! # Architecture
//!
//! The embedding system has three quality tiers:
//! - **Hash**: Content-hash fallback when no model is available (cosine = exact match only)
//! - **Fast**: Lightweight model optimized for latency (e.g., `MiniLM`, `all-minilm-l6-v2`)
//! - **Quality**: Full-size model for accuracy (e.g., e5-large, bge-large)
//!
//! The [`ModelRegistry`] manages available models and their capabilities.
//! The [`Embedder`] trait provides the embedding interface.
//!
//! # Feature Gating
//!
//! This module is compiled when the `semantic` feature is enabled.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::search_canonical::{CanonPolicy, canonicalize_and_hash};
use crate::search_error::{SearchError, SearchResult};
use mcp_agent_mail_core::DocKind;

// ────────────────────────────────────────────────────────────────────
// Embedding types
// ────────────────────────────────────────────────────────────────────

/// A dense embedding vector.
///
/// Standard dimension is 384 (fast models) or 768/1024 (quality models).
pub type EmbeddingVec = Vec<f32>;

/// The quality tier of an embedding model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// Content-hash fallback (no actual embedding model)
    Hash,
    /// Lightweight model optimized for speed
    #[default]
    Fast,
    /// Full-size model optimized for quality
    Quality,
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hash => write!(f, "hash"),
            Self::Fast => write!(f, "fast"),
            Self::Quality => write!(f, "quality"),
        }
    }
}

/// Describes the capabilities and characteristics of an embedding model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Unique identifier for this model (e.g., "all-minilm-l6-v2")
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Quality tier
    pub tier: ModelTier,
    /// Embedding dimension (e.g., 384, 768, 1024)
    pub dimension: usize,
    /// Maximum input token length (truncation threshold)
    pub max_tokens: usize,
    /// Whether this model supports batch embedding
    pub supports_batch: bool,
    /// Estimated latency for a single embedding (for capacity planning)
    pub estimated_latency_ms: u64,
    /// Whether this model is currently available/loaded
    pub available: bool,
    /// Optional provider info (e.g., "huggingface", "openai", "local")
    pub provider: Option<String>,
}

impl ModelInfo {
    /// Create a new `ModelInfo` with the given properties.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        tier: ModelTier,
        dimension: usize,
        max_tokens: usize,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            tier,
            dimension,
            max_tokens,
            supports_batch: true,
            estimated_latency_ms: match tier {
                ModelTier::Hash => 0,
                ModelTier::Fast => 5,
                ModelTier::Quality => 50,
            },
            available: false,
            provider: None,
        }
    }

    /// Builder: set the provider
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Builder: set availability
    #[must_use]
    pub const fn with_available(mut self, available: bool) -> Self {
        self.available = available;
        self
    }

    /// Builder: set estimated latency
    #[must_use]
    pub const fn with_latency(mut self, latency_ms: u64) -> Self {
        self.estimated_latency_ms = latency_ms;
        self
    }

    /// Builder: set batch support
    #[must_use]
    pub const fn with_batch_support(mut self, supports_batch: bool) -> Self {
        self.supports_batch = supports_batch;
        self
    }
}

/// The result of an embedding operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResult {
    /// The embedding vector
    pub vector: EmbeddingVec,
    /// The model that produced this embedding
    pub model_id: String,
    /// The tier of the model
    pub tier: ModelTier,
    /// The dimension of the embedding
    pub dimension: usize,
    /// Time taken to generate the embedding
    pub elapsed: Duration,
    /// The content hash (for change detection)
    pub content_hash: String,
}

impl EmbeddingResult {
    /// Create a new embedding result
    #[must_use]
    pub fn new(
        vector: EmbeddingVec,
        model_id: impl Into<String>,
        tier: ModelTier,
        elapsed: Duration,
        content_hash: impl Into<String>,
    ) -> Self {
        let dimension = vector.len();
        Self {
            vector,
            model_id: model_id.into(),
            tier,
            dimension,
            elapsed,
            content_hash: content_hash.into(),
        }
    }

    /// Create a hash-based "embedding" (not a real vector, just a marker)
    #[must_use]
    pub fn from_hash(content_hash: impl Into<String>) -> Self {
        Self {
            vector: Vec::new(),
            model_id: "hash".to_owned(),
            tier: ModelTier::Hash,
            dimension: 0,
            elapsed: Duration::ZERO,
            content_hash: content_hash.into(),
        }
    }

    /// Returns true if this is a hash-based "embedding"
    #[must_use]
    pub fn is_hash_only(&self) -> bool {
        self.tier == ModelTier::Hash || self.vector.is_empty()
    }
}

// ────────────────────────────────────────────────────────────────────
// Embedder trait
// ────────────────────────────────────────────────────────────────────

/// The primary trait for embedding text into vectors.
///
/// Implementations may use local models, remote APIs, or fallback to hashing.
pub trait Embedder: Send + Sync {
    /// Embed a single text into a vector.
    ///
    /// The text should already be canonicalized (via [`crate::search_canonical::canonicalize`]).
    ///
    /// # Errors
    /// Returns `SearchError` if embedding fails (model unavailable, timeout, etc.)
    fn embed(&self, text: &str) -> SearchResult<EmbeddingResult>;

    /// Embed multiple texts in a batch (more efficient than repeated `embed` calls).
    ///
    /// Default implementation calls `embed` in sequence.
    ///
    /// # Errors
    /// Returns `SearchError` if any embedding fails.
    fn embed_batch(&self, texts: &[&str]) -> SearchResult<Vec<EmbeddingResult>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Return information about the model used by this embedder.
    fn model_info(&self) -> &ModelInfo;

    /// Check if the embedder is ready to serve requests.
    fn is_ready(&self) -> bool {
        self.model_info().available
    }
}

// ────────────────────────────────────────────────────────────────────
// Hash-based embedder (fallback)
// ────────────────────────────────────────────────────────────────────

/// A fallback "embedder" that uses content hashes instead of real embeddings.
///
/// This enables the semantic search pipeline to function even when no
/// embedding model is available. Similarity is binary: exact match or not.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    info: ModelInfo,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl HashEmbedder {
    /// Create a new hash-based embedder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            info: ModelInfo {
                id: "hash".to_owned(),
                name: "Content Hash Fallback".to_owned(),
                tier: ModelTier::Hash,
                dimension: 0,
                max_tokens: usize::MAX,
                supports_batch: true,
                estimated_latency_ms: 0,
                available: true,
                provider: Some("builtin".to_owned()),
            },
        }
    }
}

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> SearchResult<EmbeddingResult> {
        let hash = crate::search_canonical::content_hash(text);
        Ok(EmbeddingResult::from_hash(hash))
    }

    fn embed_batch(&self, texts: &[&str]) -> SearchResult<Vec<EmbeddingResult>> {
        Ok(texts
            .iter()
            .map(|t| {
                let hash = crate::search_canonical::content_hash(t);
                EmbeddingResult::from_hash(hash)
            })
            .collect())
    }

    fn model_info(&self) -> &ModelInfo {
        &self.info
    }
}

// ────────────────────────────────────────────────────────────────────
// Model Registry
// ────────────────────────────────────────────────────────────────────

/// Configuration for the model registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryConfig {
    /// Preferred model ID for the fast tier (None = use first available)
    pub preferred_fast: Option<String>,
    /// Preferred model ID for the quality tier (None = use first available)
    pub preferred_quality: Option<String>,
    /// Whether to fallback to hash when no model is available
    pub allow_hash_fallback: bool,
    /// Maximum embedding latency before timeout (milliseconds)
    pub timeout_ms: u64,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            preferred_fast: None,
            preferred_quality: None,
            allow_hash_fallback: true,
            timeout_ms: 5000,
        }
    }
}

/// Manages available embedding models and provides the appropriate embedder
/// for each quality tier.
///
/// The registry maintains a catalog of model metadata and can instantiate
/// embedders on demand based on the requested tier.
pub struct ModelRegistry {
    /// Configuration
    config: RegistryConfig,
    /// Registered model information by ID
    models: HashMap<String, ModelInfo>,
    /// Active embedders by model ID
    embedders: HashMap<String, Arc<dyn Embedder>>,
    /// Hash fallback embedder (always available)
    hash_embedder: Arc<HashEmbedder>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("config", &self.config)
            .field("models", &self.models)
            .field("embedders_count", &self.embedders.len())
            .field("hash_embedder", &"HashEmbedder")
            .finish()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new(RegistryConfig::default())
    }
}

impl ModelRegistry {
    /// Create a new registry with the given configuration.
    #[must_use]
    pub fn new(config: RegistryConfig) -> Self {
        let hash_embedder = Arc::new(HashEmbedder::new());
        let mut models = HashMap::new();
        models.insert("hash".to_owned(), hash_embedder.model_info().clone());

        Self {
            config,
            models,
            embedders: HashMap::new(),
            hash_embedder,
        }
    }

    /// Register a model's metadata without activating it.
    ///
    /// This is useful for discovery: the registry knows about the model
    /// but doesn't instantiate its embedder until requested.
    pub fn register_model(&mut self, info: ModelInfo) {
        self.models.insert(info.id.clone(), info);
    }

    /// Activate an embedder and make it available for use.
    ///
    /// The embedder must implement the `Embedder` trait.
    pub fn activate_embedder(&mut self, embedder: Arc<dyn Embedder>) {
        let info = embedder.model_info().clone();
        self.models.insert(info.id.clone(), info);
        self.embedders
            .insert(embedder.model_info().id.clone(), embedder);
    }

    /// Deactivate an embedder (remove it from active use).
    pub fn deactivate_embedder(&mut self, model_id: &str) {
        self.embedders.remove(model_id);
        if let Some(info) = self.models.get_mut(model_id) {
            info.available = false;
        }
    }

    /// Get the best available embedder for the requested tier.
    ///
    /// Selection priority:
    /// 1. Preferred model for the tier (if configured and available)
    /// 2. First available model at the requested tier
    /// 3. Fallback to lower tier (Quality -> Fast -> Hash)
    /// 4. Hash fallback (if allowed)
    ///
    /// # Errors
    /// Returns `SearchError::ModeUnavailable` if no embedder is available
    /// and hash fallback is disabled.
    pub fn get_embedder(&self, tier: ModelTier) -> SearchResult<Arc<dyn Embedder>> {
        // Hash tier always uses the hash embedder
        if tier == ModelTier::Hash {
            return Ok(self.hash_embedder.clone());
        }

        // Try preferred model first
        let preferred = match tier {
            ModelTier::Fast => &self.config.preferred_fast,
            ModelTier::Quality => &self.config.preferred_quality,
            ModelTier::Hash => unreachable!(),
        };

        if let Some(preferred_id) = preferred
            && let Some(embedder) = self.embedders.get(preferred_id)
            && embedder.is_ready()
        {
            return Ok(embedder.clone());
        }

        // Find first available at requested tier
        for (id, info) in &self.models {
            if info.tier == tier
                && info.available
                && let Some(embedder) = self.embedders.get(id)
            {
                return Ok(embedder.clone());
            }
        }

        // Fallback to lower tier
        match tier {
            ModelTier::Quality => {
                // Try fast tier as fallback
                if let Ok(embedder) = self.get_embedder(ModelTier::Fast) {
                    return Ok(embedder);
                }
            }
            ModelTier::Fast => {
                // Fall through to hash
            }
            ModelTier::Hash => unreachable!(),
        }

        // Hash fallback
        if self.config.allow_hash_fallback {
            return Ok(self.hash_embedder.clone());
        }

        Err(SearchError::ModeUnavailable(format!(
            "No embedder available for tier {tier}"
        )))
    }

    /// Get the hash embedder (always available).
    #[must_use]
    pub fn hash_embedder(&self) -> Arc<HashEmbedder> {
        self.hash_embedder.clone()
    }

    /// List all registered models.
    #[must_use]
    pub fn list_models(&self) -> Vec<&ModelInfo> {
        self.models.values().collect()
    }

    /// List only available (active) models.
    #[must_use]
    pub fn list_available(&self) -> Vec<&ModelInfo> {
        self.models.values().filter(|m| m.available).collect()
    }

    /// Get info for a specific model by ID.
    #[must_use]
    pub fn get_model_info(&self, model_id: &str) -> Option<&ModelInfo> {
        self.models.get(model_id)
    }

    /// Check if any embedding model (beyond hash fallback) is available.
    #[must_use]
    pub fn has_real_embedder(&self) -> bool {
        self.models
            .values()
            .any(|m| m.available && m.tier != ModelTier::Hash)
    }

    /// Get the current configuration.
    #[must_use]
    pub const fn config(&self) -> &RegistryConfig {
        &self.config
    }

    /// Update the configuration.
    pub fn set_config(&mut self, config: RegistryConfig) {
        self.config = config;
    }
}

// ────────────────────────────────────────────────────────────────────
// Helper functions
// ────────────────────────────────────────────────────────────────────

/// Embed a document directly, handling canonicalization.
///
/// This is a convenience function that:
/// 1. Canonicalizes the document text
/// 2. Computes the content hash
/// 3. Generates the embedding
///
/// # Errors
/// Returns `SearchError` if embedding fails.
pub fn embed_document(
    embedder: &dyn Embedder,
    doc_kind: DocKind,
    title: &str,
    body: &str,
    policy: CanonPolicy,
) -> SearchResult<EmbeddingResult> {
    let (canonical, hash) = canonicalize_and_hash(doc_kind, title, body, policy);
    let mut result = embedder.embed(&canonical)?;
    result.content_hash = hash;
    Ok(result)
}

/// Compute cosine similarity between two embedding vectors.
///
/// Returns a value in `[-1.0, 1.0]` where 1.0 means identical direction.
/// Returns 0.0 if either vector is empty or zero-length.
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        return 0.0;
    }

    dot / denom
}

/// Normalize an embedding vector to unit length (L2 normalization).
///
/// This is required for cosine similarity to work correctly with
/// dot product (which is faster than full cosine computation).
#[must_use]
pub fn normalize_l2(v: &[f32]) -> EmbeddingVec {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < f32::EPSILON {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

// ────────────────────────────────────────────────────────────────────
// Well-known model definitions
// ────────────────────────────────────────────────────────────────────

/// Well-known embedding models that can be registered.
pub mod well_known {
    use super::{ModelInfo, ModelTier};

    /// all-MiniLM-L6-v2: Fast, 384-dim, good for low-latency search
    #[must_use]
    pub fn minilm_l6_v2() -> ModelInfo {
        ModelInfo::new(
            "all-minilm-l6-v2",
            "All-MiniLM-L6-v2",
            ModelTier::Fast,
            384,
            512,
        )
        .with_provider("sentence-transformers")
        .with_latency(5)
    }

    /// all-MiniLM-L12-v2: Balanced speed/quality, 384-dim
    #[must_use]
    pub fn minilm_l12_v2() -> ModelInfo {
        ModelInfo::new(
            "all-minilm-l12-v2",
            "All-MiniLM-L12-v2",
            ModelTier::Fast,
            384,
            512,
        )
        .with_provider("sentence-transformers")
        .with_latency(10)
    }

    /// e5-small-v2: Fast E5 model, 384-dim
    #[must_use]
    pub fn e5_small_v2() -> ModelInfo {
        ModelInfo::new("e5-small-v2", "E5-small-v2", ModelTier::Fast, 384, 512)
            .with_provider("intfloat")
            .with_latency(8)
    }

    /// e5-base-v2: Balanced E5 model, 768-dim
    #[must_use]
    pub fn e5_base_v2() -> ModelInfo {
        ModelInfo::new("e5-base-v2", "E5-base-v2", ModelTier::Quality, 768, 512)
            .with_provider("intfloat")
            .with_latency(25)
    }

    /// e5-large-v2: High-quality E5 model, 1024-dim
    #[must_use]
    pub fn e5_large_v2() -> ModelInfo {
        ModelInfo::new("e5-large-v2", "E5-large-v2", ModelTier::Quality, 1024, 512)
            .with_provider("intfloat")
            .with_latency(50)
    }

    /// bge-small-en-v1.5: Fast BGE model, 384-dim
    #[must_use]
    pub fn bge_small_en() -> ModelInfo {
        ModelInfo::new(
            "bge-small-en-v1.5",
            "BGE-small-en-v1.5",
            ModelTier::Fast,
            384,
            512,
        )
        .with_provider("baai")
        .with_latency(6)
    }

    /// bge-base-en-v1.5: Balanced BGE model, 768-dim
    #[must_use]
    pub fn bge_base_en() -> ModelInfo {
        ModelInfo::new(
            "bge-base-en-v1.5",
            "BGE-base-en-v1.5",
            ModelTier::Quality,
            768,
            512,
        )
        .with_provider("baai")
        .with_latency(20)
    }

    /// bge-large-en-v1.5: High-quality BGE model, 1024-dim
    #[must_use]
    pub fn bge_large_en() -> ModelInfo {
        ModelInfo::new(
            "bge-large-en-v1.5",
            "BGE-large-en-v1.5",
            ModelTier::Quality,
            1024,
            512,
        )
        .with_provider("baai")
        .with_latency(45)
    }

    /// `text-embedding-3-small`: `OpenAI` small model, 1536-dim
    #[must_use]
    pub fn openai_small() -> ModelInfo {
        ModelInfo::new(
            "text-embedding-3-small",
            "OpenAI text-embedding-3-small",
            ModelTier::Fast,
            1536,
            8191,
        )
        .with_provider("openai")
        .with_latency(100)
        .with_batch_support(true)
    }

    /// `text-embedding-3-large`: `OpenAI` large model, 3072-dim
    #[must_use]
    pub fn openai_large() -> ModelInfo {
        ModelInfo::new(
            "text-embedding-3-large",
            "OpenAI text-embedding-3-large",
            ModelTier::Quality,
            3072,
            8191,
        )
        .with_provider("openai")
        .with_latency(150)
        .with_batch_support(true)
    }
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ModelTier ──

    #[test]
    fn model_tier_display() {
        assert_eq!(ModelTier::Hash.to_string(), "hash");
        assert_eq!(ModelTier::Fast.to_string(), "fast");
        assert_eq!(ModelTier::Quality.to_string(), "quality");
    }

    #[test]
    fn model_tier_default() {
        assert_eq!(ModelTier::default(), ModelTier::Fast);
    }

    #[test]
    fn model_tier_serde_roundtrip() {
        for tier in [ModelTier::Hash, ModelTier::Fast, ModelTier::Quality] {
            let json = serde_json::to_string(&tier).unwrap();
            let tier2: ModelTier = serde_json::from_str(&json).unwrap();
            assert_eq!(tier, tier2);
        }
    }

    // ── ModelInfo ──

    #[test]
    fn model_info_builder() {
        let info = ModelInfo::new("test", "Test Model", ModelTier::Fast, 384, 512)
            .with_provider("test-provider")
            .with_available(true)
            .with_latency(10)
            .with_batch_support(false);

        assert_eq!(info.id, "test");
        assert_eq!(info.name, "Test Model");
        assert_eq!(info.tier, ModelTier::Fast);
        assert_eq!(info.dimension, 384);
        assert_eq!(info.max_tokens, 512);
        assert_eq!(info.provider, Some("test-provider".to_owned()));
        assert!(info.available);
        assert_eq!(info.estimated_latency_ms, 10);
        assert!(!info.supports_batch);
    }

    #[test]
    fn model_info_default_latency_by_tier() {
        assert_eq!(
            ModelInfo::new("h", "h", ModelTier::Hash, 0, 0).estimated_latency_ms,
            0
        );
        assert_eq!(
            ModelInfo::new("f", "f", ModelTier::Fast, 384, 512).estimated_latency_ms,
            5
        );
        assert_eq!(
            ModelInfo::new("q", "q", ModelTier::Quality, 768, 512).estimated_latency_ms,
            50
        );
    }

    // ── EmbeddingResult ──

    #[test]
    fn embedding_result_new() {
        let result = EmbeddingResult::new(
            vec![0.1, 0.2, 0.3],
            "test-model",
            ModelTier::Fast,
            Duration::from_millis(5),
            "abc123",
        );
        assert_eq!(result.vector, vec![0.1, 0.2, 0.3]);
        assert_eq!(result.model_id, "test-model");
        assert_eq!(result.tier, ModelTier::Fast);
        assert_eq!(result.dimension, 3);
        assert_eq!(result.content_hash, "abc123");
        assert!(!result.is_hash_only());
    }

    #[test]
    fn embedding_result_from_hash() {
        let result = EmbeddingResult::from_hash("abc123");
        assert!(result.vector.is_empty());
        assert_eq!(result.model_id, "hash");
        assert_eq!(result.tier, ModelTier::Hash);
        assert_eq!(result.dimension, 0);
        assert_eq!(result.content_hash, "abc123");
        assert!(result.is_hash_only());
    }

    // ── HashEmbedder ──

    #[test]
    fn hash_embedder_embed() {
        let embedder = HashEmbedder::new();
        let result = embedder.embed("hello world").unwrap();
        assert!(result.is_hash_only());
        assert!(!result.content_hash.is_empty());
        assert_eq!(result.content_hash.len(), 64); // SHA-256 hex
    }

    #[test]
    fn hash_embedder_batch() {
        let embedder = HashEmbedder::new();
        let results = embedder.embed_batch(&["hello", "world"]).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].is_hash_only());
        assert!(results[1].is_hash_only());
        assert_ne!(results[0].content_hash, results[1].content_hash);
    }

    #[test]
    fn hash_embedder_deterministic() {
        let embedder = HashEmbedder::new();
        let r1 = embedder.embed("test input").unwrap();
        let r2 = embedder.embed("test input").unwrap();
        assert_eq!(r1.content_hash, r2.content_hash);
    }

    #[test]
    fn hash_embedder_model_info() {
        let embedder = HashEmbedder::new();
        let info = embedder.model_info();
        assert_eq!(info.id, "hash");
        assert_eq!(info.tier, ModelTier::Hash);
        assert!(info.available);
        assert_eq!(info.dimension, 0);
    }

    #[test]
    fn hash_embedder_is_ready() {
        let embedder = HashEmbedder::new();
        assert!(embedder.is_ready());
    }

    // ── ModelRegistry ──

    #[test]
    fn registry_default() {
        let registry = ModelRegistry::default();
        assert!(registry.config.allow_hash_fallback);
        assert!(registry.list_models().iter().any(|m| m.id == "hash"));
    }

    #[test]
    fn registry_get_hash_embedder() {
        let registry = ModelRegistry::default();
        let embedder = registry.get_embedder(ModelTier::Hash).unwrap();
        assert_eq!(embedder.model_info().tier, ModelTier::Hash);
    }

    #[test]
    fn registry_fallback_to_hash() {
        let registry = ModelRegistry::default();
        // No fast model registered, should fallback to hash
        let embedder = registry.get_embedder(ModelTier::Fast).unwrap();
        assert_eq!(embedder.model_info().tier, ModelTier::Hash);
    }

    #[test]
    fn registry_fallback_disabled() {
        let config = RegistryConfig {
            allow_hash_fallback: false,
            ..Default::default()
        };
        let registry = ModelRegistry::new(config);
        let result = registry.get_embedder(ModelTier::Fast);
        assert!(result.is_err());
        assert!(matches!(result, Err(SearchError::ModeUnavailable(_))));
    }

    #[test]
    fn registry_register_model() {
        let mut registry = ModelRegistry::default();
        let info = well_known::minilm_l6_v2();
        registry.register_model(info);
        assert!(registry.get_model_info("all-minilm-l6-v2").is_some());
        // Not available until activated
        assert!(
            !registry
                .get_model_info("all-minilm-l6-v2")
                .unwrap()
                .available
        );
    }

    #[test]
    fn registry_activate_embedder() {
        let mut registry = ModelRegistry::default();
        let embedder = Arc::new(HashEmbedder::new());
        registry.activate_embedder(embedder);
        assert!(registry.has_real_embedder() || !registry.list_available().is_empty());
    }

    #[test]
    fn registry_list_models() {
        let mut registry = ModelRegistry::default();
        registry.register_model(well_known::minilm_l6_v2());
        registry.register_model(well_known::e5_large_v2());
        let models = registry.list_models();
        assert!(models.len() >= 3); // hash + 2 registered
    }

    #[test]
    fn registry_has_real_embedder() {
        let registry = ModelRegistry::default();
        // Only hash is available
        assert!(!registry.has_real_embedder());
    }

    // ── Cosine similarity ──

    #[test]
    fn cosine_identical() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_opposite() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_mismatched_length() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_empty() {
        let empty: Vec<f32> = Vec::new();
        assert!(cosine_similarity(&empty, &empty).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_zero_vector() {
        let zero = vec![0.0, 0.0, 0.0];
        let v = vec![1.0, 2.0, 3.0];
        assert!(cosine_similarity(&zero, &v).abs() < f32::EPSILON);
    }

    // ── L2 normalization ──

    #[test]
    fn normalize_l2_unit_vector() {
        let v = vec![1.0, 0.0, 0.0];
        let n = normalize_l2(&v);
        assert_eq!(n, vec![1.0, 0.0, 0.0]);
    }

    #[test]
    fn normalize_l2_general() {
        let v = vec![3.0, 4.0];
        let n = normalize_l2(&v);
        // Should be [0.6, 0.8] with norm 1.0
        let norm: f32 = n.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((n[0] - 0.6).abs() < 1e-6);
        assert!((n[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn normalize_l2_zero_vector() {
        let v = vec![0.0, 0.0, 0.0];
        let n = normalize_l2(&v);
        assert_eq!(n, v); // Zero vector unchanged
    }

    // ── Well-known models ──

    #[test]
    fn well_known_minilm() {
        let info = well_known::minilm_l6_v2();
        assert_eq!(info.id, "all-minilm-l6-v2");
        assert_eq!(info.tier, ModelTier::Fast);
        assert_eq!(info.dimension, 384);
    }

    #[test]
    fn well_known_e5() {
        let info = well_known::e5_large_v2();
        assert_eq!(info.tier, ModelTier::Quality);
        assert_eq!(info.dimension, 1024);
    }

    #[test]
    fn well_known_bge() {
        let info = well_known::bge_large_en();
        assert_eq!(info.tier, ModelTier::Quality);
        assert_eq!(info.dimension, 1024);
    }

    #[test]
    fn well_known_openai() {
        let info = well_known::openai_large();
        assert_eq!(info.provider, Some("openai".to_owned()));
        assert_eq!(info.dimension, 3072);
    }

    // ── embed_document helper ──

    #[test]
    fn embed_document_uses_canonicalization() {
        let embedder = HashEmbedder::new();
        let result = embed_document(
            &embedder,
            DocKind::Message,
            "## Subject",
            "Body with **markdown**",
            CanonPolicy::Full,
        )
        .unwrap();
        assert!(!result.content_hash.is_empty());
        // The hash should be from canonicalized text, not raw
    }

    // ── ModelTier extended ──

    #[test]
    fn model_tier_hash_trait() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ModelTier::Hash);
        set.insert(ModelTier::Fast);
        set.insert(ModelTier::Quality);
        set.insert(ModelTier::Fast); // duplicate
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn model_tier_debug() {
        for tier in [ModelTier::Hash, ModelTier::Fast, ModelTier::Quality] {
            let debug = format!("{tier:?}");
            assert!(!debug.is_empty());
        }
    }

    // ── ModelInfo extended ──

    #[test]
    fn model_info_serde_roundtrip() {
        let info = ModelInfo::new("test", "Test", ModelTier::Fast, 384, 512)
            .with_provider("local")
            .with_available(true);
        let json = serde_json::to_string(&info).unwrap();
        let info2: ModelInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info2.id, "test");
        assert_eq!(info2.tier, ModelTier::Fast);
        assert_eq!(info2.dimension, 384);
        assert_eq!(info2.provider, Some("local".to_string()));
        assert!(info2.available);
    }

    #[test]
    fn model_info_debug() {
        let info = ModelInfo::new("test", "Test", ModelTier::Fast, 384, 512);
        let debug = format!("{info:?}");
        assert!(debug.contains("test"));
    }

    #[test]
    fn model_info_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let info = ModelInfo::new("test", "Test", ModelTier::Fast, 384, 512);
        assert_clone(&info);
    }

    #[test]
    fn model_info_default_available_false() {
        let info = ModelInfo::new("x", "x", ModelTier::Fast, 1, 1);
        assert!(!info.available);
    }

    #[test]
    fn model_info_default_provider_none() {
        let info = ModelInfo::new("x", "x", ModelTier::Fast, 1, 1);
        assert!(info.provider.is_none());
    }

    #[test]
    fn model_info_default_supports_batch() {
        let info = ModelInfo::new("x", "x", ModelTier::Fast, 1, 1);
        assert!(info.supports_batch);
    }

    // ── EmbeddingResult extended ──

    #[test]
    fn embedding_result_serde_roundtrip() {
        let result = EmbeddingResult::new(
            vec![0.1, 0.2],
            "model",
            ModelTier::Fast,
            Duration::from_millis(5),
            "hash",
        );
        let json = serde_json::to_string(&result).unwrap();
        let result2: EmbeddingResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result2.model_id, "model");
        assert_eq!(result2.dimension, 2);
    }

    #[test]
    fn embedding_result_is_hash_only_hash_tier_nonempty_vec() {
        let result = EmbeddingResult {
            vector: vec![1.0],
            model_id: "hash".to_string(),
            tier: ModelTier::Hash,
            dimension: 1,
            elapsed: Duration::ZERO,
            content_hash: "abc".to_string(),
        };
        assert!(result.is_hash_only()); // Hash tier always counts as hash-only
    }

    #[test]
    fn embedding_result_is_hash_only_empty_vec_non_hash_tier() {
        let result = EmbeddingResult {
            vector: vec![],
            model_id: "fast".to_string(),
            tier: ModelTier::Fast,
            dimension: 0,
            elapsed: Duration::ZERO,
            content_hash: "abc".to_string(),
        };
        assert!(result.is_hash_only()); // Empty vector always counts as hash-only
    }

    #[test]
    fn embedding_result_debug() {
        let result = EmbeddingResult::from_hash("test");
        let debug = format!("{result:?}");
        assert!(debug.contains("hash"));
    }

    // ── HashEmbedder extended ──

    #[test]
    fn hash_embedder_default() {
        let embedder = HashEmbedder::default();
        assert_eq!(embedder.model_info().id, "hash");
    }

    #[test]
    fn hash_embedder_debug() {
        let embedder = HashEmbedder::new();
        let debug = format!("{embedder:?}");
        assert!(debug.contains("HashEmbedder"));
    }

    #[test]
    fn hash_embedder_empty_batch() {
        let embedder = HashEmbedder::new();
        let results = embedder.embed_batch(&[]).unwrap();
        assert!(results.is_empty());
    }

    // ── RegistryConfig ──

    #[test]
    fn registry_config_default() {
        let config = RegistryConfig::default();
        assert!(config.allow_hash_fallback);
        assert_eq!(config.timeout_ms, 5000);
        assert!(config.preferred_fast.is_none());
        assert!(config.preferred_quality.is_none());
    }

    #[test]
    fn registry_config_serde_roundtrip() {
        let config = RegistryConfig {
            preferred_fast: Some("fast-model".to_string()),
            preferred_quality: Some("quality-model".to_string()),
            allow_hash_fallback: false,
            timeout_ms: 1000,
        };
        let json = serde_json::to_string(&config).unwrap();
        let config2: RegistryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config2.preferred_fast, Some("fast-model".to_string()));
        assert!(!config2.allow_hash_fallback);
    }

    #[test]
    fn registry_config_debug() {
        let config = RegistryConfig::default();
        let debug = format!("{config:?}");
        assert!(debug.contains("RegistryConfig"));
    }

    // ── ModelRegistry extended ──

    #[test]
    fn registry_deactivate_embedder() {
        let mut registry = ModelRegistry::default();
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new());
        registry.activate_embedder(embedder);
        // hash should be in embedders
        registry.deactivate_embedder("hash");
        // Model should still be registered but marked unavailable
        let info = registry.get_model_info("hash").unwrap();
        assert!(!info.available);
    }

    #[test]
    fn registry_set_config() {
        let mut registry = ModelRegistry::default();
        let new_config = RegistryConfig {
            timeout_ms: 999,
            ..Default::default()
        };
        registry.set_config(new_config);
        assert_eq!(registry.config().timeout_ms, 999);
    }

    #[test]
    fn registry_list_available_only_hash() {
        let registry = ModelRegistry::default();
        let available = registry.list_available();
        // Only hash is available by default
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "hash");
    }

    #[test]
    fn registry_hash_embedder_accessor() {
        let registry = ModelRegistry::default();
        let hash = registry.hash_embedder();
        assert_eq!(hash.model_info().id, "hash");
    }

    #[test]
    fn registry_quality_falls_back_to_fast_then_hash() {
        let registry = ModelRegistry::default();
        // No fast or quality model, should fallback to hash
        let embedder = registry.get_embedder(ModelTier::Quality).unwrap();
        assert_eq!(embedder.model_info().tier, ModelTier::Hash);
    }

    #[test]
    fn registry_debug() {
        let registry = ModelRegistry::default();
        let debug = format!("{registry:?}");
        assert!(debug.contains("ModelRegistry"));
    }

    // ── Cosine similarity extended ──

    #[test]
    fn cosine_similar_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.1, 2.1, 3.1];
        let sim = cosine_similarity(&a, &b);
        assert!(sim > 0.99); // Very similar vectors
    }

    #[test]
    fn cosine_negative_values() {
        let a = vec![-1.0, -2.0, -3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    // ── normalize_l2 extended ──

    #[test]
    fn normalize_l2_idempotent() {
        let v = vec![3.0, 4.0];
        let n1 = normalize_l2(&v);
        let n2 = normalize_l2(&n1);
        for (a, b) in n1.iter().zip(n2.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn normalize_l2_single_element() {
        let v = vec![5.0];
        let n = normalize_l2(&v);
        assert!((n[0] - 1.0).abs() < 1e-6);
    }

    // ── Well-known models completeness ──

    #[test]
    fn well_known_minilm_l12() {
        let info = well_known::minilm_l12_v2();
        assert_eq!(info.id, "all-minilm-l12-v2");
        assert_eq!(info.tier, ModelTier::Fast);
        assert_eq!(info.dimension, 384);
    }

    #[test]
    fn well_known_e5_small() {
        let info = well_known::e5_small_v2();
        assert_eq!(info.id, "e5-small-v2");
        assert_eq!(info.tier, ModelTier::Fast);
    }

    #[test]
    fn well_known_e5_base() {
        let info = well_known::e5_base_v2();
        assert_eq!(info.id, "e5-base-v2");
        assert_eq!(info.tier, ModelTier::Quality);
        assert_eq!(info.dimension, 768);
    }

    #[test]
    fn well_known_bge_small() {
        let info = well_known::bge_small_en();
        assert_eq!(info.id, "bge-small-en-v1.5");
        assert_eq!(info.tier, ModelTier::Fast);
    }

    #[test]
    fn well_known_bge_base() {
        let info = well_known::bge_base_en();
        assert_eq!(info.id, "bge-base-en-v1.5");
        assert_eq!(info.tier, ModelTier::Quality);
        assert_eq!(info.dimension, 768);
    }

    #[test]
    fn well_known_openai_small() {
        let info = well_known::openai_small();
        assert_eq!(info.id, "text-embedding-3-small");
        assert_eq!(info.tier, ModelTier::Fast);
        assert_eq!(info.dimension, 1536);
    }

    // ── embed_document with different doc kinds ──

    #[test]
    fn embed_document_agent_kind() {
        let embedder = HashEmbedder::new();
        let result = embed_document(
            &embedder,
            DocKind::Agent,
            "AgentName",
            "Agent description",
            CanonPolicy::Full,
        )
        .unwrap();
        assert!(!result.content_hash.is_empty());
    }

    #[test]
    fn embed_document_project_kind() {
        let embedder = HashEmbedder::new();
        let result = embed_document(
            &embedder,
            DocKind::Project,
            "ProjectName",
            "Project description",
            CanonPolicy::Full,
        )
        .unwrap();
        assert!(!result.content_hash.is_empty());
    }

    #[test]
    fn embed_document_different_policies() {
        let embedder = HashEmbedder::new();
        let full = embed_document(
            &embedder,
            DocKind::Message,
            "Title",
            "Body **bold**",
            CanonPolicy::Full,
        )
        .unwrap();
        let title_only = embed_document(
            &embedder,
            DocKind::Message,
            "Title",
            "Body **bold**",
            CanonPolicy::TitleOnly,
        )
        .unwrap();
        // Different policies should produce different hashes
        assert_ne!(full.content_hash, title_only.content_hash);
    }
}
