# Spec: Search V3 Query / Filter / Explain Contract

**Bead:** br-2tnl.1.3
**Track:** br-2tnl.1
**Date:** 2026-02-11
**Status:** Proposed
**Reference:** docs/ADR-003-search-v3-architecture.md, docs/search-v3-component-mapping.md

## Scope

Freeze the Search V3 query contract for MCP tools, TUI, and CLI surfaces.
This is the controlling type-level specification for `mcp-agent-mail-search-core`
and the upgraded `search_planner` / `search_service` pipeline.

This spec defines:

1. Search modes (lexical | semantic | hybrid | hybrid_rerank)
2. Filters (all facets, including new V3 additions)
3. Pagination semantics and stable sorting rules
4. Explain payload schema (extended for V3 engines)
5. Validation and normalization rules

## Design Principles

- **Backward compatible**: Every existing `SearchQuery` field and MCP tool parameter
  continues to work identically. New fields are `Option<T>` with defaults that
  reproduce V2 behavior.
- **Engine-transparent**: Callers specify *what* they want (mode, filters, ranking),
  not *how* to get it. The planner selects FTS5, Tantivy, or hybrid based on mode
  and feature availability.
- **Deterministic**: Given the same query and data, results are identical across
  restarts, replicas, and engine versions (within floating-point tolerance for scores).
- **Explainable**: Every query can optionally return its full execution plan,
  including engine selection, scoring, timing, and scope enforcement.

## 1. Search Modes

### 1.1 `SearchMode` enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Lexical full-text search (BM25).
    /// V2: uses FTS5. V3: uses Tantivy when available, FTS5 fallback.
    #[default]
    Lexical,

    /// Semantic vector search (embedding similarity).
    /// Requires `semantic` feature flag. Falls back to Lexical if unavailable.
    Semantic,

    /// Hybrid lexical + semantic with RRF fusion.
    /// Requires `semantic` feature flag. Falls back to Lexical if unavailable.
    Hybrid,

    /// Hybrid + cross-encoder reranking on the fused top-K.
    /// Requires `rerank` feature flag. Falls back to Hybrid if unavailable.
    HybridRerank,
}
```

### 1.2 Fallback chain

When a mode's required feature is unavailable, the engine degrades gracefully:

```
HybridRerank → Hybrid → Lexical (Tantivy) → Lexical (FTS5)
Semantic     → Lexical (Tantivy) → Lexical (FTS5)
Hybrid       → Lexical (Tantivy) → Lexical (FTS5)
Lexical      → Tantivy if available, else FTS5
```

The explain payload records the *requested* mode and the *actual* mode used.

### 1.3 Kill switch

`SEARCH_ENGINE` env var overrides mode selection:

| Value | Behavior |
|-------|----------|
| `fts5` | Force FTS5 for all lexical queries. Semantic/hybrid degrade to FTS5. |
| `tantivy` | Force Tantivy for lexical. Semantic/hybrid available if feature-gated. |
| `shadow` | Run both FTS5 and Tantivy, serve FTS5, log comparison metrics. |
| `hybrid` | Full hybrid pipeline (default after quality gates pass). |
| unset | Auto-select based on feature availability. |

## 2. SearchQuery (V3 Extension)

All existing fields are preserved unchanged. New fields are additive.

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    // ── Existing V2 fields (unchanged) ─────────────────────────────

    /// Free-text query string (sanitized for the active engine).
    pub text: String,

    /// Entity kind to search. Default: Message.
    #[serde(default)]
    pub doc_kind: DocKind,

    /// Restrict to a single project.
    pub project_id: Option<i64>,

    /// Search across all projects linked to a product.
    pub product_id: Option<i64>,

    /// Filter by importance levels.
    #[serde(default)]
    pub importance: Vec<Importance>,

    /// Filter by message direction (requires agent_name).
    pub direction: Option<Direction>,

    /// Filter by agent name (sender for outbox, recipient for inbox).
    pub agent_name: Option<String>,

    /// Filter by thread ID.
    pub thread_id: Option<String>,

    /// Filter by ack_required flag.
    pub ack_required: Option<bool>,

    /// Filter by creation time range (inclusive, microsecond timestamps).
    #[serde(default)]
    pub time_range: TimeRange,

    /// How to rank results. Default: Relevance (BM25).
    #[serde(default)]
    pub ranking: RankingMode,

    /// Maximum results to return (clamped to 1..=1000). Default: 50.
    pub limit: Option<usize>,

    /// Cursor for stable pagination (opaque token from previous result).
    pub cursor: Option<String>,

    /// Whether to include explain metadata in results.
    #[serde(default)]
    pub explain: bool,

    /// Scope policy controlling result visibility.
    #[serde(default)]
    pub scope: ScopePolicy,

    /// Redaction configuration for restricted results.
    pub redaction: Option<RedactionConfig>,

    // ── NEW V3 fields ──────────────────────────────────────────────

    /// Search mode. Default: Lexical (backward compatible with V2).
    #[serde(default)]
    pub mode: SearchMode,

    /// Filter by sender agent name (exact match).
    /// Distinct from `agent_name` which is direction-dependent.
    pub sender_name: Option<String>,

    /// Filter by recipient agent name (in to/cc/bcc).
    pub recipient_name: Option<String>,

    /// Minimum score threshold. Results below this are excluded.
    /// Interpretation depends on mode:
    ///   Lexical: BM25 score (lower = more relevant for FTS5;
    ///            higher = more relevant for Tantivy)
    ///   Semantic: cosine similarity (0.0..1.0)
    ///   Hybrid: RRF fused score (0.0..1.0)
    pub min_score: Option<f64>,

    /// Number of candidates to fetch before reranking.
    /// Only used when mode = HybridRerank. Default: 100.
    /// Clamped to 10..=1000.
    pub rerank_top_k: Option<usize>,

    /// RRF fusion parameter k. Default: 60.
    /// Only used when mode = Hybrid or HybridRerank.
    /// Clamped to 1..=1000.
    pub rrf_k: Option<usize>,
}
```

### 2.1 Effective defaults

| Field | Default | Notes |
|-------|---------|-------|
| `mode` | `Lexical` | Identical to V2 behavior |
| `doc_kind` | `Message` | Unchanged |
| `ranking` | `Relevance` | Unchanged |
| `limit` | 50 | Clamped 1..=1000 |
| `rerank_top_k` | 100 | Clamped 10..=1000 |
| `rrf_k` | 60 | Standard RRF constant |
| `min_score` | None | No threshold (return all) |

### 2.2 Validation rules

1. **Text normalization**: Leading/trailing whitespace trimmed. Internal
   whitespace collapsed. Empty text after normalization with no filters → Empty plan.
2. **FTS sanitization**: For lexical mode, `sanitize_fts_query()` applied. On
   FTS syntax error, LIKE fallback. For Tantivy mode, Tantivy's query parser
   handles syntax errors by treating the query as a phrase.
3. **Mode downgrade**: If requested mode's feature is unavailable, downgrade per
   fallback chain (section 1.2). Never error — always serve results.
4. **Limit clamping**: `effective_limit() = limit.unwrap_or(50).clamp(1, 1000)`.
5. **rerank_top_k clamping**: `effective_rerank_top_k() = rerank_top_k.unwrap_or(100).clamp(10, 1000)`.
6. **rrf_k clamping**: `effective_rrf_k() = rrf_k.unwrap_or(60).clamp(1, 1000)`.
7. **Conflicting filters**: `sender_name` and `recipient_name` can be used together
   (AND semantics). If `direction` + `agent_name` is also set, all are AND'd.
8. **Doc kind restrictions**: `sender_name`, `recipient_name`, `direction`,
   `importance`, `ack_required`, `thread_id`, `time_range` are message-only facets.
   They are silently ignored for Agent and Project doc kinds.

## 3. Ranking and Scoring

### 3.1 RankingMode (extended)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RankingMode {
    /// Relevance ranking (BM25 for lexical, cosine for semantic, RRF for hybrid).
    #[default]
    Relevance,

    /// Most recent first (created_ts DESC).
    Recency,
}
```

No change from V2. The `Relevance` variant adapts its semantics based on `SearchMode`.

### 3.2 Score semantics by engine

| Engine | Score direction | Score range | Meaning |
|--------|----------------|-------------|---------|
| FTS5 BM25 | Lower = better | Negative reals | SQLite `bm25()` convention |
| Tantivy BM25 | Higher = better | 0..+inf | Standard BM25 |
| Vector cosine | Higher = better | 0.0..1.0 | Normalized dot product |
| RRF fused | Higher = better | 0.0..1.0 | `sum(1/(k+rank))` normalized |
| Reranked | Higher = better | 0.0..1.0 | Cross-encoder logit → sigmoid |

**Score normalization contract**: The search service normalizes all scores to
**higher = better, range [0.0, 1.0]** before returning to callers. This means:

- FTS5 BM25 scores are negated and min-max normalized within the result set.
- Tantivy BM25 scores are min-max normalized within the result set.
- Vector, RRF, and reranker scores are already in [0.0, 1.0].
- The `explain` payload includes `raw_score` (engine-native) alongside normalized `score`.

### 3.3 Stable sort order

For deterministic pagination, the final sort order is always:

```
ORDER BY normalized_score DESC, id ASC
```

Ties in normalized score (within f64 epsilon) are broken by ascending document ID,
ensuring a total, stable ordering.

## 4. Pagination

### 4.1 SearchCursor (V3)

The cursor format extends the V2 format to support multi-engine pagination:

```rust
pub struct SearchCursor {
    /// Normalized score of the last-seen result.
    pub score: f64,
    /// Document ID of the last-seen result.
    pub id: i64,
    /// Engine that produced this cursor (for consistency across pages).
    pub engine: Option<CursorEngine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorEngine {
    Fts5,
    Tantivy,
    Hybrid,
}
```

**Wire format**: `s<score_hex>:i<id>[:e<engine>]`

- V2 tokens (`s<hex>:i<id>`) are parsed as `engine = None` (backward compatible).
- V3 tokens include the `:e<tag>` suffix.
- A cursor produced by engine X must be consumed by engine X. If the engine changes
  between pages (unlikely but possible during shadow mode), the cursor is invalidated
  and the query restarts from page 1 with an explain note.

### 4.2 Pagination rules

1. **Page size**: Determined by `effective_limit()`.
2. **Next cursor**: Present if `results.len() == limit` (may have more pages).
3. **Absent cursor**: Fewer results than limit → last page.
4. **Cursor invalidation**: If the cursor's engine doesn't match the current engine,
   the cursor is silently discarded and the query starts from page 1. The explain
   payload notes `cursor_invalidated: true`.
5. **Deleted documents**: If a document referenced in the cursor has been deleted,
   the cursor skips forward to the next valid position.

## 5. Explain Payload (V3)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryExplain {
    // ── Existing V2 fields (unchanged) ─────────────────────────────

    /// The plan method chosen (string representation).
    pub method: String,

    /// The normalized/sanitized query (or None if LIKE fallback).
    pub normalized_query: Option<String>,

    /// Whether LIKE fallback was used.
    pub used_like_fallback: bool,

    /// Number of active facet filters.
    pub facet_count: usize,

    /// Which facets were applied.
    pub facets_applied: Vec<String>,

    /// The raw SQL executed (for FTS5/LIKE paths).
    pub sql: String,

    /// Scope policy that was applied.
    #[serde(default = "default_scope_label")]
    pub scope_policy: String,

    /// How many results were denied by visibility rules.
    #[serde(default)]
    pub denied_count: usize,

    /// How many results were redacted.
    #[serde(default)]
    pub redacted_count: usize,

    // ── NEW V3 fields ──────────────────────────────────────────────

    /// Requested search mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_mode: Option<String>,

    /// Actual search mode used (after fallback chain).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_mode: Option<String>,

    /// Engine that served the query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,

    /// Whether a mode downgrade occurred.
    #[serde(default)]
    pub mode_downgraded: bool,

    /// Reason for mode downgrade (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,

    /// Timing breakdown in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<ExplainTiming>,

    /// Whether the cursor was invalidated (engine mismatch).
    #[serde(default)]
    pub cursor_invalidated: bool,

    /// Shadow mode comparison results (only when SEARCH_ENGINE=shadow).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow: Option<ShadowComparison>,
}

/// Timing breakdown for query execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainTiming {
    /// Total query time in microseconds.
    pub total_us: u64,
    /// Time spent in lexical search (FTS5 or Tantivy).
    pub lexical_us: Option<u64>,
    /// Time spent in vector search.
    pub vector_us: Option<u64>,
    /// Time spent in RRF fusion.
    pub fusion_us: Option<u64>,
    /// Time spent in reranking.
    pub rerank_us: Option<u64>,
    /// Time spent in scope enforcement.
    pub scope_us: Option<u64>,
}

/// Shadow-mode comparison between FTS5 and Tantivy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowComparison {
    /// Which engine served the result.
    pub serving_engine: String,
    /// Which engine was the shadow.
    pub shadow_engine: String,
    /// Overlap between result sets (Jaccard coefficient, 0.0..1.0).
    pub overlap_jaccard: f64,
    /// NDCG@10 of shadow results against serving results.
    pub ndcg_at_10: f64,
    /// Mean Reciprocal Rank difference.
    pub mrr_delta: f64,
    /// Number of results unique to serving engine.
    pub serving_only: usize,
    /// Number of results unique to shadow engine.
    pub shadow_only: usize,
}
```

## 6. MCP Tool Interface

### 6.1 `search_messages` (current contract)

`search_messages` is the canonical MCP surface and now supports rich filters:

```text
search_messages(
    project_key: str,
    query: str,
    limit?: int,                # default 20, max 1000
    offset?: int,               # default 0
    ranking?: str,              # "relevance" | "recency"
    sender?: str,               # aliases: from_agent, sender_name
    importance?: str,           # comma-separated: low,normal,high,urgent
    thread_id?: str,
    date_start?: str,           # aliases: date_from, after, since
    date_end?: str,             # aliases: date_to, before, until
    explain?: bool
) -> {
    result: [{id, subject, importance, ack_required, created_ts, thread_id, from, reason_codes?, score_factors?}],
    assistance?: QueryAssistance,
    guidance?: ZeroResultGuidance,
    explain?: QueryExplain,     # only when explain=true
    next_cursor?: str,
    diagnostics?: SearchDiagnostics  # fallback/budget/timeout signals when degraded
}
```

Date-only bounds are normalized in UTC and `date_end` is inclusive through end-of-day.

### 6.2 `search_messages_product` (current contract)

`search_messages_product` supports product-wide search plus project/date/sender aliases:

```text
search_messages_product(
    product_key: str,
    query: str,
    limit?: int,
    project?: str,              # aliases: project_key_filter, project_slug, proj
    sender?: str,               # aliases: from_agent, sender_name
    importance?: str,
    thread_id?: str,
    date_start?: str,           # aliases: date_from, after, since
    date_end?: str,             # aliases: date_to, before, until
) -> {
    result: [{id, subject, importance, ack_required, created_ts, thread_id, from, project_id}],
    assistance?: QueryAssistance,
    diagnostics?: SearchDiagnostics
}
```

### 6.3 Notes

- Degraded-mode diagnostics are deterministic and intended for automation branching.
- `explain` is optional for callers; diagnostics may still be emitted when degradation signals are detected.
- TUI Search mode guidance:
  - `Auto` / `Lexical`: default deterministic path for low-latency operator triage.
  - `Semantic` / `Hybrid`: when unavailable in the active sync path, UI must surface degraded-mode hints instead of silently pretending success.

## 7. Filters Summary

### 7.1 Complete filter catalog

| Filter | V2 | V3 | Type | DocKind | Behavior |
|--------|----|----|------|---------|----------|
| `project_id` | Y | Y | `i64` | All | Exact match |
| `product_id` | Y | Y | `i64` | Message | Cross-project via product links |
| `importance` | Y | Y | `Vec<Importance>` | Message | OR within set |
| `direction` | Y | Y | `Direction` | Message | Requires `agent_name` |
| `agent_name` | Y | Y | `String` | Message | Direction-dependent sender/recipient |
| `thread_id` | Y | Y | `String` | Message | Exact match |
| `ack_required` | Y | Y | `bool` | Message | Exact match |
| `time_range` | Y | Y | `TimeRange` | Message | Inclusive bounds (microseconds) |
| `sender_name` | N | Y | `String` | Message | Exact match on sender |
| `recipient_name` | N | Y | `String` | Message | Match in to/cc/bcc |
| `min_score` | N | Y | `f64` | All | Post-normalization threshold |

### 7.2 Filter interaction rules

- All filters are AND'd together.
- `importance` values within the Vec are OR'd (match any).
- `project_id` and `product_id` are mutually exclusive; if both are set,
  `project_id` takes precedence.
- `sender_name` and `agent_name + direction=Outbox` are equivalent but
  `sender_name` is preferred in V3 for clarity.
- Message-only filters are silently ignored for Agent/Project doc kinds.

## 8. Index Lifecycle Events

The search service emits lifecycle events for observability:

| Event | When | Payload |
|-------|------|---------|
| `index.rebuild.start` | Full Tantivy index rebuild begins | `{doc_count, engine}` |
| `index.rebuild.complete` | Full rebuild finishes | `{doc_count, duration_ms, engine}` |
| `index.incremental.batch` | Incremental update batch | `{batch_size, engine}` |
| `index.schema.mismatch` | Schema hash doesn't match | `{expected, actual}` |
| `search.mode.downgrade` | Requested mode unavailable | `{requested, actual, reason}` |
| `search.shadow.comparison` | Shadow mode result comparison | `ShadowComparison` |
| `search.cursor.invalidated` | Cursor engine mismatch | `{cursor_engine, current_engine}` |

Events are logged via the existing query tracking infrastructure (`record_query`).

## 9. Error Handling

### 9.1 Error types

```rust
#[derive(Debug)]
pub enum SearchError {
    /// Query validation failed (empty query with no filters, etc.).
    InvalidQuery(String),
    /// Index is not ready (still building or corrupted).
    IndexNotReady(String),
    /// Engine-specific error (Tantivy, vector, ONNX).
    EngineError(String),
    /// Feature not available (semantic without feature flag, etc.).
    FeatureUnavailable(String),
}
```

### 9.2 Error handling rules

1. **Never return SearchError to MCP callers**. All errors are caught by the
   search service and degraded to a valid (possibly empty) response.
2. `InvalidQuery` → Empty result set + explain noting the validation failure.
3. `IndexNotReady` → Fall back to FTS5 + explain noting the fallback.
4. `EngineError` → Fall back to next engine in chain + explain noting the error.
5. `FeatureUnavailable` → Mode downgrade per fallback chain + explain.

## 10. Implementation Checklist

This spec controls the following implementation beads:

- [ ] **br-2tnl.2.1**: Scaffold `mcp-agent-mail-search-core` crate with trait definitions
- [ ] **br-2tnl.2.2**: Implement `TantivyEngine` with `am_normalize` tokenizer
- [ ] **br-2tnl.2.3**: Implement `VectorIndex` (AMVI format, mmap, SIMD)
- [ ] **br-2tnl.2.4**: Implement `rrf_fuse()` and `HashEmbedder`
- [ ] **br-2tnl.2.5**: Implement `FlashRankReranker` (feature-gated)
- [ ] **br-2tnl.3**: Wire `DocumentSource` into `mcp-agent-mail-db`
- [ ] **br-2tnl.4**: Add `SearchMode` + `PlanMethod::Tantivy` to planner
- [ ] **br-2tnl.5**: Shadow validation harness + metrics
- [ ] **br-2tnl.6**: `search_messages_v3` MCP tool
- [ ] **br-2tnl.7**: Score normalization pipeline
- [ ] **br-2tnl.8**: Quality gate tests (NDCG, MRR, latency benchmarks)

## Non-Goals (Deferred)

- **Thread-level search**: Derived from message search (group by thread_id,
  rank by best message score). Not a separate mode.
- **Snippet/highlight generation**: Tantivy supports it, but not in V3 scope.
- **Facet aggregation counts**: ("5 urgent, 12 normal") — future enhancement.
- **Real-time streaming results**: All queries are batch request/response.
