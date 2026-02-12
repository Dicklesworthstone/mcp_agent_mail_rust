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

use std::sync::{Arc, OnceLock};

use crate::error::SearchResult;
use crate::fastembed::{FastEmbedEmbedder, get_quality_embedder};
use crate::model2vec::{Model2VecEmbedder, get_fast_embedder};
use crate::two_tier::{TwoTierConfig, TwoTierEmbedder, TwoTierIndex, TwoTierSearcher};

/// Availability status for two-tier search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Basic embedder information.
#[derive(Debug, Clone)]
pub struct EmbedderInfo {
    /// Embedder ID.
    pub id: String,
    /// Output dimension.
    pub dimension: usize,
}

impl TwoTierContext {
    /// Initialize the context, detecting available embedders.
    fn init() -> Self {
        let has_fast = get_fast_embedder().is_some();
        let has_quality = get_quality_embedder().is_some();

        let availability = match (has_fast, has_quality) {
            (true, true) => TwoTierAvailability::Full,
            (true, false) => TwoTierAvailability::FastOnly,
            (false, true) => TwoTierAvailability::QualityOnly,
            (false, false) => TwoTierAvailability::None,
        };

        let fast_info = get_fast_embedder().map(|e| EmbedderInfo {
            id: e.id().to_string(),
            dimension: e.dimension(),
        });

        let quality_info = get_quality_embedder().map(|e| EmbedderInfo {
            id: e.id().to_string(),
            dimension: e.dimension(),
        });

        // Adjust config based on available embedders
        let config = TwoTierConfig {
            fast_dimension: fast_info.as_ref().map_or(256, |i| i.dimension),
            quality_dimension: quality_info.as_ref().map_or(384, |i| i.dimension),
            ..TwoTierConfig::default()
        };

        tracing::info!(
            availability = %availability,
            fast = ?fast_info.as_ref().map(|i| &i.id),
            quality = ?quality_info.as_ref().map(|i| &i.id),
            "Two-tier search context initialized"
        );

        Self {
            availability,
            config,
            fast_info,
            quality_info,
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
    pub fn fast_info(&self) -> Option<&EmbedderInfo> {
        self.fast_info.as_ref()
    }

    /// Get quality embedder info (if available).
    #[must_use]
    pub fn quality_info(&self) -> Option<&EmbedderInfo> {
        self.quality_info.as_ref()
    }

    /// Create a new `TwoTierIndex` with this context's configuration.
    #[must_use]
    pub fn create_index(&self) -> TwoTierIndex {
        TwoTierIndex::new(&self.config)
    }

    /// Create a searcher for the given index.
    ///
    /// Returns `None` if no embedders are available.
    #[must_use]
    pub fn create_searcher<'a>(&self, index: &'a TwoTierIndex) -> Option<TwoTierSearcher<'a>> {
        let fast_embedder: Arc<dyn TwoTierEmbedder> = match get_fast_embedder() {
            Some(_) => Arc::new(FastEmbedderWrapper),
            None => return None,
        };

        let quality_embedder: Option<Arc<dyn TwoTierEmbedder>> =
            if get_quality_embedder().is_some() {
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
                crate::error::SearchError::ModeUnavailable("fast embedder unavailable".into())
            })?
            .embed(query)
    }

    /// Embed a query for quality search.
    ///
    /// # Errors
    ///
    /// Returns an error if the quality embedder is unavailable.
    pub fn embed_quality(&self, query: &str) -> SearchResult<Vec<f32>> {
        get_quality_embedder()
            .ok_or_else(|| {
                crate::error::SearchError::ModeUnavailable("quality embedder unavailable".into())
            })?
            .embed(query)
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
                crate::error::SearchError::ModeUnavailable("fast embedder unavailable".into())
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
        get_quality_embedder()
            .ok_or_else(|| {
                crate::error::SearchError::ModeUnavailable("quality embedder unavailable".into())
            })?
            .embed(text)
    }

    fn dimension(&self) -> usize {
        get_quality_embedder().map_or(384, FastEmbedEmbedder::dimension)
    }

    fn id(&self) -> &str {
        get_quality_embedder().map_or("unavailable", |e| e.id())
    }
}

// ────────────────────────────────────────────────────────────────────
// Global context singleton
// ────────────────────────────────────────────────────────────────────

/// Global two-tier search context.
static CONTEXT: OnceLock<TwoTierContext> = OnceLock::new();

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
        assert_eq!(TwoTierAvailability::Full.to_string(), "full (fast + quality)");
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
}
