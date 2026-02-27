//! Bridge module between `mcp-agent-mail-search-core` and `frankensearch`.
//!
//! frankensearch is a standalone two-tier hybrid search engine that this crate
//! progressively migrates toward. This bridge module provides:
//!
//! - Re-exports of frankensearch types for direct use
//! - Conversion functions between domain-specific types (`u64` doc IDs, `DocKind`)
//!   and frankensearch's generic types (`String` doc IDs)
//! - [`SyncEmbedderAdapter`]: wraps search-core's sync `TwoTierEmbedder` trait
//!   as frankensearch's async `Embedder` trait

use std::sync::Arc;

use crate::document::DocKind;

// Cx re-exported through frankensearch (which re-exports from asupersync).
// We do NOT depend on asupersync directly from this crate.
use frankensearch::Cx;

// ─── Re-exports ──────────────────────────────────────────────────────────────

/// The frankensearch facade crate, available for direct advanced access.
pub use frankensearch as fs;

// Core types re-exported with `Fs` prefix to avoid ambiguity with
// search-core's domain-specific types of the same name.
pub use frankensearch::Cx as FsCx;
pub use frankensearch::core::config::{
    TwoTierConfig as FsTwoTierConfig, TwoTierMetrics as FsTwoTierMetrics,
};
pub use frankensearch::core::traits::{
    Embedder as FsEmbedder, ModelCategory as FsModelCategory, ModelInfo as FsModelInfo,
    ModelTier as FsModelTier, SearchFuture as FsSearchFuture,
};
pub use frankensearch::core::types::{
    FusedHit as FsFusedHit, IndexableDocument, PhaseMetrics as FsPhaseMetrics,
    ScoredResult as FsScoredResult, SearchMode as FsSearchMode, SearchPhase as FsSearchPhase,
    VectorHit as FsVectorHit,
};
pub use frankensearch::{
    EmbedderStack as FsEmbedderStack, IndexBuilder as FsIndexBuilder, RrfConfig as FsRrfConfig,
    TantivyIndex as FsTantivyIndex, TwoTierAvailability as FsTwoTierAvailability,
    TwoTierIndex as FsTwoTierIndex, TwoTierSearcher as FsTwoTierSearcher,
    VectorIndex as FsVectorIndex, rrf_fuse as fs_rrf_fuse,
};

// ─── Doc ID Conversion ──────────────────────────────────────────────────────

/// Convert a domain-specific `u64` document ID to frankensearch's `String` format.
#[inline]
#[must_use]
pub fn doc_id_to_string(id: u64) -> String {
    id.to_string()
}

/// Parse a frankensearch `String` doc ID back to the domain-specific `u64`.
///
/// Returns `None` if the string is not a valid `u64`.
#[inline]
#[must_use]
pub fn doc_id_from_string(id: &str) -> Option<u64> {
    id.parse().ok()
}

// ─── Config Conversion ──────────────────────────────────────────────────────

/// Convert a search-core `TwoTierConfig` into a frankensearch `TwoTierConfig`.
///
/// Fields that exist in frankensearch but not in search-core are filled with
/// defaults. The `quality_weight` is widened from `f32` to `f64`.
#[must_use]
pub fn to_fs_config(config: &crate::two_tier::TwoTierConfig) -> FsTwoTierConfig {
    FsTwoTierConfig {
        quality_weight: f64::from(config.quality_weight),
        fast_only: config.fast_only,
        ..FsTwoTierConfig::default()
    }
}

/// Convert a frankensearch `TwoTierConfig` back to search-core's version.
///
/// Precision may be lost when narrowing `quality_weight` from `f64` to `f32`.
#[allow(clippy::cast_possible_truncation)]
#[must_use]
pub fn from_fs_config(config: &FsTwoTierConfig) -> crate::two_tier::TwoTierConfig {
    crate::two_tier::TwoTierConfig {
        quality_weight: config.quality_weight as f32,
        fast_only: config.fast_only,
        ..crate::two_tier::TwoTierConfig::default()
    }
}

// ─── Result Conversion ──────────────────────────────────────────────────────

/// Convert a frankensearch `ScoredResult` to a search-core `ScoredResult`.
///
/// Returns `None` if the `doc_id` cannot be parsed as `u64`.
/// Domain-specific fields (`doc_kind`, `project_id`) are set to defaults;
/// callers should enrich them from the document store.
#[must_use]
pub fn from_fs_scored_result(result: &FsScoredResult) -> Option<crate::two_tier::ScoredResult> {
    let doc_id: u64 = result.doc_id.parse().ok()?;
    Some(crate::two_tier::ScoredResult {
        idx: 0,
        doc_id,
        doc_kind: DocKind::Message,
        project_id: None,
        score: result.score,
    })
}

/// Convert a batch of frankensearch `ScoredResult`s, skipping unparseable IDs.
#[must_use]
pub fn from_fs_scored_results(results: &[FsScoredResult]) -> Vec<crate::two_tier::ScoredResult> {
    results.iter().filter_map(from_fs_scored_result).collect()
}

/// Convert a search-core `ScoredResult` to a frankensearch `ScoredResult`.
#[must_use]
pub fn to_fs_scored_result(result: &crate::two_tier::ScoredResult) -> FsScoredResult {
    use frankensearch::core::types::ScoreSource;
    FsScoredResult {
        doc_id: doc_id_to_string(result.doc_id),
        score: result.score,
        source: ScoreSource::SemanticFast,
        index: None,
        fast_score: Some(result.score),
        quality_score: None,
        lexical_score: None,
        rerank_score: None,
        explanation: None,
        metadata: None,
    }
}

// ─── ModelTier Conversion ────────────────────────────────────────────────────

/// Convert search-core's `ModelTier` (which has a `Hash` variant) to
/// frankensearch's `ModelTier` (which does not).
///
/// `Hash` maps to `Fast` since hash embedders serve as fast-tier fallback.
#[must_use]
pub const fn to_fs_model_tier(tier: crate::embedder::ModelTier) -> FsModelTier {
    match tier {
        crate::embedder::ModelTier::Hash | crate::embedder::ModelTier::Fast => FsModelTier::Fast,
        crate::embedder::ModelTier::Quality => FsModelTier::Quality,
    }
}

/// Convert frankensearch's `ModelTier` to search-core's version.
#[must_use]
pub const fn from_fs_model_tier(tier: FsModelTier) -> crate::embedder::ModelTier {
    match tier {
        FsModelTier::Fast => crate::embedder::ModelTier::Fast,
        FsModelTier::Quality => crate::embedder::ModelTier::Quality,
    }
}

// ─── Sync Embedder Adapter ──────────────────────────────────────────────────

/// Adapts a search-core `TwoTierEmbedder` (sync) to frankensearch's `Embedder`
/// (async with `&Cx`).
///
/// The async wrapper simply resolves immediately since the underlying embedder
/// is synchronous. The `&Cx` parameter is accepted but unused.
pub struct SyncEmbedderAdapter {
    inner: Arc<dyn crate::two_tier::TwoTierEmbedder>,
    model_name: String,
    is_semantic: bool,
    category: FsModelCategory,
}

impl SyncEmbedderAdapter {
    /// Wrap a sync `TwoTierEmbedder` as a frankensearch `Embedder`.
    #[must_use]
    pub fn new(
        embedder: Arc<dyn crate::two_tier::TwoTierEmbedder>,
        is_semantic: bool,
        category: FsModelCategory,
    ) -> Self {
        let model_name = embedder.id().to_owned();
        Self {
            inner: embedder,
            model_name,
            is_semantic,
            category,
        }
    }

    /// Create a fast-tier adapter (e.g., for `Model2Vec`).
    #[must_use]
    pub fn fast(embedder: Arc<dyn crate::two_tier::TwoTierEmbedder>) -> Self {
        Self::new(embedder, true, FsModelCategory::StaticEmbedder)
    }

    /// Create a quality-tier adapter (e.g., for `FastEmbed`).
    #[must_use]
    pub fn quality(embedder: Arc<dyn crate::two_tier::TwoTierEmbedder>) -> Self {
        Self::new(embedder, true, FsModelCategory::TransformerEmbedder)
    }

    /// Create a hash-tier adapter.
    #[must_use]
    pub fn hash(embedder: Arc<dyn crate::two_tier::TwoTierEmbedder>) -> Self {
        Self::new(embedder, false, FsModelCategory::HashEmbedder)
    }
}

impl std::fmt::Debug for SyncEmbedderAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncEmbedderAdapter")
            .field("model_name", &self.model_name)
            .field("dimension", &self.inner.dimension())
            .field("is_semantic", &self.is_semantic)
            .field("category", &self.category)
            .finish()
    }
}

impl FsEmbedder for SyncEmbedderAdapter {
    fn embed<'a>(&'a self, _cx: &'a Cx, text: &'a str) -> FsSearchFuture<'a, Vec<f32>> {
        Box::pin(async move {
            self.inner
                .embed(text)
                .map_err(|e| frankensearch::SearchError::EmbeddingFailed {
                    model: self.model_name.clone(),
                    source: Box::new(e),
                })
        })
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    fn id(&self) -> &str {
        self.inner.id()
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn is_semantic(&self) -> bool {
        self.is_semantic
    }

    fn category(&self) -> FsModelCategory {
        self.category
    }
}

// ─── Error Mapping ─────────────────────────────────────────────────────────

/// Map a frankensearch `SearchError` to a search-core `SearchError`.
///
/// This preserves the error semantics while translating between the two
/// crates' error enums.
#[must_use]
pub fn map_fs_error(err: frankensearch::SearchError) -> crate::error::SearchError {
    match err {
        frankensearch::SearchError::ModelNotFound { name } => {
            crate::error::SearchError::ModeUnavailable(format!("model not found: {name}"))
        }
        frankensearch::SearchError::ModelLoadFailed { path, source } => {
            crate::error::SearchError::Internal(format!(
                "model load failed at {}: {source}",
                path.display()
            ))
        }
        frankensearch::SearchError::EmbeddingFailed { model, source } => {
            crate::error::SearchError::Internal(format!("embedding failed ({model}): {source}"))
        }
        frankensearch::SearchError::Cancelled { phase, reason } => {
            crate::error::SearchError::Timeout(format!("{phase}: {reason}"))
        }
        other => crate::error::SearchError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_id_roundtrip() {
        let id: u64 = 42;
        let s = doc_id_to_string(id);
        assert_eq!(s, "42");
        assert_eq!(doc_id_from_string(&s), Some(id));
    }

    #[test]
    fn doc_id_from_invalid_string() {
        assert_eq!(doc_id_from_string("not-a-number"), None);
        assert_eq!(doc_id_from_string(""), None);
    }

    #[test]
    fn config_roundtrip() {
        let config = crate::two_tier::TwoTierConfig {
            quality_weight: 0.8,
            fast_only: true,
            ..crate::two_tier::TwoTierConfig::default()
        };
        let fs_config = to_fs_config(&config);
        assert!((fs_config.quality_weight - 0.8).abs() < 1e-6);
        assert!(fs_config.fast_only);

        let back = from_fs_config(&fs_config);
        assert!((back.quality_weight - 0.8).abs() < 1e-6);
        assert!(back.fast_only);
    }

    #[test]
    fn model_tier_conversion() {
        assert_eq!(
            to_fs_model_tier(crate::embedder::ModelTier::Hash),
            FsModelTier::Fast
        );
        assert_eq!(
            to_fs_model_tier(crate::embedder::ModelTier::Fast),
            FsModelTier::Fast
        );
        assert_eq!(
            to_fs_model_tier(crate::embedder::ModelTier::Quality),
            FsModelTier::Quality
        );
        assert_eq!(
            from_fs_model_tier(FsModelTier::Fast),
            crate::embedder::ModelTier::Fast
        );
        assert_eq!(
            from_fs_model_tier(FsModelTier::Quality),
            crate::embedder::ModelTier::Quality
        );
    }

    #[test]
    fn scored_result_conversion() {
        use frankensearch::core::types::ScoreSource;

        let fs_result = FsScoredResult {
            doc_id: "123".to_string(),
            score: 0.95,
            source: ScoreSource::SemanticFast,
            index: None,
            fast_score: Some(0.95),
            quality_score: None,
            lexical_score: None,
            rerank_score: None,
            explanation: None,
            metadata: None,
        };

        let domain = from_fs_scored_result(&fs_result).unwrap();
        assert_eq!(domain.doc_id, 123);
        assert!((domain.score - 0.95).abs() < 1e-6);

        let back = to_fs_scored_result(&domain);
        assert_eq!(back.doc_id, "123");
        assert!((back.score - 0.95).abs() < 1e-6);
    }

    #[test]
    fn scored_result_unparseable_id_returns_none() {
        use frankensearch::core::types::ScoreSource;

        let fs_result = FsScoredResult {
            doc_id: "not-a-u64".to_string(),
            score: 0.5,
            source: ScoreSource::Hybrid,
            index: None,
            fast_score: None,
            quality_score: None,
            lexical_score: None,
            rerank_score: None,
            explanation: None,
            metadata: None,
        };
        assert!(from_fs_scored_result(&fs_result).is_none());
    }

    // ── Batch conversion (from_fs_scored_results) ─────────────────────

    #[test]
    fn batch_scored_results_skips_invalid() {
        use frankensearch::core::types::ScoreSource;

        fn make(id: &str, score: f32) -> FsScoredResult {
            FsScoredResult {
                doc_id: id.to_string(),
                score,
                source: ScoreSource::SemanticFast,
                index: None,
                fast_score: Some(score),
                quality_score: None,
                lexical_score: None,
                rerank_score: None,
                explanation: None,
                metadata: None,
            }
        }

        let results = vec![
            make("10", 0.9),
            make("bad", 0.5), // invalid — skipped
            make("20", 0.8),
            make("", 0.1), // invalid — skipped
            make("30", 0.7),
        ];
        let converted = from_fs_scored_results(&results);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].doc_id, 10);
        assert_eq!(converted[1].doc_id, 20);
        assert_eq!(converted[2].doc_id, 30);
    }

    #[test]
    fn batch_scored_results_empty_input() {
        let converted = from_fs_scored_results(&[]);
        assert!(converted.is_empty());
    }

    // ── Doc ID boundary values ────────────────────────────────────────

    #[test]
    fn doc_id_zero() {
        assert_eq!(doc_id_to_string(0), "0");
        assert_eq!(doc_id_from_string("0"), Some(0));
    }

    #[test]
    fn doc_id_u64_max() {
        let max = u64::MAX;
        let s = doc_id_to_string(max);
        assert_eq!(s, "18446744073709551615");
        assert_eq!(doc_id_from_string(&s), Some(max));
    }

    #[test]
    fn doc_id_negative_string_returns_none() {
        assert_eq!(doc_id_from_string("-1"), None);
    }

    #[test]
    fn doc_id_overflow_string_returns_none() {
        // u64::MAX + 1
        assert_eq!(doc_id_from_string("18446744073709551616"), None);
    }

    // ── Config edge cases ─────────────────────────────────────────────

    #[test]
    fn config_quality_weight_precision_loss() {
        // f64 → f32 → f64 may lose precision
        let fs_config = FsTwoTierConfig {
            quality_weight: 0.123_456_789_012_345_6_f64,
            fast_only: false,
            ..FsTwoTierConfig::default()
        };
        let domain = from_fs_config(&fs_config);
        // f32 preserves ~7 significant digits
        assert!((domain.quality_weight - 0.123_456_79).abs() < 1e-7);
    }

    #[test]
    fn config_fast_only_false_roundtrip() {
        let config = crate::two_tier::TwoTierConfig {
            quality_weight: 0.5,
            fast_only: false,
            ..crate::two_tier::TwoTierConfig::default()
        };
        let fs = to_fs_config(&config);
        assert!(!fs.fast_only);
        let back = from_fs_config(&fs);
        assert!(!back.fast_only);
    }

    // ── map_fs_error ──────────────────────────────────────────────────

    #[test]
    fn map_fs_error_model_not_found() {
        let err = frankensearch::SearchError::ModelNotFound {
            name: "bge-m3".to_string(),
        };
        let mapped = map_fs_error(err);
        match mapped {
            crate::error::SearchError::ModeUnavailable(msg) => {
                assert!(msg.contains("bge-m3"));
            }
            other => panic!("expected ModeUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn map_fs_error_model_load_failed() {
        let err = frankensearch::SearchError::ModelLoadFailed {
            path: std::path::PathBuf::from("/models/bad.onnx"),
            source: Box::new(std::io::Error::other("corrupt")),
        };
        let mapped = map_fs_error(err);
        match mapped {
            crate::error::SearchError::Internal(msg) => {
                assert!(msg.contains("/models/bad.onnx"));
                assert!(msg.contains("corrupt"));
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn map_fs_error_embedding_failed() {
        let err = frankensearch::SearchError::EmbeddingFailed {
            model: "model2vec".to_string(),
            source: Box::new(std::io::Error::other("OOM")),
        };
        let mapped = map_fs_error(err);
        match mapped {
            crate::error::SearchError::Internal(msg) => {
                assert!(msg.contains("model2vec"));
                assert!(msg.contains("OOM"));
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn map_fs_error_cancelled() {
        let err = frankensearch::SearchError::Cancelled {
            phase: "embed_quality".to_string(),
            reason: "budget exhausted".to_string(),
        };
        let mapped = map_fs_error(err);
        match mapped {
            crate::error::SearchError::Timeout(msg) => {
                assert!(msg.contains("embed_quality"));
                assert!(msg.contains("budget exhausted"));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn map_fs_error_other_variant_falls_through() {
        // Use a variant not explicitly matched (e.g., IndexCorrupted)
        let err = frankensearch::SearchError::IndexCorrupted {
            path: std::path::PathBuf::from("/idx"),
            detail: "bad CRC".to_string(),
        };
        let mapped = map_fs_error(err);
        match mapped {
            crate::error::SearchError::Internal(msg) => {
                assert!(!msg.is_empty());
            }
            other => panic!("expected Internal from catch-all, got {other:?}"),
        }
    }

    // ── SyncEmbedderAdapter ───────────────────────────────────────────

    /// Minimal `TwoTierEmbedder` impl for testing the adapter.
    struct StubEmbedder {
        dim: usize,
        name: String,
    }

    impl StubEmbedder {
        fn new(name: &str, dim: usize) -> Self {
            Self {
                dim,
                name: name.to_string(),
            }
        }
    }

    impl crate::two_tier::TwoTierEmbedder for StubEmbedder {
        fn embed(&self, _text: &str) -> crate::error::SearchResult<Vec<f32>> {
            Ok(vec![0.0; self.dim])
        }
        fn dimension(&self) -> usize {
            self.dim
        }
        fn id(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn adapter_fast_constructor() {
        let emb = Arc::new(StubEmbedder::new("fast-stub", 64));
        let adapter = SyncEmbedderAdapter::fast(emb);
        assert!(adapter.is_semantic);
        assert_eq!(adapter.category, FsModelCategory::StaticEmbedder);
        assert_eq!(adapter.model_name, "fast-stub");
    }

    #[test]
    fn adapter_quality_constructor() {
        let emb = Arc::new(StubEmbedder::new("quality-stub", 384));
        let adapter = SyncEmbedderAdapter::quality(emb);
        assert!(adapter.is_semantic);
        assert_eq!(adapter.category, FsModelCategory::TransformerEmbedder);
        assert_eq!(adapter.model_name, "quality-stub");
    }

    #[test]
    fn adapter_hash_constructor() {
        let emb = Arc::new(StubEmbedder::new("hash-stub", 128));
        let adapter = SyncEmbedderAdapter::hash(emb);
        assert!(!adapter.is_semantic);
        assert_eq!(adapter.category, FsModelCategory::HashEmbedder);
        assert_eq!(adapter.model_name, "hash-stub");
    }

    #[test]
    fn adapter_debug_format() {
        let emb = Arc::new(StubEmbedder::new("dbg-stub", 32));
        let adapter = SyncEmbedderAdapter::fast(emb);
        let debug = format!("{adapter:?}");
        assert!(debug.contains("SyncEmbedderAdapter"));
        assert!(debug.contains("dbg-stub"));
        assert!(debug.contains("32")); // dimension
    }

    #[test]
    fn adapter_embedder_trait_methods() {
        let emb = Arc::new(StubEmbedder::new("trait-stub", 16));
        let adapter = SyncEmbedderAdapter::new(emb, true, FsModelCategory::StaticEmbedder);
        // FsEmbedder trait methods
        assert_eq!(FsEmbedder::dimension(&adapter), 16);
        assert_eq!(FsEmbedder::id(&adapter), "trait-stub");
        assert_eq!(FsEmbedder::model_name(&adapter), "trait-stub");
        assert!(FsEmbedder::is_semantic(&adapter));
        assert_eq!(
            FsEmbedder::category(&adapter),
            FsModelCategory::StaticEmbedder
        );
    }
}
