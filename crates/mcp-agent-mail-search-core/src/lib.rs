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
pub mod lexical_parser;
pub mod lexical_response;

#[cfg(feature = "tantivy-engine")]
pub mod tantivy_schema;

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

pub use filter_compiler::{active_filter_count, has_active_filters};
#[cfg(feature = "tantivy-engine")]
pub use filter_compiler::{CompiledFilters, compile_filters};
pub use lexical_parser::{SanitizedQuery, extract_terms, sanitize_query};
pub use lexical_response::{find_highlights, generate_snippet};
#[cfg(feature = "tantivy-engine")]
pub use lexical_response::{ResponseConfig, execute_search};
#[cfg(feature = "tantivy-engine")]
pub use lexical_parser::{LexicalParser, LexicalParserConfig, ParseOutcome};
