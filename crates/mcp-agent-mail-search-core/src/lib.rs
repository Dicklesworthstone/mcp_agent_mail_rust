//! Pluggable search engine traits and types for MCP Agent Mail
//!
//! This crate defines the core abstractions for the Search V3 subsystem:
//! - [`SearchEngine`] — the primary search trait (lexical, semantic, or hybrid)
//! - [`IndexLifecycle`] — index creation, rebuild, and incremental update
//! - [`DocumentSource`] — abstract document fetching (DB is one impl)
//! - [`SearchQuery`] / [`SearchResults`] / [`SearchHit`] — query/response models
//!
//! Feature flags control which engine backends are compiled:
//! - `tantivy-engine` — Tantivy-based full-text lexical search
//! - `semantic` — vector embedding search
//! - `hybrid` — two-tier fusion (enables both `tantivy-engine` and `semantic`)

#![forbid(unsafe_code)]

pub mod canonical;
pub mod consistency;
pub mod document;
pub mod engine;
pub mod envelope;
pub mod error;
pub mod index_layout;
pub mod query;
pub mod results;
pub mod updater;

pub mod cache;
pub mod diversity;
pub mod filter_compiler;
pub mod fusion;
pub mod hybrid_candidates;
pub mod lexical_parser;
pub mod lexical_response;
pub mod rollout;

#[cfg(feature = "tantivy-engine")]
pub mod tantivy_schema;

#[cfg(feature = "semantic")]
pub mod embedder;

#[cfg(feature = "semantic")]
pub mod embedding_jobs;

#[cfg(feature = "semantic")]
pub mod vector_index;

#[cfg(feature = "semantic")]
pub mod two_tier;

#[cfg(feature = "semantic")]
pub mod model2vec;

#[cfg(feature = "semantic")]
pub mod fastembed;

#[cfg(feature = "semantic")]
pub mod auto_init;

#[cfg(feature = "semantic")]
pub mod fs_bridge;

// Re-export key types
pub use canonical::{
    CanonPolicy, canonicalize, canonicalize_and_hash, content_hash, strip_markdown,
};
pub use consistency::{
    ConsistencyConfig, ConsistencyFinding, ConsistencyReport, NoProgress, ReindexConfig,
    ReindexProgress, ReindexResult, Severity, check_consistency, full_reindex, repair_if_needed,
};
pub use document::{DocChange, DocId, DocKind, Document};
pub use engine::{DocumentSource, IndexLifecycle, SearchEngine};
pub use envelope::{
    AgentRow, DocVersion, MessageRow, ProjectRow, Provenance, SearchDocumentEnvelope, Visibility,
    agent_to_envelope, message_to_envelope, project_to_envelope,
};
pub use error::{SearchError, SearchResult};
pub use index_layout::{IndexCheckpoint, IndexLayout, IndexScope, SchemaField, SchemaHash};
pub use query::{DateRange, ImportanceFilter, SearchFilter, SearchMode, SearchQuery};
pub use results::{
    ExplainComposerConfig, ExplainReasonCode, ExplainReport, ExplainStage, ExplainVerbosity,
    HighlightRange, HitExplanation, ScoreFactor, SearchHit, SearchResults, StageExplanation,
    StageScoreInput, compose_explain_report, compose_hit_explanation,
};
pub use updater::{IncrementalUpdater, UpdaterConfig, UpdaterStats, deduplicate_changes};

pub use cache::{
    CACHE_MAX_ENTRIES_ENV, CACHE_TTL_SECONDS_ENV, CacheConfig, CacheEntry, CacheInvalidator,
    CacheMetrics, DEFAULT_CACHE_MAX_ENTRIES, DEFAULT_CACHE_TTL_SECONDS, InvalidationEvent,
    InvalidationTrigger, QueryCache, QueryCacheKey, WarmResource, WarmState, WarmStatus,
    WarmWorker, WarmWorkerConfig,
};
pub use diversity::{
    DIVERSITY_ENABLED_ENV, DIVERSITY_MAX_PER_SENDER_ENV, DIVERSITY_MAX_PER_THREAD_ENV,
    DIVERSITY_SCORE_TOLERANCE_ENV, DIVERSITY_WINDOW_SIZE_ENV, DiversityConfig, DiversityMeta,
    DiversityResult, diversify,
};
#[cfg(feature = "tantivy-engine")]
pub use filter_compiler::{CompiledFilters, compile_filters};
pub use filter_compiler::{active_filter_count, has_active_filters};
pub use fusion::{
    DEFAULT_RRF_K, FusedHit, FusionExplain, FusionResult, RRF_K_ENV_VAR, RrfConfig,
    SourceContribution, fuse_rrf, fuse_rrf_default,
};
pub use hybrid_candidates::{
    CandidateActionLoss, CandidateBudget, CandidateBudgetAction, CandidateBudgetConfig,
    CandidateBudgetDecision, CandidateBudgetDerivation, CandidateHit, CandidateMode,
    CandidatePreparation, CandidateSource, CandidateStageCounts, CandidateStatePosterior,
    PreparedCandidate, QueryClass, prepare_candidates,
};
pub use lexical_parser::{
    AppliedFilterHint, DidYouMeanHint, QueryAssistance, SanitizedQuery, extract_terms,
    parse_query_assistance, sanitize_query,
};
#[cfg(feature = "tantivy-engine")]
pub use lexical_parser::{LexicalParser, LexicalParserConfig, ParseOutcome};
#[cfg(feature = "tantivy-engine")]
pub use lexical_response::{ResponseConfig, execute_search};
pub use lexical_response::{find_highlights, generate_snippet};
pub use rollout::{RolloutController, ShadowComparison, ShadowMetrics, ShadowMetricsSnapshot};

#[cfg(feature = "semantic")]
pub use embedder::{
    Embedder, EmbeddingResult, EmbeddingVec, HashEmbedder, ModelInfo, ModelRegistry, ModelTier,
    RegistryConfig, cosine_similarity, embed_document, normalize_l2, well_known,
};

#[cfg(feature = "semantic")]
pub use vector_index::{
    IndexEntry, VectorFilter, VectorHit, VectorIndex, VectorIndexConfig, VectorIndexStats,
    VectorMetadata,
};

#[cfg(feature = "semantic")]
pub use embedding_jobs::{
    BatchResult, EmbeddingJobConfig, EmbeddingJobRunner, EmbeddingQueue, EmbeddingRequest,
    IndexRefreshWorker, JobMetrics, JobMetricsSnapshot, JobResult, NoProgress as JobNoProgress,
    QueueStats, RebuildProgress, RebuildResult, RefreshWorkerConfig,
};

#[cfg(feature = "semantic")]
pub use two_tier::{
    IndexStatus as TwoTierIndexStatus, ScoredResult, SearchPhase, TwoTierConfig, TwoTierEmbedder,
    TwoTierEntry, TwoTierIndex, TwoTierMetadata, TwoTierSearcher, blend_scores,
    dot_product_f16_simd, normalize_scores,
};

#[cfg(feature = "semantic")]
pub use model2vec::{
    MODEL_POTION_32M, MODEL_POTION_128M, Model2VecEmbedder, get_fast_embedder,
    is_fast_embedder_available,
};

#[cfg(feature = "semantic")]
pub use fastembed::{
    FastEmbedEmbedder, MODEL_BGE_SMALL, MODEL_MINILM_L6_V2, get_quality_embedder,
    is_quality_embedder_available,
};

#[cfg(feature = "semantic")]
pub use auto_init::{
    EmbedderInfo as TwoTierEmbedderInfo, TwoTierAvailability, TwoTierContext, get_two_tier_context,
    is_full_two_tier_available, is_two_tier_available,
};

// frankensearch bridge — re-export conversion utilities and frankensearch types
#[cfg(feature = "semantic")]
pub use fs_bridge::{
    // frankensearch types (Fs-prefixed to avoid name collisions)
    FsEmbedder,
    FsEmbedderStack,
    FsIndexBuilder,
    FsModelCategory,
    FsModelInfo,
    FsModelTier,
    FsRrfConfig,
    FsScoredResult,
    FsSearchPhase,
    FsTwoTierAvailability,
    FsTwoTierConfig,
    FsTwoTierIndex,
    FsTwoTierMetrics,
    FsTwoTierSearcher,
    FsVectorHit,
    FsVectorIndex,
    IndexableDocument,
    // Sync-to-async embedder adapter
    SyncEmbedderAdapter,
    // Conversion utilities
    doc_id_from_string,
    doc_id_to_string,
    from_fs_config,
    // Model tier conversion
    from_fs_model_tier,
    from_fs_scored_result,
    from_fs_scored_results,
    // The frankensearch facade crate itself
    fs,
    // Error mapping
    map_fs_error,
    to_fs_config,
    to_fs_model_tier,
    to_fs_scored_result,
};
