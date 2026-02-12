//! FastEmbed-based ML embedders for quality semantic search.
//!
//! Uses ONNX models for high-quality semantic embeddings.
//! These are slower than `Model2Vec` but produce better results.
//!
//! # Supported Models
//!
//! - `all-MiniLM-L6-v2` (384 dims) - Our quality tier choice
//! - `bge-small-en-v1.5` (384 dims)
//! - `nomic-embed-text-v1.5` (768 dims, supports MRL)

use std::sync::{Mutex, OnceLock};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::error::{SearchError, SearchResult};
use crate::two_tier::TwoTierEmbedder;

/// Model name constant for MiniLM-L6-v2 (our quality tier choice).
pub const MODEL_MINILM_L6_V2: &str = "all-MiniLM-L6-v2";

/// Model name constant for BGE Small.
pub const MODEL_BGE_SMALL: &str = "bge-small-en-v1.5";

/// FastEmbed-backed semantic embedder.
///
/// Uses ONNX runtime for transformer model inference.
/// Thread-safe via internal mutex.
pub struct FastEmbedEmbedder {
    model: Mutex<TextEmbedding>,
    id: String,
    dimension: usize,
}

impl std::fmt::Debug for FastEmbedEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastEmbedEmbedder")
            .field("id", &self.id)
            .field("dimension", &self.dimension)
            .finish_non_exhaustive()
    }
}

impl FastEmbedEmbedder {
    /// Load the MiniLM-L6-v2 model (our quality tier).
    ///
    /// # Errors
    ///
    /// Returns an error if the model cannot be loaded.
    pub fn load_minilm() -> SearchResult<Self> {
        Self::load_model(EmbeddingModel::AllMiniLML6V2, MODEL_MINILM_L6_V2, 384)
    }

    /// Load the BGE Small model.
    ///
    /// # Errors
    ///
    /// Returns an error if the model cannot be loaded.
    pub fn load_bge_small() -> SearchResult<Self> {
        Self::load_model(EmbeddingModel::BGESmallENV15, MODEL_BGE_SMALL, 384)
    }

    /// Load a specific `FastEmbed` model.
    fn load_model(model: EmbeddingModel, id: &str, dimension: usize) -> SearchResult<Self> {
        let options = InitOptions::new(model).with_show_download_progress(false);

        let text_embedding = TextEmbedding::try_new(options)
            .map_err(|e| SearchError::ModeUnavailable(format!("failed to load {id}: {e}")))?;

        tracing::info!(model = id, dimension = dimension, "FastEmbed model loaded");

        Ok(Self {
            model: Mutex::new(text_embedding),
            id: id.to_string(),
            dimension,
        })
    }

    /// Embed a single text.
    fn embed_internal(&self, text: &str) -> SearchResult<Vec<f32>> {
        if text.is_empty() {
            return Err(SearchError::InvalidQuery("empty text".to_string()));
        }

        let model = self
            .model
            .lock()
            .map_err(|_| SearchError::Internal("fastembed lock poisoned".to_string()))?;

        let embeddings = model
            .embed(vec![text], None)
            .map_err(|e| SearchError::Internal(format!("fastembed embed failed: {e}")))?;

        let mut embedding = embeddings.into_iter().next().ok_or_else(|| {
            SearchError::Internal("fastembed returned no embedding".to_string())
        })?;

        if embedding.len() != self.dimension {
            return Err(SearchError::Internal(format!(
                "fastembed dimension mismatch: expected {}, got {}",
                self.dimension,
                embedding.len()
            )));
        }

        // L2 normalize
        l2_normalize(&mut embedding);
        Ok(embedding)
    }

    /// Get the embedding dimension.
    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }

    /// Get the model ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl TwoTierEmbedder for FastEmbedEmbedder {
    fn embed(&self, text: &str) -> SearchResult<Vec<f32>> {
        self.embed_internal(text)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        &self.id
    }
}

/// L2-normalize a vector in place.
#[inline]
fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Global auto-initialization
// ────────────────────────────────────────────────────────────────────

/// Global quality embedder instance (MiniLM-L6-v2).
static QUALITY_EMBEDDER: OnceLock<Option<FastEmbedEmbedder>> = OnceLock::new();

/// Get the global quality embedder, auto-initializing if necessary.
///
/// Returns `None` if the model cannot be loaded.
#[must_use]
pub fn get_quality_embedder() -> Option<&'static FastEmbedEmbedder> {
    QUALITY_EMBEDDER
        .get_or_init(|| {
            // Try MiniLM first (our preferred quality model)
            match FastEmbedEmbedder::load_minilm() {
                Ok(embedder) => {
                    tracing::info!(
                        model = MODEL_MINILM_L6_V2,
                        "Quality embedder auto-initialized"
                    );
                    Some(embedder)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to load quality embedder");
                    None
                }
            }
        })
        .as_ref()
}

/// Check if the quality embedder is available.
#[must_use]
pub fn is_quality_embedder_available() -> bool {
    get_quality_embedder().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_constants() {
        assert_eq!(MODEL_MINILM_L6_V2, "all-MiniLM-L6-v2");
        assert_eq!(MODEL_BGE_SMALL, "bge-small-en-v1.5");
    }

    #[test]
    fn test_l2_normalize() {
        let mut vec = vec![3.0, 4.0];
        l2_normalize(&mut vec);

        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((vec[0] - 0.6).abs() < 1e-6);
        assert!((vec[1] - 0.8).abs() < 1e-6);
    }

    // Integration tests require model download
    #[test]
    #[ignore = "requires model download"]
    fn test_minilm_embedding() {
        let embedder = FastEmbedEmbedder::load_minilm().expect("should load");
        let embedding = embedder.embed_internal("hello world").expect("should embed");

        assert_eq!(embedding.len(), 384);

        // Check normalization
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }
}
