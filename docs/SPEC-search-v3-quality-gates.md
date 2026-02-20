# Spec: Search V3 Relevance / Performance SLOs and Quality Gates

**Bead:** br-2tnl.1.4
**Track:** br-2tnl.1
**Date:** 2026-02-11
**Status:** Proposed
**Reference:** SPEC-search-v3-query-contract.md, ADR-003-search-v3-architecture.md, SPEC-unified-search-corpus.md

## Purpose

Define measurable acceptance criteria for the Search V3 engine transition.
These gates govern when each migration phase advances:

```
FTS5 → Shadow → Tantivy → Hybrid
```

No phase transition occurs until all gates for the target phase are met.

## 1. Benchmark Corpus

### 1.1 Standard corpus

A synthetic + sampled corpus for reproducible testing:

| Corpus | Messages | Projects | Agents | Description |
|--------|----------|----------|--------|-------------|
| `micro` | 1,000 | 1 | 5 | Fast iteration, CI smoke tests |
| `small` | 10,000 | 3 | 20 | Standard regression suite |
| `medium` | 100,000 | 10 | 50 | Performance ceiling tests |
| `large` | 1,000,000 | 25 | 100 | Scale validation (optional, nightly) |

Each corpus includes:

- 50 annotated query/relevance-judgment pairs (human-labeled)
- Known needle messages for precision tests (1% prevalence)
- Multi-language messages (English primary, Unicode edge cases)
- Thread structures with 1-50 message depth
- All importance levels, ack states, and time ranges represented

### 1.2 Relevance judgments

Each judgment is a (query, document_id, grade) triple:

| Grade | Meaning |
|-------|---------|
| 3 | Highly relevant — directly answers the query |
| 2 | Relevant — contains useful information |
| 1 | Marginally relevant — tangentially related |
| 0 | Not relevant |

At least 20 graded documents per query. Judgments are stored as
`tests/fixtures/search_v3/relevance_judgments.jsonl`.

## 2. Relevance SLOs

### 2.1 Lexical mode (Tantivy vs FTS5)

Tantivy must meet or exceed FTS5 quality before serving:

| Metric | Definition | Gate |
|--------|------------|------|
| NDCG@10 | Normalized Discounted Cumulative Gain at rank 10 | >= FTS5 NDCG@10 |
| NDCG@50 | NDCG at rank 50 | >= FTS5 NDCG@50 - 0.02 (tolerance) |
| MRR | Mean Reciprocal Rank of first relevant result | >= FTS5 MRR |
| Recall@50 | Fraction of relevant docs found in top 50 | >= FTS5 Recall@50 |
| Overlap@50 | Jaccard coefficient of top-50 result sets | >= 0.80 |

All metrics computed on the `small` corpus with 50 standard queries.

**Phase gate**: Shadow → Tantivy requires ALL lexical gates to pass on both
`small` and `medium` corpora.

### 2.2 Semantic mode

| Metric | Gate | Notes |
|--------|------|-------|
| NDCG@10 | >= 0.65 | Absolute floor for semantic-only |
| Recall@10 | >= 0.40 | Must find relevant docs in small windows |

Semantic mode is never the sole serving path — it feeds into hybrid.
These gates prevent regression in the vector pipeline.

### 2.3 Hybrid mode

| Metric | Gate | Notes |
|--------|------|-------|
| NDCG@10 | >= Tantivy NDCG@10 + 0.03 | Hybrid must measurably improve over lexical |
| MRR | >= Tantivy MRR | Must not regress first-relevant-result position |
| Recall@50 | >= Tantivy Recall@50 | Hybrid should never lose recall |

**Phase gate**: Tantivy → Hybrid requires ALL hybrid gates to pass.

### 2.4 HybridRerank mode

| Metric | Gate | Notes |
|--------|------|-------|
| NDCG@10 | >= Hybrid NDCG@10 | Reranking must not degrade top-10 quality |
| MRR | >= Hybrid MRR + 0.02 | Reranking should improve first-result placement |

## 3. Latency SLOs

All latencies measured end-to-end (`execute_search` entry to return), excluding
network I/O. Measured on the standard benchmark host with warm caches.

### 3.1 Lexical mode

| Corpus | p50 | p95 | p99 |
|--------|-----|-----|-----|
| `micro` (1k) | < 200 us | < 500 us | < 1 ms |
| `small` (10k) | < 500 us | < 1 ms | < 2 ms |
| `medium` (100k) | < 1 ms | < 3 ms | < 5 ms |
| `large` (1M) | < 5 ms | < 15 ms | < 30 ms |

**Baseline comparison**: FTS5 at 100k messages: p50 = 65 us, p95 = 81 us
(from SPEC-unified-search-corpus.md benchmarks). Tantivy is expected to be
slightly slower than FTS5 for pure lexical due to index traversal overhead,
but must stay within the SLO bounds.

### 3.2 Semantic mode (vector search)

| Corpus | p50 | p95 | p99 |
|--------|-----|-----|-----|
| `micro` (1k) | < 500 us | < 1 ms | < 2 ms |
| `small` (10k) | < 1 ms | < 3 ms | < 5 ms |
| `medium` (100k) | < 10 ms | < 20 ms | < 30 ms |

Note: brute-force vector scan on 100k messages at 384-dim is ~5-10 ms projected.
HNSW optimization deferred until >100k messages.

### 3.3 Hybrid mode

| Corpus | p50 | p95 | p99 |
|--------|-----|-----|-----|
| `micro` (1k) | < 1 ms | < 2 ms | < 5 ms |
| `small` (10k) | < 2 ms | < 5 ms | < 10 ms |
| `medium` (100k) | < 15 ms | < 30 ms | < 50 ms |

### 3.4 HybridRerank mode

| Corpus | p50 | p95 | p99 |
|--------|-----|-----|-----|
| `micro` (1k) | < 5 ms | < 10 ms | < 20 ms |
| `small` (10k) | < 10 ms | < 25 ms | < 50 ms |
| `medium` (100k) | < 30 ms | < 60 ms | < 100 ms |

Reranking dominates latency. `rerank_top_k` (default 100) directly controls
the number of ONNX inference calls.

### 3.5 Regression rule

No phase transition if any latency SLO regresses by > 20% compared to the
previous engine at the same corpus size.

## 4. Resource Budgets

### 4.1 Memory

| Component | Budget | Notes |
|-----------|--------|-------|
| Tantivy index (heap) | < 50 MB | Segment metadata, term dict cache |
| Vector index (mmap) | 8-76 MB at 10k-100k | `count * dim * 2` bytes F16 + record table |
| ONNX reranker model | < 200 MB | FlashRank MiniLM (loaded once) |
| Embedder model | < 100 MB | Hash embedder: ~0; ORT embedder: ~80 MB |

**Total budget**: < 500 MB RSS increase with all features enabled at 100k messages.

### 4.2 Disk

| Component | Budget per 100k messages |
|-----------|--------------------------|
| Tantivy index | < 200 MB |
| AMVI vector file | < 80 MB (384-dim F16) |
| ONNX model files | < 300 MB (one-time) |

### 4.3 Index build time

| Corpus | Full rebuild | Incremental (1000 msgs) |
|--------|-------------|------------------------|
| `micro` (1k) | < 1 s | < 100 ms |
| `small` (10k) | < 5 s | < 200 ms |
| `medium` (100k) | < 30 s | < 500 ms |
| `large` (1M) | < 5 min | < 1 s |

Full rebuild is triggered by schema mismatch or manual request.
Incremental updates are batched via the coalesce queue and triggered
on message send.

## 5. Error Budgets and Degradation Policy

### 5.1 Availability target

The search service must return a valid response for 100% of well-formed
queries, even under degraded conditions. This is achieved via the fallback
chain (SearchMode downgrade).

### 5.2 Degradation scenarios

| Scenario | Response | SLO impact |
|----------|----------|------------|
| Tantivy index corrupted | Rebuild + FTS5 fallback | Latency within FTS5 SLO |
| Vector index missing | Lexical-only (mode downgrade) | Relevance = lexical SLO |
| ONNX model missing | Hybrid without rerank | Relevance = hybrid SLO |
| FTS5 table dropped | Tantivy-only (no FTS5 fallback) | Must have Tantivy index |
| Both FTS5 and Tantivy down | Empty results + explain error | Acceptable for < 60s during rebuild |
| Embedding timeout | Skip semantic component | Relevance = lexical SLO |

### 5.3 Recovery time objectives

| Component | Recovery | Method |
|-----------|----------|--------|
| Tantivy index | < 30 s at 100k | Auto-rebuild on corruption detection |
| Vector index | < 60 s at 100k | Rebuild from messages + embedding |
| ONNX model | < 5 s | Reload from disk |
| FTS5 | < 10 s at 100k | Schema migration re-run |

## 6. Shadow Mode Quality Gates

During `SEARCH_ENGINE=shadow` phase, the following are continuously measured:

| Metric | Threshold | Action if breached |
|--------|-----------|-------------------|
| Overlap@50 (per query) | < 0.60 | Log warning, investigate |
| Mean Overlap@50 (rolling 1h) | < 0.75 | Block phase transition |
| p95 latency ratio (Tantivy/FTS5) | > 3.0 | Log warning |
| Mean latency ratio (rolling 1h) | > 2.0 | Block phase transition |
| Tantivy error rate | > 1% of queries | Block phase transition |

Shadow metrics are recorded via the existing query tracking infrastructure
and reported in `resource://tooling/metrics`.

## 7. Test Harness

### 7.1 Benchmark runner

```
tests/bench/search_v3_bench.rs
```

- Loads standard corpus from `tests/fixtures/search_v3/`
- Runs all 50 queries per mode (lexical, semantic, hybrid, hybrid_rerank)
- Computes NDCG@k, MRR, Recall@k, Overlap@k
- Reports p50/p95/p99 latency per mode and corpus size
- Outputs JSON report to `target/bench/search_v3_report.json`

### 7.2 Gate checker

```
scripts/search_v3_gate_check.sh
```

- Reads the benchmark report JSON
- Compares against SLO thresholds defined in this spec
- Exits 0 (all gates pass) or 1 (any gate fails) with human-readable summary
- Used in CI as a phase-transition gate

### 7.3 Regression tests

Added to the standard `cargo test` suite:

- `test_lexical_ndcg_floor`: NDCG@10 >= 0.50 on micro corpus (smoke test)
- `test_latency_ceiling_micro`: p99 < 5 ms on micro corpus
- `test_fallback_chain`: Each mode correctly degrades when features disabled
- `test_score_normalization`: All scores in [0.0, 1.0] range
- `test_cursor_engine_tag`: V3 cursor roundtrip with engine tag

## 8. Phase Transition Decision Matrix

| From → To | Required Gates | Approver |
|-----------|---------------|----------|
| FTS5 → Shadow | Shadow infrastructure deployed | Automated (feature flag) |
| Shadow → Tantivy | All lexical SLOs (S2.1) + latency SLOs (S3.1) + shadow metrics (S6) | Manual review + CI green |
| Tantivy → Hybrid | All hybrid SLOs (S2.3) + latency SLOs (S3.3) + semantic SLOs (S2.2) | Manual review + CI green |
| Hybrid → HybridRerank | All rerank SLOs (S2.4) + latency SLOs (S3.4) | Manual review + CI green |

Each transition is reversible via `SEARCH_ENGINE` env var.

## 9. Steady-State Verification Mapping

Operational execution for these gates is defined in:
`docs/RUNBOOK-search-v3-migration.md#steady-state-operations-post-cutover`.

The weekly steady-state bundle must include:

- `tests/e2e/test_search_v3_stdio.sh`
- `tests/e2e/test_search_v3_http.sh`
- `tests/e2e/test_search_v3_resilience.sh`
- `tests/e2e/test_search_v3_load_concurrency.sh`

Artifact integrity validation:

```bash
source scripts/e2e_lib.sh
e2e_validate_bundle_tree tests/artifacts
```

Any guardrail breach (error-rate, latency drift, relevance drift, or
scope/redaction regression) requires opening a follow-up bead with:

- failing query samples,
- health snapshot JSON,
- relevant server log window,
- artifact paths under `tests/artifacts/search_v3_*`,
- scenario-level `case_id` + status from `search_v3/summaries/suite_summary.json`,
- timings + failure reason details from `metrics.json` and diagnostics logs,
- `repro.txt` / `repro.env` so the verification can be rerun deterministically.
