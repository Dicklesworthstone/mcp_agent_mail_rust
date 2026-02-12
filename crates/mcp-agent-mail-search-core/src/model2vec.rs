//! `Model2Vec` embedding backend for ultra-fast semantic search.
//!
//! `Model2Vec` uses distilled static embeddings from transformer models,
//! providing ~0ms inference time with reasonable semantic quality.
//!
//! # Supported Models
//!
//! - `potion-retrieval-32M` (256 dims, ~32MB)
//! - `potion-multilingual-128M` (256 dims, ~128MB)
//!
//! # File Format
//!
//! - `tokenizer.json` - `HuggingFace` tokenizer
//! - `model.safetensors` - Static embedding weights
//! - `config.json` - Model configuration (optional)

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use safetensors::SafeTensors;
use tokenizers::Tokenizer;

use crate::error::{SearchError, SearchResult};
use crate::two_tier::TwoTierEmbedder;

/// Model name constant for potion-retrieval-32M.
pub const MODEL_POTION_32M: &str = "potion-retrieval-32M";

/// Model name constant for potion-multilingual-128M (our fast tier choice).
pub const MODEL_POTION_128M: &str = "potion-multilingual-128M";

/// Required model files.
const REQUIRED_FILES: &[&str] = &["tokenizer.json", "model.safetensors"];

/// `Model2Vec` embedder using static embedding lookup.
///
/// This embedder loads a tokenizer and an embedding matrix, then performs:
/// 1. Subword tokenization
/// 2. Embedding lookup for each token
/// 3. Mean pooling over tokens
/// 4. L2 normalization
pub struct Model2VecEmbedder {
    /// Subword tokenizer (BPE or `WordPiece` from teacher model).
    tokenizer: Tokenizer,
    /// Static embedding matrix [`vocab_size` × dims].
    embeddings: Vec<Vec<f32>>,
    /// Output dimensions.
    dimensions: usize,
    /// Model identifier.
    name: String,
    /// Vocabulary size.
    vocab_size: usize,
}

impl std::fmt::Debug for Model2VecEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model2VecEmbedder")
            .field("name", &self.name)
            .field("dimensions", &self.dimensions)
            .field("vocab_size", &self.vocab_size)
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
        // Validate required files
        for file in REQUIRED_FILES {
            let file_path = model_dir.join(file);
            if !file_path.exists() {
                return Err(SearchError::ModeUnavailable(format!(
                    "missing required model file: {}",
                    file_path.display()
                )));
            }
        }

        // Load tokenizer
        let tokenizer_path = model_dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| SearchError::Internal(format!("failed to load tokenizer: {e}")))?;

        // Load embeddings from safetensors
        let embeddings_path = model_dir.join("model.safetensors");
        let embeddings_data = std::fs::read(&embeddings_path)
            .map_err(|e| SearchError::Internal(format!("failed to read embeddings: {e}")))?;

        let safetensors = SafeTensors::deserialize(&embeddings_data)
            .map_err(|e| SearchError::Internal(format!("failed to parse safetensors: {e}")))?;

        // Find the embedding tensor
        let tensor_name = Self::find_embedding_tensor_name(&safetensors)?;
        let tensor = safetensors.tensor(&tensor_name).map_err(|e| {
            SearchError::Internal(format!("failed to get tensor {tensor_name}: {e}"))
        })?;

        // Validate tensor shape [vocab_size, dims]
        let shape = tensor.shape();
        if shape.len() != 2 {
            return Err(SearchError::Internal(format!(
                "expected 2D tensor, got shape: {shape:?}"
            )));
        }
        let vocab_size = shape[0];
        let dimensions = shape[1];

        // Convert tensor data to embedding vectors
        let embeddings = Self::tensor_to_embeddings(tensor.data(), vocab_size, dimensions)?;

        tracing::info!(
            model = model_name,
            vocab_size = vocab_size,
            dimensions = dimensions,
            "Model2Vec embedder loaded"
        );

        Ok(Self {
            tokenizer,
            embeddings,
            dimensions,
            name: model_name.to_string(),
            vocab_size,
        })
    }

    /// Find the embedding tensor name in a safetensors file.
    fn find_embedding_tensor_name(safetensors: &SafeTensors<'_>) -> SearchResult<String> {
        let names: Vec<String> = safetensors.names().into_iter().cloned().collect();

        // Try common embedding tensor names
        for candidate in &["embeddings", "embedding", "word_embeddings", "embed", "emb"] {
            if names.contains(&(*candidate).to_string()) {
                return Ok((*candidate).to_string());
            }
        }

        // If only one tensor, use it
        if names.len() == 1 {
            return Ok(names[0].clone());
        }

        Err(SearchError::Internal(format!(
            "could not find embedding tensor. Available: {names:?}"
        )))
    }

    /// Convert raw tensor bytes to embedding vectors.
    fn tensor_to_embeddings(
        data: &[u8],
        vocab_size: usize,
        dimensions: usize,
    ) -> SearchResult<Vec<Vec<f32>>> {
        // Expect f32 data (4 bytes per float)
        let expected_bytes = vocab_size * dimensions * 4;
        if data.len() != expected_bytes {
            return Err(SearchError::Internal(format!(
                "tensor size mismatch: expected {expected_bytes} bytes, got {}",
                data.len()
            )));
        }

        let mut embeddings = Vec::with_capacity(vocab_size);
        for v in 0..vocab_size {
            let mut row = Vec::with_capacity(dimensions);
            for d in 0..dimensions {
                let offset = (v * dimensions + d) * 4;
                let bytes: [u8; 4] = data[offset..offset + 4].try_into().map_err(|_| {
                    SearchError::Internal("byte slice conversion failed".to_string())
                })?;
                row.push(f32::from_le_bytes(bytes));
            }
            embeddings.push(row);
        }

        Ok(embeddings)
    }

    /// Try to load from standard model locations.
    ///
    /// Searches in order:
    /// 1. `~/.cache/huggingface/hub/models--minishlab--<model_name>`
    /// 2. `~/.local/share/mcp-agent-mail/models/<model_name>`
    /// 3. `~/.cache/mcp-agent-mail/models/<model_name>`
    ///
    /// # Errors
    ///
    /// Returns an error if the model cannot be found.
    pub fn try_load(model_name: &str) -> SearchResult<Self> {
        let candidates = Self::model_search_paths(model_name);

        for candidate in &candidates {
            if candidate.exists() {
                // For HuggingFace hub cache, we need to find the snapshot directory
                if candidate.to_string_lossy().contains("huggingface") {
                    if let Some(snapshot_dir) = Self::find_hf_snapshot(candidate) {
                        if let Ok(embedder) = Self::load_from_dir(&snapshot_dir, model_name) {
                            return Ok(embedder);
                        }
                    }
                } else if let Ok(embedder) = Self::load_from_dir(candidate, model_name) {
                    return Ok(embedder);
                }
            }
        }

        Err(SearchError::ModeUnavailable(format!(
            "{model_name} model not found. Searched: {}",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    /// Find the latest snapshot directory in a `HuggingFace` hub cache.
    fn find_hf_snapshot(hub_path: &Path) -> Option<PathBuf> {
        let snapshots_dir = hub_path.join("snapshots");
        if !snapshots_dir.exists() {
            return None;
        }

        std::fs::read_dir(&snapshots_dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().ok().is_some_and(|ft| ft.is_dir()))
            .map(|e| e.path())
            .max_by_key(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            })
    }

    /// Get standard model search paths.
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

    /// Check if a specific model is available.
    #[must_use]
    pub fn is_available(model_name: &str) -> bool {
        Self::try_load(model_name).is_ok()
    }

    /// Get the vocabulary size.
    #[must_use]
    pub const fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    /// Embed a single text using static lookup + mean pooling.
    fn embed_internal(&self, text: &str) -> SearchResult<Vec<f32>> {
        if text.is_empty() {
            return Err(SearchError::InvalidQuery("empty text".to_string()));
        }

        // Tokenize
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| SearchError::Internal(format!("tokenization failed: {e}")))?;

        let token_ids = encoding.get_ids();

        if token_ids.is_empty() {
            return Err(SearchError::InvalidQuery(
                "text tokenizes to empty sequence".to_string(),
            ));
        }

        // Mean pool over token embeddings
        let mut sum = vec![0.0f32; self.dimensions];
        let mut count = 0usize;

        for &token_id in token_ids {
            let idx = token_id as usize;
            if idx < self.vocab_size {
                let row = &self.embeddings[idx];
                for (s, &r) in sum.iter_mut().zip(row.iter()) {
                    *s += r;
                }
                count += 1;
            }
            // OOV tokens are silently skipped
        }

        if count == 0 {
            return Err(SearchError::Internal("all tokens were OOV".to_string()));
        }

        // Compute mean
        #[allow(clippy::cast_precision_loss)]
        let inv = 1.0 / count as f32;
        for s in &mut sum {
            *s *= inv;
        }

        // L2 normalize
        l2_normalize(&mut sum);

        Ok(sum)
    }
}

impl TwoTierEmbedder for Model2VecEmbedder {
    fn embed(&self, text: &str) -> SearchResult<Vec<f32>> {
        self.embed_internal(text)
    }

    fn dimension(&self) -> usize {
        self.dimensions
    }

    fn id(&self) -> &str {
        &self.name
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
    fn test_required_files() {
        assert!(REQUIRED_FILES.contains(&"tokenizer.json"));
        assert!(REQUIRED_FILES.contains(&"model.safetensors"));
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
    fn test_tensor_to_embeddings_small() {
        // Create a small test tensor: 2 vocab × 3 dims
        let data: Vec<u8> = vec![
            // Row 0: [1.0, 2.0, 3.0]
            0x00, 0x00, 0x80, 0x3F, // 1.0
            0x00, 0x00, 0x00, 0x40, // 2.0
            0x00, 0x00, 0x40, 0x40, // 3.0
            // Row 1: [4.0, 5.0, 6.0]
            0x00, 0x00, 0x80, 0x40, // 4.0
            0x00, 0x00, 0xA0, 0x40, // 5.0
            0x00, 0x00, 0xC0, 0x40, // 6.0
        ];

        let embeddings = Model2VecEmbedder::tensor_to_embeddings(&data, 2, 3).unwrap();

        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 3);
        assert_eq!(embeddings[1].len(), 3);

        assert!((embeddings[0][0] - 1.0).abs() < 1e-6);
        assert!((embeddings[0][1] - 2.0).abs() < 1e-6);
        assert!((embeddings[0][2] - 3.0).abs() < 1e-6);

        assert!((embeddings[1][0] - 4.0).abs() < 1e-6);
        assert!((embeddings[1][1] - 5.0).abs() < 1e-6);
        assert!((embeddings[1][2] - 6.0).abs() < 1e-6);
    }

    #[test]
    fn test_tensor_size_mismatch() {
        let data = vec![0u8; 10]; // Not a valid tensor size
        let result = Model2VecEmbedder::tensor_to_embeddings(&data, 2, 3);
        assert!(result.is_err());
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
}
