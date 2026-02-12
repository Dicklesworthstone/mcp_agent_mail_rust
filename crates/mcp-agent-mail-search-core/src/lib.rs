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

pub mod filter_compiler;
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
pub use results::{ExplainReport, HighlightRange, SearchHit, SearchResults};
pub use updater::{IncrementalUpdater, UpdaterConfig, UpdaterStats, deduplicate_changes};

#[cfg(feature = "tantivy-engine")]
pub use filter_compiler::{CompiledFilters, compile_filters};
pub use filter_compiler::{active_filter_count, has_active_filters};
pub use hybrid_candidates::{
    CandidateBudget, CandidateBudgetConfig, CandidateHit, CandidateMode, CandidatePreparation,
    CandidateSource, CandidateStageCounts, PreparedCandidate, QueryClass, prepare_candidates,
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
