//! Automatic two-tier search initialization.
//!
//! This module provides automatic embedder detection and initialization
//! for the two-tier progressive search system. No manual setup required.
//!
//! # How It Works
//!
//! On first access, the system automatically:
//! 1. Checks for potion-128M (fast tier) in `HuggingFace` cache
//! 2. Loads `MiniLM-L6-v2` (quality tier) via `FastEmbed`
//! 3. Creates a global `TwoTierSearchContext` ready for use
//!
//! # Usage
//!
//! ```ignore
//! use mcp_agent_mail_search_core::auto_init::{get_two_tier_context, TwoTierAvailability};
//!
//! // Get the auto-initialized context (lazy, thread-safe)
//! let ctx = get_two_tier_context();
//!
//! match ctx.availability() {
//!     TwoTierAvailability::Full => {
//!         // Both fast and quality tiers available
//!     }
//!     TwoTierAvailability::FastOnly => {
//!         // Only fast tier, quality refinement disabled
//!     }
//!     TwoTierAvailability::QualityOnly => {
//!         // Only quality tier (unusual)
//!     }
//!     TwoTierAvailability::None => {
//!         // Fall back to lexical-only search
//!     }
//! }
//! ```

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::search_error::SearchResult;
#[cfg(feature = "quality-fastembed")]
use crate::search_fastembed::get_quality_embedder;
use crate::search_fs_bridge::{FsEmbedderStack, SyncEmbedderAdapter};
use crate::search_metrics::TwoTierInitMetrics;
use crate::search_model2vec::{Model2VecEmbedder, get_fast_embedder};
use crate::search_two_tier::{TwoTierConfig, TwoTierEmbedder, TwoTierIndex, TwoTierSearcher};

/// Availability status for two-tier search.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TwoTierAvailability {
    /// Both fast and quality embedders are available.
    Full,
    /// Only fast embedder available (quality refinement disabled).
    FastOnly,
    /// Only quality embedder available (no instant results).
    QualityOnly,
    /// No embedders available (fall back to lexical search).
    None,
}

impl std::fmt::Display for TwoTierAvailability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full (fast + quality)"),
            Self::FastOnly => write!(f, "fast-only"),
            Self::QualityOnly => write!(f, "quality-only"),
            Self::None => write!(f, "unavailable"),
        }
    }
}

/// Global context for two-tier search.
///
/// This provides thread-safe access to the auto-initialized embedders
/// and search infrastructure.
#[derive(Debug)]
pub struct TwoTierContext {
    /// Availability status.
    availability: TwoTierAvailability,
    /// Configuration.
    config: TwoTierConfig,
    /// Fast embedder info (if available).
    fast_info: Option<EmbedderInfo>,
    /// Quality embedder info (if available).
    quality_info: Option<EmbedderInfo>,
    /// Initialization timing and mode metrics.
    init_metrics: TwoTierInitMetrics,
}

/// Basic embedder information.
#[derive(Debug, Clone)]
pub struct EmbedderInfo {
    /// Embedder ID.
    pub id: String,
    /// Output dimension.
    pub dimension: usize,
}

#[cfg(feature = "quality-fastembed")]
const QUALITY_INSTALL_HINT: &str = "pip install fastembed && python -c \"from fastembed import TextEmbedding; TextEmbedding('sentence-transformers/all-MiniLM-L6-v2')\"";
#[cfg(not(feature = "quality-fastembed"))]
const QUALITY_INSTALL_HINT: &str =
    "quality tier disabled at compile time; build with feature \"quality-fastembed\"";

#[cfg(feature = "quality-fastembed")]
fn current_quality_embedder_info() -> Option<EmbedderInfo> {
    get_quality_embedder().map(|e| EmbedderInfo {
        id: e.id().to_string(),
        dimension: e.dimension(),
    })
}

#[cfg(not(feature = "quality-fastembed"))]
fn current_quality_embedder_info() -> Option<EmbedderInfo> {
    None
}

#[cfg(feature = "quality-fastembed")]
fn quality_embedder_available() -> bool {
    get_quality_embedder().is_some()
}

#[cfg(not(feature = "quality-fastembed"))]
const fn quality_embedder_available() -> bool {
    false
}

#[cfg(feature = "quality-fastembed")]
fn embed_quality_query(query: &str) -> SearchResult<Vec<f32>> {
    get_quality_embedder()
        .ok_or_else(|| {
            crate::search_error::SearchError::ModeUnavailable("quality embedder unavailable".into())
        })?
        .embed(query)
}

#[cfg(not(feature = "quality-fastembed"))]
fn embed_quality_query(_query: &str) -> SearchResult<Vec<f32>> {
    Err(crate::search_error::SearchError::ModeUnavailable(
        "quality embedder unavailable".into(),
    ))
}

#[cfg(feature = "quality-fastembed")]
fn quality_embedder_dimension() -> usize {
    get_quality_embedder().map_or(384, crate::search_fastembed::FastEmbedEmbedder::dimension)
}

#[cfg(not(feature = "quality-fastembed"))]
const fn quality_embedder_dimension() -> usize {
    384
}

#[cfg(feature = "quality-fastembed")]
fn quality_embedder_id() -> &'static str {
    get_quality_embedder().map_or("unavailable", |e| e.id())
}

#[cfg(not(feature = "quality-fastembed"))]
const fn quality_embedder_id() -> &'static str {
    "unavailable"
}

impl TwoTierContext {
    /// Initialize the context, detecting available embedders.
    fn init() -> Self {
        let _init_span = tracing::info_span!("two_tier.init").entered();

        let init_attempts = next_init_attempt();
        let init_timestamp = chrono::Utc::now().timestamp();

        let fast_start = Instant::now();
        let fast_embedder = get_fast_embedder();
        #[allow(clippy::cast_possible_truncation)]
        let fast_embedder_load_ms = fast_start.elapsed().as_millis() as u64;
        let has_fast = fast_embedder.is_some();

        let quality_start = Instant::now();
        let quality_info = current_quality_embedder_info();
        #[allow(clippy::cast_possible_truncation)]
        let quality_embedder_load_ms = quality_start.elapsed().as_millis() as u64;
        let has_quality = quality_info.is_some();

        let availability = match (has_fast, has_quality) {
            (true, true) => TwoTierAvailability::Full,
            (true, false) => TwoTierAvailability::FastOnly,
            (false, true) => TwoTierAvailability::QualityOnly,
            (false, false) => TwoTierAvailability::None,
        };

        let fast_info = fast_embedder.map(|e| EmbedderInfo {
            id: e.id().to_string(),
            dimension: e.dimension(),
        });

        // Adjust config based on available embedders
        let config = TwoTierConfig {
            fast_dimension: fast_info.as_ref().map_or(256, |i| i.dimension),
            quality_dimension: quality_info.as_ref().map_or(384, |i| i.dimension),
            ..TwoTierConfig::default()
        };

        let init_metrics = TwoTierInitMetrics {
            init_timestamp,
            fast_embedder_load_ms,
            quality_embedder_load_ms,
            availability,
            init_attempts,
        };

        tracing::info!(
            target: "search.two_tier",
            availability = %availability,
            init_attempts,
            fast_embedder_load_ms,
            quality_embedder_load_ms,
            fast = ?fast_info.as_ref().map(|i| &i.id),
            quality = ?quality_info.as_ref().map(|i| &i.id),
            "Two-tier search context initialized"
        );
        match availability {
            TwoTierAvailability::Full => {
                tracing::info!(
                    target: "search.two_tier",
                    fast_model = ?fast_info.as_ref().map(|i| &i.id),
                    quality_model = ?quality_info.as_ref().map(|i| &i.id),
                    fast_embedder_load_ms,
                    quality_embedder_load_ms,
                    "Two-tier search initialized: full mode (fast + quality refinement)"
                );
            }
            TwoTierAvailability::FastOnly => {
                tracing::warn!(
                    target: "search.two_tier",
                    fast_model = ?fast_info.as_ref().map(|i| &i.id),
                    fast_embedder_load_ms,
                    quality_embedder_load_ms,
                    install_hint = QUALITY_INSTALL_HINT,
                    "Two-tier search initialized in FAST-ONLY mode; install MiniLM-L6-v2 to enable quality refinement"
                );
            }
            TwoTierAvailability::QualityOnly => {
                tracing::warn!(
                    target: "search.two_tier",
                    quality_model = ?quality_info.as_ref().map(|i| &i.id),
                    quality_embedder_load_ms,
                    "Two-tier search initialized in QUALITY-ONLY mode (fast embedder unavailable)"
                );
            }
            TwoTierAvailability::None => {
                tracing::warn!(
                    target: "search.two_tier",
                    "Two-tier search unavailable; falling back to lexical-only search"
                );
            }
        }

        Self {
            availability,
            config,
            fast_info,
            quality_info,
            init_metrics,
        }
    }

    /// Get the availability status.
    #[must_use]
    pub const fn availability(&self) -> TwoTierAvailability {
        self.availability
    }

    /// Check if two-tier search is available (at least one embedder).
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.availability != TwoTierAvailability::None
    }

    /// Check if full two-tier search is available (both embedders).
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.availability == TwoTierAvailability::Full
    }

    /// Get the configuration.
    #[must_use]
    pub const fn config(&self) -> &TwoTierConfig {
        &self.config
    }

    /// Get fast embedder info (if available).
    #[must_use]
    pub const fn fast_info(&self) -> Option<&EmbedderInfo> {
        self.fast_info.as_ref()
    }

    /// Get quality embedder info (if available).
    #[must_use]
    pub const fn quality_info(&self) -> Option<&EmbedderInfo> {
        self.quality_info.as_ref()
    }

    /// Get initialization timing and availability metrics.
    #[must_use]
    pub const fn init_metrics(&self) -> &TwoTierInitMetrics {
        &self.init_metrics
    }

    /// Create a new `TwoTierIndex` with this context's configuration.
    #[must_use]
    pub fn create_index(&self) -> TwoTierIndex {
        TwoTierIndex::new(&self.config)
    }

    /// Create a frankensearch `EmbedderStack` from the auto-detected embedders.
    ///
    /// This uses `SyncEmbedderAdapter` to bridge search-core's sync embedders
    /// to frankensearch's async `Embedder` trait. Returns `None` if no embedders
    /// are available.
    ///
    /// This is the primary integration point for consumers migrating to
    /// frankensearch — use this stack with `frankensearch::IndexBuilder` or
    /// `frankensearch::TwoTierSearcher`.
    #[must_use]
    pub fn create_fs_embedder_stack(&self) -> Option<FsEmbedderStack> {
        let fast: Arc<dyn frankensearch::Embedder> = if get_fast_embedder().is_some() {
            Arc::new(SyncEmbedderAdapter::fast(Arc::new(FastEmbedderWrapper)))
        } else {
            return None;
        };

        let quality: Option<Arc<dyn frankensearch::Embedder>> = if quality_embedder_available() {
            Some(Arc::new(SyncEmbedderAdapter::quality(Arc::new(
                QualityEmbedderWrapper,
            ))))
        } else {
            None
        };

        Some(FsEmbedderStack::from_parts(fast, quality))
    }

    /// Create a searcher for the given index.
    ///
    /// Returns `None` if no embedders are available.
    #[must_use]
    pub fn create_searcher<'a>(&self, index: &'a TwoTierIndex) -> Option<TwoTierSearcher<'a>> {
        let fast_embedder: Option<Arc<dyn TwoTierEmbedder>> = match get_fast_embedder() {
            Some(_) => Some(Arc::new(FastEmbedderWrapper)),
            None => return None,
        };

        let quality_embedder: Option<Arc<dyn TwoTierEmbedder>> = if quality_embedder_available() {
            Some(Arc::new(QualityEmbedderWrapper))
        } else {
            None
        };

        Some(TwoTierSearcher::new(
            index,
            fast_embedder,
            quality_embedder,
            self.config.clone(),
        ))
    }

    /// Embed a query for fast search.
    ///
    /// # Errors
    ///
    /// Returns an error if the fast embedder is unavailable.
    pub fn embed_fast(&self, query: &str) -> SearchResult<Vec<f32>> {
        get_fast_embedder()
            .ok_or_else(|| {
                crate::search_error::SearchError::ModeUnavailable(
                    "fast embedder unavailable".into(),
                )
            })?
            .embed(query)
    }

    /// Embed a query for quality search.
    ///
    /// # Errors
    ///
    /// Returns an error if the quality embedder is unavailable.
    pub fn embed_quality(&self, query: &str) -> SearchResult<Vec<f32>> {
        embed_quality_query(query)
    }
}

// ────────────────────────────────────────────────────────────────────
// Wrapper types for global embedders
// ────────────────────────────────────────────────────────────────────

/// Wrapper to implement `TwoTierEmbedder` for the global fast embedder.
struct FastEmbedderWrapper;

impl TwoTierEmbedder for FastEmbedderWrapper {
    fn embed(&self, text: &str) -> SearchResult<Vec<f32>> {
        get_fast_embedder()
            .ok_or_else(|| {
                crate::search_error::SearchError::ModeUnavailable(
                    "fast embedder unavailable".into(),
                )
            })?
            .embed(text)
    }

    fn dimension(&self) -> usize {
        get_fast_embedder().map_or(256, Model2VecEmbedder::dimension)
    }

    fn id(&self) -> &str {
        get_fast_embedder().map_or("unavailable", |e| e.id())
    }
}

/// Wrapper to implement `TwoTierEmbedder` for the global quality embedder.
struct QualityEmbedderWrapper;

impl TwoTierEmbedder for QualityEmbedderWrapper {
    fn embed(&self, text: &str) -> SearchResult<Vec<f32>> {
        embed_quality_query(text)
    }

    fn dimension(&self) -> usize {
        quality_embedder_dimension()
    }

    fn id(&self) -> &str {
        quality_embedder_id()
    }
}

// ────────────────────────────────────────────────────────────────────
// Global context singleton
// ────────────────────────────────────────────────────────────────────

/// Global two-tier search context.
static CONTEXT: OnceLock<TwoTierContext> = OnceLock::new();
static INIT_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

fn next_init_attempt() -> u32 {
    let mut current = INIT_ATTEMPTS.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(1);
        match INIT_ATTEMPTS.compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

/// Get the global two-tier search context.
///
/// Auto-initializes on first call. Thread-safe.
#[must_use]
pub fn get_two_tier_context() -> &'static TwoTierContext {
    CONTEXT.get_or_init(TwoTierContext::init)
}

/// Check if two-tier search is available.
///
/// This is a convenience function that checks if at least one
/// embedder is available.
#[must_use]
pub fn is_two_tier_available() -> bool {
    get_two_tier_context().is_available()
}

/// Check if full two-tier search is available.
///
/// This checks if both fast and quality embedders are available.
#[must_use]
pub fn is_full_two_tier_available() -> bool {
    get_two_tier_context().is_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_availability_display() {
        assert_eq!(
            TwoTierAvailability::Full.to_string(),
            "full (fast + quality)"
        );
        assert_eq!(TwoTierAvailability::FastOnly.to_string(), "fast-only");
        assert_eq!(TwoTierAvailability::QualityOnly.to_string(), "quality-only");
        assert_eq!(TwoTierAvailability::None.to_string(), "unavailable");
    }

    #[test]
    fn test_context_defaults() {
        // This test may vary depending on available models
        let ctx = get_two_tier_context();
        // Just verify it doesn't panic
        let _ = ctx.availability();
        let _ = ctx.config();
        let _ = ctx.is_available();
    }

    // ── TwoTierAvailability trait coverage ──

    #[test]
    fn availability_debug_all_variants() {
        let variants = [
            TwoTierAvailability::Full,
            TwoTierAvailability::FastOnly,
            TwoTierAvailability::QualityOnly,
            TwoTierAvailability::None,
        ];
        for v in &variants {
            let debug = format!("{v:?}");
            assert!(!debug.is_empty());
        }
    }

    #[test]
    fn availability_clone_copy() {
        fn assert_clone<T: Clone>(_: &T) {}
        let a = TwoTierAvailability::Full;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_clone(&a);
    }

    #[test]
    fn availability_eq_same_variants() {
        assert_eq!(TwoTierAvailability::Full, TwoTierAvailability::Full);
        assert_eq!(TwoTierAvailability::FastOnly, TwoTierAvailability::FastOnly);
        assert_eq!(
            TwoTierAvailability::QualityOnly,
            TwoTierAvailability::QualityOnly
        );
        assert_eq!(TwoTierAvailability::None, TwoTierAvailability::None);
    }

    #[test]
    fn availability_ne_different_variants() {
        assert_ne!(TwoTierAvailability::Full, TwoTierAvailability::None);
        assert_ne!(TwoTierAvailability::FastOnly, TwoTierAvailability::Full);
        assert_ne!(
            TwoTierAvailability::QualityOnly,
            TwoTierAvailability::FastOnly
        );
    }

    #[test]
    fn availability_four_distinct_variants() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(format!("{:?}", TwoTierAvailability::Full));
        set.insert(format!("{:?}", TwoTierAvailability::FastOnly));
        set.insert(format!("{:?}", TwoTierAvailability::QualityOnly));
        set.insert(format!("{:?}", TwoTierAvailability::None));
        assert_eq!(set.len(), 4);
    }

    // ── EmbedderInfo ──

    #[test]
    fn embedder_info_debug() {
        let info = EmbedderInfo {
            id: "test-model".to_string(),
            dimension: 256,
        };
        let debug = format!("{info:?}");
        assert!(debug.contains("test-model"));
        assert!(debug.contains("256"));
    }

    #[test]
    fn embedder_info_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let info = EmbedderInfo {
            id: "model-a".to_string(),
            dimension: 384,
        };
        assert_clone(&info);
        assert_eq!(info.id, "model-a");
        assert_eq!(info.dimension, 384);
    }

    // ── TwoTierContext accessors via global singleton ──

    #[test]
    fn context_is_full_consistency() {
        let ctx = get_two_tier_context();
        // is_full should equal (availability == Full)
        assert_eq!(
            ctx.is_full(),
            ctx.availability() == TwoTierAvailability::Full
        );
    }

    #[test]
    fn context_is_available_consistency() {
        let ctx = get_two_tier_context();
        // is_available should equal (availability != None)
        assert_eq!(
            ctx.is_available(),
            ctx.availability() != TwoTierAvailability::None
        );
    }

    #[test]
    fn context_config_dimensions() {
        let ctx = get_two_tier_context();
        let config = ctx.config();
        // Dimensions should be reasonable values
        assert!(config.fast_dimension > 0);
        assert!(config.quality_dimension > 0);
    }

    #[test]
    fn context_create_index() {
        let ctx = get_two_tier_context();
        let index = ctx.create_index();
        // Just verify it doesn't panic and returns a valid index
        let _ = format!("{index:?}");
    }

    #[test]
    fn context_debug() {
        let ctx = get_two_tier_context();
        let debug = format!("{ctx:?}");
        assert!(debug.contains("TwoTierContext"));
    }

    #[test]
    fn context_init_metrics_populated() {
        let ctx = get_two_tier_context();
        let metrics = ctx.init_metrics();
        assert!(metrics.init_attempts >= 1);
        assert!(metrics.init_timestamp > 0);
    }

    // ── Convenience functions ──

    #[test]
    fn is_two_tier_available_matches_context() {
        let ctx = get_two_tier_context();
        assert_eq!(is_two_tier_available(), ctx.is_available());
    }

    #[test]
    fn is_full_two_tier_available_matches_context() {
        let ctx = get_two_tier_context();
        assert_eq!(is_full_two_tier_available(), ctx.is_full());
    }

    // ── Embedder info accessors ──

    #[test]
    fn context_fast_info_matches_availability() {
        let ctx = get_two_tier_context();
        match ctx.availability() {
            TwoTierAvailability::Full | TwoTierAvailability::FastOnly => {
                assert!(ctx.fast_info().is_some());
            }
            TwoTierAvailability::QualityOnly | TwoTierAvailability::None => {
                assert!(ctx.fast_info().is_none());
            }
        }
    }

    #[test]
    fn context_quality_info_matches_availability() {
        let ctx = get_two_tier_context();
        match ctx.availability() {
            TwoTierAvailability::Full | TwoTierAvailability::QualityOnly => {
                assert!(ctx.quality_info().is_some());
            }
            TwoTierAvailability::FastOnly | TwoTierAvailability::None => {
                assert!(ctx.quality_info().is_none());
            }
        }
    }
}
