# ADR-003: Search V3 Architecture — Tantivy + Semantic + Hybrid

**Status:** Proposed
**Date:** 2026-02-12
**Authors:** RubyPrairie (claude-code/opus-4.6)
**Bead:** br-2tnl.1.2
**Reference:** docs/search-v3-component-mapping.md (T1.1 dossier)

## Context

Agent Mail search currently uses SQLite FTS5 with BM25 ranking, exposed through a
sophisticated planner/service/scope stack (~13,000 lines in `mcp-agent-mail-db`).
This works well for basic lexical matching but has three scaling limitations:

1. **Relevance quality** — FTS5 BM25 lacks semantic understanding. Searching for
   "deployment issues" won't find messages about "rollout problems."
2. **Advanced ranking** — No support for cross-encoder reranking or hybrid
   lexical+semantic fusion.
3. **Index lifecycle** — FTS5 triggers fire synchronously on every INSERT/UPDATE,
   creating write amplification on high-frequency fields like `last_active_ts`.

Two sister repos (`/dp/coding_agent_session_search` and `/dp/xf`) have
production-grade search stacks built on Tantivy, vector indexes, and ONNX
rerankers. The T1.1 dossier (docs/search-v3-component-mapping.md) maps
their components to Agent Mail requirements.

## Decision

### D1: New `mcp-agent-mail-search-core` Crate

Create a new crate that owns all retrieval logic (Tantivy, vectors, fusion,
reranking) behind trait abstractions. This crate does NOT depend on
`mcp-agent-mail-db` — it communicates via traits.

```
mcp-agent-mail-search-core
├── traits: SearchEngine, IndexLifecycle, DocumentSource, SearchEmbedder, SearchReranker
├── query: SearchQuery, SearchResults, SearchHit, SearchMode
├── tantivy: TantivyEngine (implements SearchEngine + IndexLifecycle)
├── vector: VectorIndex (mmap F16 + SIMD)
├── fusion: rrf_fuse()
├── embedder: HashEmbedder, OrtEmbedder (feature-gated)
└── reranker: FlashRankReranker (feature-gated)
```

**Rationale**: Separation of concerns. The search-core crate is a pure retrieval
engine. It doesn't know about Agent Mail's permission model, scope enforcement,
or database schema. This makes it independently testable and potentially reusable.

### D2: Trait-Based Integration

```rust
/// Retrieval engine abstraction
pub trait SearchEngine: Send + Sync {
    fn search(&self, query: &SearchQuery) -> Result<SearchResults, SearchError>;
}

/// Index maintenance
pub trait IndexLifecycle: Send + Sync {
    fn rebuild(&self) -> Result<IndexStats, SearchError>;
    fn update_incremental(&self, changes: &[DocChange]) -> Result<usize, SearchError>;
    fn health(&self) -> IndexHealth;
}

/// Document source for indexing (implemented by mcp-agent-mail-db)
pub trait DocumentSource: Send + Sync {
    fn fetch_batch(&self, since_id: i64, limit: usize) -> Result<Vec<SearchDocument>, SearchError>;
    fn total_count(&self) -> Result<u64, SearchError>;
}
```

`mcp-agent-mail-db` implements `DocumentSource` for the messages table.
`mcp-agent-mail-tools` uses `SearchEngine` via the search service layer.
The existing search planner gains a new `PlanMethod::Tantivy` variant.

### D3: Feature Flags for Optional Components

```toml
[features]
default = ["tantivy"]
tantivy = ["dep:tantivy"]
semantic = ["tantivy", "dep:half", "dep:wide", "dep:memmap2"]
rerank = ["semantic", "dep:ort"]
hybrid = ["semantic"]
```

**Rationale**: Hash embedder (zero deps) is always available. Tantivy is
default. Semantic search, reranking, and hybrid fusion are opt-in to avoid
pulling heavy ONNX dependencies for minimal deployments.

### D4: Sync-Only Search Pipeline

All search operations are synchronous:

```
Query → SearchEngine::search()  [sync, Tantivy]
     → VectorIndex::search()    [sync, SIMD dot product]
     → rrf_fuse()               [sync, pure compute]
     → Reranker::rerank()       [sync, ONNX session]
```

**Rationale**: Tantivy, vector search, RRF, and ONNX inference are all
inherently synchronous. The CASS/XF daemon architecture exists to warm models
across processes — Agent Mail loads models once at startup and keeps them in
memory. No tokio/async needed in the search path.

Background indexing (triggered on message send) uses the existing coalesce
queue infrastructure in `mcp-agent-mail-db` to batch Tantivy commits.

### D5: Tantivy Schema for Agent Mail Messages

```rust
// Fields:
message_id:    i64   (INDEXED | STORED | FAST)
project_id:    i64   (INDEXED | STORED | FAST)
product_id:    i64   (INDEXED | STORED | FAST)
subject:       TEXT  (positions + frequencies, stored)
body:          TEXT  (positions + frequencies, stored)
subject_prefix: TEXT (basic indexing, for edge n-gram prefix search)
sender_name:   STRING (stored, exact-match filter)
thread_id:     STRING (stored + FAST, exact-match filter)
importance:    STRING (stored, exact-match filter)
created_ts:    i64   (INDEXED | STORED | FAST, microseconds)
ack_required:  u64   (INDEXED | STORED)
```

Custom tokenizer `am_normalize`: `SimpleTokenizer` → `LowerCaser` →
`RemoveLongFilter(256)`. Handles hyphenated tokens (e.g., `br-2tnl`)
correctly without splitting.

Edge n-grams (2-21 chars) precomputed into `subject_prefix` field for
typeahead/prefix search.

Schema versioned via `schema_hash.json` file alongside the index directory.
Schema mismatch triggers automatic full rebuild.

### D6: Vector Index Format (AMVI)

Simplified binary format derived from XF's XFVI:

```
Header (24 bytes):
  Magic "AMVI", Version u16, Dimension u32, Count u64, EmbedderIdLen u16, EmbedderId

Records (12 bytes each, sorted by message_id):
  message_id i64, vec_offset u32

Vector slab (32-byte aligned):
  Count × Dimension × 2 bytes (F16)
```

Memory-mapped via `memmap2`. SIMD dot product via `wide::f32x8`.
No HNSW — brute-force search is fast enough for <100k messages (~10ms).
HNSW becomes a future optimization when corpora exceed 100k.

### D7: Shadow Validation Before FTS5 Decommission

During the transition period:
1. Both FTS5 and Tantivy run in parallel
2. Results are compared for parity (NDCG, MRR, overlap metrics)
3. FTS5 remains the serving path until Tantivy passes quality gates
4. After validation, FTS5 triggers are removed (schema migration)

**Kill switch**: `SEARCH_ENGINE=fts5` env var forces FTS5 path regardless
of Tantivy availability. Default progresses: `fts5` → `tantivy` → `hybrid`.

### D8: Scope Enforcement Is Post-Retrieval

The existing scope system (`search_scope.rs`) applies visibility verdicts
**after** retrieval, regardless of engine. This means:

- Tantivy returns raw results (message_ids + scores)
- Search service hydrates from SQLite (metadata, body, sender)
- Scope enforcement filters/redacts based on viewer identity

**No scope logic in search-core**. The crate has no concept of permissions,
contacts, or policies.

## Consequences

### Positive
- Clean crate boundary: search-core is independently testable
- Feature flags prevent dependency bloat for minimal deployments
- Sync pipeline avoids async complexity and tokio dependency
- Shadow validation prevents regression during engine transition
- Scope enforcement unchanged — no security review needed

### Negative
- Additional crate to maintain (search-core)
- Tantivy index is separate from SQLite — two storage systems to coordinate
- Feature flags add build matrix complexity

### Neutral
- FTS5 remains during shadow validation — some code duplication is temporary
- Vector index adds ~8-76 MB disk per project (10k-100k messages)

## Crate Dependency Graph (After)

```
mcp-agent-mail-core
  ├─ mcp-agent-mail-db         (SQLite, FTS5 fallback)
  ├─ mcp-agent-mail-search-core (Tantivy, vectors, fusion)  ← NEW
  ├─ mcp-agent-mail-storage    (Git archive)
  ├─ mcp-agent-mail-guard      (pre-commit guard)
  ├─ mcp-agent-mail-share      (export)
  └─ mcp-agent-mail-tools      (uses db + search-core via traits)
       └─ mcp-agent-mail-server (HTTP/MCP/TUI)
```

`mcp-agent-mail-tools` depends on both `mcp-agent-mail-db` (for DocumentSource impl
and scope queries) and `mcp-agent-mail-search-core` (for SearchEngine).

## Migration Path

| Phase | Engine | Config | Duration |
|-------|--------|--------|----------|
| 1. Current | FTS5 only | `SEARCH_ENGINE=fts5` (default) | Now |
| 2. Shadow | FTS5 serving, Tantivy validation | `SEARCH_ENGINE=shadow` | ~2 weeks |
| 3. Tantivy | Tantivy serving, FTS5 removed | `SEARCH_ENGINE=tantivy` | After quality gates |
| 4. Hybrid | Tantivy + semantic | `SEARCH_ENGINE=hybrid` | After semantic validation |
