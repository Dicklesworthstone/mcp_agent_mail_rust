//! `Model2Vec` embedding backend for ultra-fast semantic search.
//!
//! Thin wrapper around [`frankensearch::Model2VecEmbedder`] that adds
//! agent-mail-specific model search paths and preserves the sync
//! `TwoTierEmbedder` interface.
//!
//! # Supported Models
//!
//! - `potion-retrieval-32M` (256 dims, ~32MB)
//! - `potion-multilingual-128M` (256 dims, ~128MB)

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use frankensearch::Embedder as _;

use crate::error::{SearchError, SearchResult};
use crate::fs_bridge::map_fs_error;
use crate::two_tier::TwoTierEmbedder;

/// Model name constant for potion-retrieval-32M.
pub const MODEL_POTION_32M: &str = "potion-retrieval-32M";

/// Model name constant for potion-multilingual-128M (our fast tier choice).
pub const MODEL_POTION_128M: &str = "potion-multilingual-128M";

/// `Model2Vec` embedder — thin wrapper around `frankensearch::Model2VecEmbedder`.
///
/// Delegates all embedding logic to frankensearch while preserving the
/// `TwoTierEmbedder` sync interface and agent-mail-specific model search paths.
pub struct Model2VecEmbedder {
    inner: frankensearch::Model2VecEmbedder,
}

impl std::fmt::Debug for Model2VecEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model2VecEmbedder")
            .field("name", &self.inner.id())
            .field("dimensions", &self.inner.dimension())
            .field("vocab_size", &self.inner.vocab_size())
            .finish_non_exhaustive()
    }
}

impl Model2VecEmbedder {
    /// Load the model from a directory containing model files.
    ///
    /// # Errors
    ///
    /// Returns an error if the model files are missing or invalid.
    pub fn load_from_dir(model_dir: &Path, model_name: &str) -> SearchResult<Self> {
        let inner = frankensearch::Model2VecEmbedder::load_with_name(model_dir, model_name)
            .map_err(map_fs_error)?;

        tracing::info!(
            model = model_name,
            vocab_size = inner.vocab_size(),
            dimensions = inner.dimension(),
            "Model2Vec embedder loaded (via frankensearch)"
        );

        Ok(Self { inner })
    }

    /// Try to load from standard model locations.
    ///
    /// Searches frankensearch standard paths first, then falls back to
    /// agent-mail-specific directories.
    ///
    /// # Errors
    ///
    /// Returns an error if the model cannot be found.
    pub fn try_load(model_name: &str) -> SearchResult<Self> {
        // Try frankensearch standard paths first (includes HuggingFace cache)
        if let Some(dir) = frankensearch_embed::find_model_dir(model_name) {
            return Self::load_from_dir(&dir, model_name);
        }

        // Fall back to agent-mail-specific paths
        let candidates = Self::agent_mail_model_paths(model_name);
        for candidate in &candidates {
            if candidate.exists()
                && let Ok(embedder) = Self::load_from_dir(candidate, model_name)
            {
                return Ok(embedder);
            }
        }

        Err(SearchError::ModeUnavailable(format!(
            "{model_name} model not found"
        )))
    }

    /// Agent-mail-specific model search paths (not covered by frankensearch).
    fn agent_mail_model_paths(model_name: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // mcp-agent-mail data directory
        if let Some(data) = dirs::data_local_dir() {
            paths.push(data.join("mcp-agent-mail").join("models").join(model_name));
        }

        // mcp-agent-mail cache directory
        if let Some(cache) = dirs::cache_dir() {
            paths.push(cache.join("mcp-agent-mail").join("models").join(model_name));
        }

        paths
    }

    /// Get standard model search paths (frankensearch + agent-mail).
    #[must_use]
    pub fn model_search_paths(model_name: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // HuggingFace hub cache
        if let Some(cache) = dirs::cache_dir() {
            paths.push(
                cache
                    .join("huggingface")
                    .join("hub")
                    .join(format!("models--minishlab--{model_name}")),
            );
        }

        // Agent-mail-specific paths
        paths.extend(Self::agent_mail_model_paths(model_name));

        paths
    }

    /// Check if a specific model is available.
    #[must_use]
    pub fn is_available(model_name: &str) -> bool {
        Self::try_load(model_name).is_ok()
    }

    /// Get the vocabulary size.
    #[must_use]
    pub const fn vocab_size(&self) -> usize {
        self.inner.vocab_size()
    }

    /// Access the inner frankensearch embedder.
    #[must_use]
    pub const fn as_inner(&self) -> &frankensearch::Model2VecEmbedder {
        &self.inner
    }
}

impl TwoTierEmbedder for Model2VecEmbedder {
    fn embed(&self, text: &str) -> SearchResult<Vec<f32>> {
        if text.is_empty() {
            return Err(SearchError::InvalidQuery("empty text".to_string()));
        }
        self.inner.embed_sync(text).map_err(map_fs_error)
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    fn id(&self) -> &str {
        self.inner.id()
    }
}

// ────────────────────────────────────────────────────────────────────
// Global auto-initialization
// ────────────────────────────────────────────────────────────────────

/// Global fast embedder instance (potion-128M).
static FAST_EMBEDDER: OnceLock<Option<Model2VecEmbedder>> = OnceLock::new();

/// Get the global fast embedder, auto-initializing if necessary.
///
/// Returns `None` if the model is not available.
#[must_use]
pub fn get_fast_embedder() -> Option<&'static Model2VecEmbedder> {
    FAST_EMBEDDER
        .get_or_init(|| {
            // Try potion-128M first (our preferred fast model)
            if let Ok(embedder) = Model2VecEmbedder::try_load(MODEL_POTION_128M) {
                tracing::info!(model = MODEL_POTION_128M, "Fast embedder auto-initialized");
                return Some(embedder);
            }

            // Fall back to potion-32M
            if let Ok(embedder) = Model2VecEmbedder::try_load(MODEL_POTION_32M) {
                tracing::info!(
                    model = MODEL_POTION_32M,
                    "Fast embedder auto-initialized (fallback)"
                );
                return Some(embedder);
            }

            tracing::warn!("No fast embedder model available");
            None
        })
        .as_ref()
}

/// Check if the fast embedder is available.
#[must_use]
pub fn is_fast_embedder_available() -> bool {
    get_fast_embedder().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_constants() {
        assert_eq!(MODEL_POTION_32M, "potion-retrieval-32M");
        assert_eq!(MODEL_POTION_128M, "potion-multilingual-128M");
    }

    #[test]
    fn test_model_search_paths() {
        let paths = Model2VecEmbedder::model_search_paths(MODEL_POTION_128M);
        assert!(!paths.is_empty());

        // Should include HuggingFace cache path
        assert!(
            paths
                .iter()
                .any(|p| p.to_string_lossy().contains("huggingface"))
        );
    }

    #[test]
    fn model_search_paths_32m() {
        let paths = Model2VecEmbedder::model_search_paths(MODEL_POTION_32M);
        assert!(!paths.is_empty());
        assert!(
            paths
                .iter()
                .any(|p| p.to_string_lossy().contains("mcp-agent-mail"))
        );
    }

    #[test]
    fn model_search_paths_custom_name() {
        let paths = Model2VecEmbedder::model_search_paths("my-custom-model");
        assert!(!paths.is_empty());
        assert!(
            paths
                .iter()
                .any(|p| p.to_string_lossy().contains("my-custom-model"))
        );
    }

    #[test]
    fn is_available_nonexistent_model() {
        assert!(!Model2VecEmbedder::is_available(
            "nonexistent-model-xyz-12345"
        ));
    }

    #[test]
    fn is_fast_embedder_available_no_panic() {
        let _ = is_fast_embedder_available();
    }

    #[test]
    fn get_fast_embedder_no_panic() {
        let _ = get_fast_embedder();
    }
}
