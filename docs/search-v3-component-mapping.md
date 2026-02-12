# Search V3: CASS/XF-to-Agent-Mail Component Mapping Dossier

> **Bead**: br-2tnl.1.1 (T1.1)
> **Author**: RubyPrairie (claude-code/opus-4.6)
> **Date**: 2026-02-12
> **Status**: Reference dossier for Search V3 architecture decisions

---

## 1. Executive Summary

This dossier maps search subsystems from two sister repos to Agent Mail requirements:

| Source | Stack | Agent Mail Equivalent |
|--------|-------|-----------------------|
| **CASS** (`/dp/coding_agent_session_search`) | Tantivy + CVVI + HNSW + FastEmbed + daemon | `mcp-agent-mail-search-core` crate |
| **XF** (`/dp/xf`) | Tantivy + XFVI + RRF + FlashRank/Mxbai ONNX | `mcp-agent-mail-search-core` crate |
| **Agent Mail current** | SQLite FTS5 + BM25 + search_planner + scope | Keep as fallback; replace retrieval engine |

**Key principle**: Agent Mail already has a sophisticated 13,000+ line search stack (planner, service, scope, recipes). Search V3 replaces the *retrieval engine* (FTS5 → Tantivy + vectors) while preserving the *query planning, scope enforcement, and tool interfaces*.

---

## 2. Component-by-Component Mapping

### 2.1 Tantivy Schema & Analyzers

#### CASS Approach
- **File**: `src/search/tantivy.rs` lines 386-421
- **Fields**: agent, workspace, source_path, title, content, title_prefix, content_prefix, created_at (FAST), preview, provenance fields
- **Tokenizer**: Custom `hyphen_normalize` = `SimpleTokenizer` → `LowerCaser` → `RemoveLongFilter(256)`
- **Edge n-grams**: Separate `title_prefix`/`content_prefix` fields with pre-computed 2-21 char n-grams
- **Schema versioning**: `schema_hash.json` file alongside index; mismatch triggers full rebuild

#### XF Approach
- **File**: `src/search.rs` lines ~100-150
- **Fields**: id (STRING), text (TEXT indexed with positions), text_prefix (TEXT basic), type (STRING), created_at (i64 FAST), metadata (STORED)
- **Tokenizer**: Default Tantivy tokenizer (lowercase + split on non-alphanumeric)
- **Prefix field**: Separate field with `IndexRecordOption::Basic` (no positions, saves space)

#### Agent Mail Equivalent
```rust
// Proposed schema for mcp-agent-mail-search-core
pub fn build_message_schema() -> tantivy::Schema {
    let mut b = Schema::builder();
    let text_opts = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("am_normalize")   // Custom: LowerCaser + RemoveLong(256)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();

    // Identity
    b.add_i64_field("message_id", INDEXED | STORED | FAST);
    b.add_i64_field("project_id", INDEXED | STORED | FAST);
    b.add_i64_field("product_id", INDEXED | STORED | FAST);

    // Searchable text
    b.add_text_field("subject", text_opts.clone());
    b.add_text_field("body", text_opts);
    b.add_text_field("subject_prefix", TEXT);  // Edge n-gram (basic indexing)

    // Exact-match filters (STRING = not tokenized)
    b.add_text_field("sender_name", STRING | STORED);
    b.add_text_field("thread_id", STRING | STORED | FAST);
    b.add_text_field("importance", STRING | STORED);

    // Temporal
    b.add_i64_field("created_ts", INDEXED | STORED | FAST);  // Microseconds

    // Boolean flags
    b.add_u64_field("ack_required", INDEXED | STORED);

    b.build()
}
```

**Migration notes**:
- CASS's `hyphen_normalize` is the right base — prevents Tantivy from splitting `POL-358` into `POL` and `358`
- Edge n-grams from CASS are worth adopting for prefix/typeahead search
- XF's separate prefix field pattern saves index space vs inline n-grams
- Agent Mail's existing `sanitize_fts_query()` handles hyphenated tokens; Tantivy's custom tokenizer makes this unnecessary at the query level
- Schema versioning via hash file (CASS pattern) is cleaner than migration-based versioning

---

### 2.2 Vector Index & ANN

#### CASS Approach
- **Format**: CVVI binary (magic `"CVVI"`, version u16, F32/F16 quantization)
- **File**: `src/search/vector_index.rs` — full mmap with 70-byte fixed rows
- **HNSW**: `src/search/ann_index.rs` — M=16, ef_construction=200, ef_search=100, DistDot
- **Parallel search**: Rayon with 10,000-element threshold, 1024-element chunks
- **Binary format**: CHSW for HNSW graph persistence

#### XF Approach
- **Format**: XFVI binary (magic `"XFVI"`, F16 only, sorted by doc_type+doc_id)
- **File**: `src/vector.rs` — mmap via `fmmap` crate
- **SIMD**: `wide::f32x8` for dot product (8-wide, portable x86/ARM)
- **Two-phase top-k**: Phase 1 stores offsets only (no String alloc); Phase 2 extracts top-k

#### Agent Mail Equivalent
- **Adopt**: XF's XFVI format (simpler, F16-only is sufficient for Agent Mail scale)
- **Adopt**: XF's SIMD dot product (`wide::f32x8`)
- **Defer**: HNSW (CASS) — overkill for typical Agent Mail corpora (<100k messages). Brute-force with SIMD is fast enough up to ~50k vectors
- **Defer**: Rayon parallelism — single-threaded search is fine for <100k messages

**Proposed vector index layout**:
```
{archive_root}/.search/vectors/{embedder_id}.amvi
```

Binary format (simplified from XFVI):
```
Header (24 bytes):
  Magic: "AMVI" (4 bytes)
  Version: u16
  Dimension: u32
  Count: u64
  EmbedderIdLen: u16
  EmbedderId: bytes

Records (12 bytes each, sorted by message_id):
  message_id: i64
  vec_offset: u32  // Offset into slab

Vector slab:
  Count × Dimension × 2 bytes (F16)
```

**Estimated sizes** (384-dim embeddings):
- 10k messages: ~7.5 MB (vectors) + 120 KB (records) = ~7.6 MB
- 100k messages: ~75 MB + 1.2 MB = ~76 MB

---

### 2.3 Embedder Abstraction

#### CASS Approach
- **Trait**: `src/search/embedder.rs` lines 60-151
  ```rust
  pub trait Embedder: Send + Sync {
      fn embed(&self, text: &str) -> Result<Vec<f32>>;
      fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
      fn dimension(&self) -> usize;
      fn id(&self) -> &str;
      fn is_semantic(&self) -> bool;
  }
  ```
- **Implementations**: Hash (FNV-1a, 384-dim), FastEmbed (MiniLM), daemon-forwarded
- **Fallback chain**: Daemon → local FastEmbed → hash embedder

#### XF Approach
- **Trait**: `src/embedder.rs`
  ```rust
  pub trait Embedder: Send + Sync {
      fn embed(&self, text: &str) -> Result<Vec<f32>>;
      fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
      fn dimension(&self) -> usize;
      fn id(&self) -> &str;
      fn model_name(&self) -> &str;
      fn is_semantic(&self) -> bool;
      fn supports_mrl(&self) -> bool;
      fn category(&self) -> ModelCategory;
  }
  ```
- **Implementations**: Hash (FNV-1a), StaticMrl (ONNX), MiniLM/BGE/Nomic (fastembed), Model2Vec
- **Model registry**: Canonical names, auto-download, MRL truncation wrapper

#### Agent Mail Equivalent
```rust
// In mcp-agent-mail-search-core
pub trait SearchEmbedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>, SearchError>;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError>;
    fn dimension(&self) -> usize;
    fn id(&self) -> &str;
    fn is_semantic(&self) -> bool;
}
```

**Implementation priority**:
1. **Hash embedder** (zero deps, instant, ~384-dim) — always available, good baseline
2. **Static-MRL embedder** (ONNX via `ort`, ~256-dim) — better quality, no fastembed dep
3. **MiniLM** (via fastembed, 384-dim) — best quality, heaviest dep — behind feature flag

**Runtime constraint**: All `embed()` calls must be sync (no tokio). ONNX inference via `ort` is sync. Hash is trivially sync. No daemon needed.

---

### 2.4 Hybrid Fusion (RRF)

#### CASS Approach
- **File**: `src/search/query.rs` lines 183-185
- **Algorithm**: RRF with k=60
- **Candidate multiplier**: 3x lexical, 4x semantic (ANN)
- **Integration**: Part of the monolithic `SearchClient::search_with_mode()`

#### XF Approach
- **File**: `src/hybrid.rs` (1,171 lines, clean standalone)
- **Algorithm**: RRF with k=60
- **Candidate multiplier**: 3x for both
- **Tie-breaking**: RRF score → in_both bonus → doc_id (deterministic)
- **Function**: `rrf_fuse(lexical, semantic, limit, offset) -> Vec<FusedHit>`

#### Agent Mail Equivalent
- **Adopt XF's approach** — cleaner, standalone, deterministic tie-breaking
- **Preserve Agent Mail's cursor pagination** — extend `SearchCursor` to include RRF score

```rust
// In mcp-agent-mail-search-core
pub fn rrf_fuse(
    lexical: &[LexicalHit],
    semantic: &[SemanticHit],
    limit: usize,
    offset: usize,
) -> Vec<FusedHit> {
    const RRF_K: f32 = 60.0;
    // ... standard RRF with deterministic tie-breaking
}
```

**No async needed** — RRF is pure computation.

---

### 2.5 Reranking

#### CASS Approach
- **File**: `src/search/fastembed_reranker.rs`
- **Model**: `ms-marco-MiniLM-L-6-v2` via fastembed
- **Trait**: `fn rerank(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>>`

#### XF Approach
- **File**: `src/flashrank_reranker.rs` (351 lines), `src/mxbai_reranker.rs`
- **Models**: FlashRank Nano (~4MB) or Mxbai xsmall (~6MB) via `ort` (ONNX Runtime)
- **Pipeline**: Tokenize → pad → ONNX inference → sigmoid(logits)
- **Max sequence length**: 512 tokens
- **Batch size**: 32

#### Agent Mail Equivalent
- **Adopt XF's FlashRank** — smaller model, `ort` only (no fastembed dep), CPU-fast
- **Behind feature flag**: `search-rerank` feature in Cargo.toml
- **Fallback**: Skip reranking when feature disabled or model unavailable

```rust
// In mcp-agent-mail-search-core (behind feature flag)
pub trait SearchReranker: Send + Sync {
    fn rerank(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>, SearchError>;
    fn model_name(&self) -> &str;
    fn max_length(&self) -> usize;
}
```

**Runtime constraint**: ONNX inference via `ort` is sync. Mutex-wrapped session is fine for single-threaded Agent Mail search.

---

### 2.6 Index Lifecycle

#### CASS Approach
- **Creation**: `run_index()` in `src/indexer/mod.rs` — streaming via crossbeam channels
- **Incremental**: Content-hash deduplication (`conversation_exists()`)
- **Rebuild**: Schema mismatch triggers `remove_dir_all` + fresh build
- **Merge**: Background segment merge when >4 segments, 5-min cooldown
- **Watch mode**: `notify` crate for filesystem monitoring + auto-reindex

#### XF Approach
- **Creation**: `Index::create_in_dir(path, schema)`
- **Reader**: `ReloadPolicy::OnCommitWithDelay` — auto-refreshes readers after commits
- **Writer**: Explicit `commit()` calls
- **No watch mode** — manual reindex via CLI

#### Agent Mail Equivalent
- **Trigger**: Index on message ingest (send_message, reply_message), not filesystem watch
- **Incremental**: Message ID monotonically increases → only index messages > last_indexed_id
- **Rebuild**: On schema version mismatch (detected via hash file, CASS pattern)
- **Merge**: Triggered during low-activity periods (e.g., health check) or explicit CLI command
- **No daemon**: All in-process

```rust
// In mcp-agent-mail-search-core
pub trait IndexLifecycle: Send + Sync {
    /// Rebuild entire index from scratch
    fn rebuild(&self) -> Result<IndexStats, SearchError>;

    /// Index new/changed documents since last checkpoint
    fn update_incremental(&self, changes: &[DocChange]) -> Result<usize, SearchError>;

    /// Current index health metrics
    fn health(&self) -> IndexHealth;
}

pub struct IndexHealth {
    pub document_count: u64,
    pub segment_count: u32,
    pub last_commit_ts: i64,
    pub schema_version: String,
    pub index_size_bytes: u64,
    pub needs_rebuild: bool,
}
```

**Integration with write-behind cache**: Index updates can be batched via the existing `CoalesceQueue` (in `mcp-agent-mail-db/src/coalesce.rs`), coalescing multiple message inserts into a single Tantivy commit.

---

### 2.7 Caching Strategies

#### CASS Approach
- **Query embedding cache**: LRU, query text → embedding vector
- **Regex cache**: Global LRU, (field, pattern) → compiled RegexQuery
- **Result cache**: Sharded LRU, per-agent/workspace
- **String interner**: LRU, deduplicates cache keys

#### XF Approach
- **Model cache**: LRU in daemon, max 4 models in memory
- **Vector index cache**: OS page cache via mmap (no application-level cache)
- **No query result cache** — SQLite is fast enough

#### Agent Mail Equivalent
- **Adopt**: Query embedding cache (LRU, from CASS) — avoids re-embedding identical queries
- **Adopt**: OS page cache for vector mmap (XF pattern) — zero application overhead
- **Skip**: Result cache — Agent Mail already has read-behind cache in `mcp-agent-mail-db`
- **Skip**: Regex cache — Agent Mail's FTS sanitizer handles this differently

---

### 2.8 Runtime Migration Hazards

#### Tokio → asupersync

| Component | Uses Tokio? | Migration Strategy |
|-----------|-------------|-------------------|
| Tantivy index/search | No (sync) | Direct use |
| Vector mmap + dot product | No (sync) | Direct use |
| HNSW search | No (sync) | Direct use |
| RRF fusion | No (pure compute) | Direct use |
| ONNX reranking | No (sync Mutex) | Direct use |
| Hash embedding | No (pure compute) | Direct use |
| ONNX embedding | Depends on `ort` | Sync API available |
| FastEmbed | Yes (internal tokio) | **Replace with `ort` direct** |
| Daemon client (CASS) | Yes (UDS async) | **Remove entirely** |
| Two-tier search (CASS) | Yes (background spawn) | **Replace with sync pipeline** |
| Background merge | Yes (tokio::spawn) | Use `std::thread` or coalesce queue |
| File watching (CASS) | Yes (notify + tokio) | **Remove entirely** (event-driven instead) |

**Safe zone**: All core search operations (Tantivy queries, vector search, RRF, reranking) are inherently synchronous. Only background/daemon features need async, and those are being removed for Agent Mail.

#### rusqlite → sqlmodel_rust

- CASS and XF use `rusqlite` directly
- Agent Mail uses `sqlmodel_rust` (wraps SQLite with type-safe queries)
- **No conflict**: Search core crate owns its own Tantivy index; SQLite is only for the existing FTS5 fallback (in `mcp-agent-mail-db`)
- **Migration path**: Keep FTS5 as fallback during shadow-validation phase; decommission after Tantivy parity is proven

---

## 3. Existing Agent Mail Search (Preserve)

The current Agent Mail search stack is **production-grade** (~13,000 lines) and must be preserved:

### 3.1 Search Planner (`search_planner.rs`, 1,923 lines)
- **SearchQuery**: text, doc_kind, project/product scope, importance, direction, agent, thread, ack, time range, ranking mode, cursor, explain
- **Plan methods**: FTS (preferred) → LIKE (fallback) → FilterOnly → Empty
- **Cursor pagination**: Stable (score, id) encoding with IEEE 754 bit representation
- **Facet SQL generation**: Composable WHERE clauses

**V3 integration**: Search planner gains a new `PlanMethod::Tantivy` that delegates to search-core. Existing FTS/LIKE methods remain as fallback.

### 3.2 Search Service (`search_service.rs`, 474 lines)
- **Two paths**: `execute_search()` (scoped) and `execute_search_simple()` (legacy)
- **Scope enforcement**: Visibility verdicts (Allow/Redact/Deny) with contact policy checks

**V3 integration**: Service layer unchanged — it orchestrates planner + scope regardless of retrieval engine.

### 3.3 Search Scope (`search_scope.rs`, 1,080 lines)
- **ScopeContext**: viewer identity, approved contacts, project membership, sender policies
- **ScopeVerdict**: Allow/Redact/Deny with rich reasons
- **Redaction policies**: body, sender, thread redaction

**V3 integration**: Scope enforcement is post-retrieval — works identically whether results come from FTS5 or Tantivy.

### 3.4 Search Recipes (`search_recipes.rs`, 1,067 lines)
- **Saved searches**: Named, pinned, use-counted, deep-linked
- **Query history**: Auto-logged, prunable

**V3 integration**: Recipes are query-level — independent of retrieval engine.

### 3.5 FTS Sanitization (`queries.rs`)
- `sanitize_fts_query()`: Handles SQL injection, emoji-only, bare operators, hyphenated tokens
- `extract_like_terms()`: Fallback when FTS fails
- `quote_hyphenated_tokens()`: Jira-style ID handling

**V3 integration**: Sanitization becomes less critical with Tantivy (which has its own query parser). Keep for FTS5 fallback path. Custom Tantivy tokenizer handles hyphenation at index time.

### 3.6 MCP Tool Interface
```
search_messages(project_key, query, limit) → [{id, subject, importance, ack_required, created_ts, thread_id, from}]
summarize_thread(project_key, thread_id, ...) → {summary, examples}
```

**V3 integration**: Tool signatures are unchanged. Internally, they call the search service which delegates to either FTS5 or Tantivy based on configuration.

---

## 4. Proposed Crate Boundary

```
mcp-agent-mail-search-core (NEW)
├── traits.rs       ← SearchEngine, IndexLifecycle, DocumentSource, SearchEmbedder, SearchReranker
├── query.rs        ← SearchQuery, SearchResults, SearchHit, SearchMode, FusedHit
├── fusion.rs       ← rrf_fuse(), score normalization
├── schema.rs       ← Tantivy schema builder, tokenizer registration, schema versioning
├── index.rs        ← TantivyEngine (implements SearchEngine + IndexLifecycle)
├── vector.rs       ← VectorIndex (mmap, F16, SIMD dot product)
├── embedder/
│   ├── hash.rs     ← HashEmbedder (FNV-1a, zero deps)
│   └── ort.rs      ← OrtEmbedder (static-mrl, behind feature flag)
├── reranker/
│   └── flashrank.rs ← FlashRankReranker (ONNX, behind feature flag)
└── health.rs       ← IndexHealth, stale detection

Feature flags:
  - default = ["tantivy"]
  - semantic = ["tantivy", "half", "wide", "memmap2"]
  - rerank = ["semantic", "ort"]
  - hybrid = ["semantic"]
```

**Dependency rules**:
- search-core does NOT depend on `mcp-agent-mail-db` (no SQL)
- search-core communicates via traits (`DocumentSource` provides documents to index)
- `mcp-agent-mail-db` implements `DocumentSource` for the message table
- `mcp-agent-mail-tools` orchestrates search-core + db + scope

---

## 5. Reusable Components Summary

### Directly Portable (Copy/Adapt)

| Component | Source | File | Lines | Notes |
|-----------|--------|------|------:|-------|
| Edge n-gram generation | CASS | `tantivy.rs:343-384` | 42 | Uses ArrayVec for stack alloc |
| Schema hash versioning | CASS | `tantivy.rs:85-129` | 45 | JSON hash file alongside index |
| RRF fusion | XF | `hybrid.rs:~50-120` | 70 | Clean standalone, deterministic |
| SIMD dot product (F16) | XF | `vector.rs:~500-550` | 50 | `wide::f32x8`, portable |
| Two-phase top-k | XF | `vector.rs:~600-700` | 100 | Offset-first, then hydrate |
| FlashRank ONNX pipeline | XF | `flashrank_reranker.rs` | 351 | Tokenize → pad → inference → sigmoid |
| Hash embedder | Both | `embedder.rs` | ~100 | FNV-1a, zero deps |
| Embedder trait | XF | `embedder.rs` | ~30 | Clean, with MRL support |
| Reranker trait | XF | `reranker.rs` | ~20 | Simple, sync |
| Metadata fast-path | XF | `search.rs` | ~10 | Skip JSON parse for `"{}"`/`"null"` |
| Deterministic tie-breaking | XF | `hybrid.rs` | ~10 | Score → in_both → doc_id |

### Intentionally Not Ported

| Component | Source | Reason |
|-----------|--------|--------|
| Daemon client | CASS | Agent Mail has no external daemon; all in-process |
| Two-tier progressive search | CASS | Requires async daemon; sync pipeline is sufficient |
| Watch mode / file monitoring | CASS | Agent Mail uses event-driven indexing (on message send) |
| Rayon parallel search | Both | <100k messages typical; single-threaded SIMD is fast enough |
| CVVI binary format | CASS | XF's simpler XFVI approach is sufficient |
| HNSW graph | CASS | Brute-force SIMD is fast enough for Agent Mail scale |
| Daemon model cache | XF | Models loaded once at startup, no eviction needed |
| Connector system | CASS | Agent Mail has a single data source (SQLite messages table) |

### Needs New Implementation

| Component | Why |
|-----------|-----|
| Agent Mail tokenizer (`am_normalize`) | Custom tokenizer for message domain (hyphen-aware, URL-tolerant) |
| Agent Mail schema builder | Message-specific fields (sender, thread, importance, ack) |
| DocumentSource trait | Bridge between `mcp-agent-mail-db` and search-core |
| Index trigger on message send | Integration with write-behind queue |
| Shadow validation harness | Run FTS5 and Tantivy in parallel, compare results |

---

## 6. Performance Projections

### Current FTS5 Benchmarks (from SPEC)

| Messages | FTS p50 (us) | FTS p95 (us) | DB size (MiB) |
|---------:|-------------:|-------------:|--------------:|
| 10,000 | 48 | 56 | 3.65 |
| 100,000 | 65 | 81 | 35.98 |

### Expected Tantivy Performance (from CASS/XF patterns)

| Messages | Lexical p50 (us) | Lexical p95 (us) | Index size (MiB) |
|---------:|------------------:|------------------:|-----------------:|
| 10,000 | 20-30 | 40-50 | ~5 |
| 100,000 | 30-50 | 60-80 | ~50 |

### Expected Vector Search Performance

| Messages | Brute SIMD p50 (us) | Index size (MiB) |
|---------:|--------------------:|-----------------:|
| 10,000 | 500-1,000 | ~8 |
| 100,000 | 5,000-10,000 | ~76 |

### Expected Hybrid Performance (RRF fusion adds ~10us)

| Messages | Hybrid p50 (us) | Hybrid p95 (us) |
|---------:|------------------:|------------------:|
| 10,000 | 600-1,100 | 1,200-1,500 |
| 100,000 | 5,100-10,100 | 10,500-12,000 |

**Conclusion**: For <100k messages, brute-force vector search is acceptable. HNSW becomes necessary only at >100k scale.

---

## 7. Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Tantivy version incompatibility with asupersync | High | Tantivy is sync; no runtime conflict |
| `ort` (ONNX) binary size bloat | Medium | Behind feature flag; hash embedder as default |
| FTS5 → Tantivy result quality regression | High | Shadow validation phase before decommission |
| Index corruption during concurrent writes | High | Single-writer design (Tantivy default); coordinate via coalesce queue |
| Scope enforcement gap | Critical | Scope is post-retrieval; apply identically to both engines |
| Memory usage from mmap vector index | Low | OS handles paging; only active pages in RAM |

---

## 8. Recommended Implementation Order

```
1. Create search-core crate skeleton with traits     (br-2tnl.2.1)
2. Implement Tantivy schema + index lifecycle        (br-2tnl.3.x)
3. Implement hash embedder + vector index            (br-2tnl.4.x)
4. Implement RRF fusion                              (br-2tnl.5.x)
5. Wire into search_planner as PlanMethod::Tantivy   (br-2tnl.6.x)
6. Shadow validation (FTS5 vs Tantivy in parallel)   (br-2tnl.7.x)
7. Feature-flag reranking (FlashRank)                (br-2tnl.5.x)
8. Decommission FTS5 triggers                        (br-2tnl.8.x)
```

Each step is independently testable and deployable behind feature flags.
