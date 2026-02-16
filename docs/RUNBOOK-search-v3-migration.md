# RUNBOOK: Search V3 Migration Playbook

**Status:** Active
**Bead:** br-2tnl.8.3
**Supplemental Bead:** br-1x3h5
**Author:** MagentaMantis (claude-code/opus-4.5)
**Date:** 2026-02-12

## Overview

This runbook provides step-by-step procedures for migrating from SQLite FTS5 to Search V3
(Tantivy + optional semantic/hybrid). The migration is designed to be:

- **Safe:** Shadow validation before cutover
- **Reversible:** Kill switches and fallback paths
- **Observable:** Metrics at every stage

## Prerequisites

1. **Rust toolchain:** Nightly (see `rust-toolchain.toml`)
2. **Disk space:** ~50MB per 10k messages for Tantivy + vector indexes
3. **Environment:** All instances must have Search V3 crate enabled (check `Cargo.toml` features)

## Configuration Reference

| Variable | Values | Default | Description |
|----------|--------|---------|-------------|
| `AM_SEARCH_ENGINE` | `legacy`, `lexical`, `semantic`, `hybrid`, `auto` | `legacy` | Primary search engine |
| `AM_SEARCH_SHADOW_MODE` | `off`, `log_only`, `compare` | `off` | Shadow comparison mode |
| `AM_SEARCH_SEMANTIC_ENABLED` | `true`, `false` | `false` | Kill switch for semantic embeddings |
| `AM_SEARCH_RERANK_ENABLED` | `true`, `false` | `false` | Kill switch for reranking |
| `AM_SEARCH_FALLBACK_ON_ERROR` | `true`, `false` | `true` | Fall back to FTS5 on V3 errors |
| `AM_SEARCH_ENGINE_FOR_<TOOL>` | (same as above) | - | Per-tool engine override |

**Kill switch behavior:**
- `AM_SEARCH_SEMANTIC_ENABLED=false` + `AM_SEARCH_ENGINE=hybrid` → degrades to `lexical`
- `AM_SEARCH_RERANK_ENABLED=false` → skips reranking step, returns fusion scores

---

## Phase 1: Preparation (Day -7)

### 1.1 Verify Feature Flags

```bash
# Confirm search-core crate is included
cargo tree -p mcp-agent-mail-search-core

# Confirm tantivy feature is enabled
cargo tree -p mcp-agent-mail-search-core -f "{p} {f}"
```

Expected output includes `tantivy-engine` feature.

### 1.2 Build Initial Index

```bash
# Set environment (or use .env file)
export AM_SEARCH_ENGINE=legacy
export DATABASE_URL=sqlite:///path/to/agent_mail.db
export STORAGE_ROOT=/path/to/.mcp_agent_mail

# Trigger full index build (runs in background)
# The index builds automatically on first startup with V3 enabled
cargo run -p mcp-agent-mail -- serve --port 8765 --no-tui &

# Monitor index build progress
tail -f logs/search_index.log | grep -E "(IndexLifecycle|rebuild|indexed)"
```

### 1.3 Validation Checkpoint 1: Index Built

```bash
# Check index directory exists
ls -la "${STORAGE_ROOT}/search_index/"

# Verify index health via health_check tool
curl -X POST http://127.0.0.1:8765/mcp/ \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
  | jq '.result.content[0].text | fromjson | .search_index_health'
```

Expected: `"status": "ready"` with document counts matching database.

---

## Phase 2: Shadow Validation (Days 0-14)

### 2.1 Enable Shadow Mode (Log Only)

```bash
# Shadow mode: run both engines, return legacy results, log comparison
export AM_SEARCH_ENGINE=legacy
export AM_SEARCH_SHADOW_MODE=log_only
```

Restart server. All search queries now run against both FTS5 and Tantivy.

### 2.2 Monitor Shadow Metrics

```bash
# View shadow comparison logs
grep "Search V3 shadow comparison" logs/server.log

# Key metrics to watch:
# - overlap_pct: percentage of top-10 results shared (target: >80%)
# - rank_correlation: Kendall tau (-1 to +1, target: >0.6)
# - latency_delta_ms: V3 vs legacy (target: <50ms slower, ideally faster)
# - v3_error_count: should be zero
```

### 2.3 Metrics Dashboard Queries

If using TUI (screen 7: Tool Metrics), shadow metrics appear under "search_messages".

For programmatic access:

```bash
curl -X POST http://127.0.0.1:8765/mcp/ \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
  | jq '.result.content[0].text | fromjson | .shadow_metrics'
```

### 2.4 Validation Checkpoint 2: Shadow Metrics Pass

**Quality Gates (must all pass):**

| Metric | Threshold | Rationale |
|--------|-----------|-----------|
| `equivalent_pct` | ≥75% | Most queries should produce equivalent results |
| `v3_error_pct` | <1% | V3 path must be stable |
| `avg_overlap_pct` | ≥70% | Result sets should substantially overlap |
| `avg_latency_delta_ms` | <100 | V3 should not be dramatically slower |

If any threshold fails, investigate before proceeding:

```bash
# Find divergent queries
grep "divergent results" logs/server.log | head -20

# Check specific query behavior
grep "query_text.*<your-query>" logs/server.log
```

---

## Phase 3: Shadow Compare Mode (Days 14-21)

### 3.1 Switch to Compare Mode

```bash
# Shadow compare: run both, return V3 results, log divergence warnings
export AM_SEARCH_ENGINE=lexical
export AM_SEARCH_SHADOW_MODE=compare
```

Restart server. Users now receive V3 results, but FTS5 still runs for comparison.

### 3.2 Monitor for Regressions

Watch for user complaints or automated test failures:

```bash
# Check for divergence warnings (compare mode logs divergent results)
grep "divergent results" logs/server.log | wc -l

# Compare with baseline
# (Store baseline count at start of compare phase)
```

### 3.3 Validation Checkpoint 3: Compare Mode Stable

After 7 days in compare mode:

- No critical bug reports related to search
- `v3_error_pct` still <1%
- No sustained latency degradation

---

## Phase 4: Full Cutover (Day 21+)

### 4.1 Disable Shadow Mode

```bash
export AM_SEARCH_ENGINE=lexical   # or hybrid if semantic enabled
export AM_SEARCH_SHADOW_MODE=off
```

Restart server. Only V3 engine runs.

### 4.2 Enable Semantic/Hybrid (Optional)

If semantic embeddings are desired:

```bash
export AM_SEARCH_ENGINE=hybrid
export AM_SEARCH_SEMANTIC_ENABLED=true
# Optionally enable reranking for improved relevance
export AM_SEARCH_RERANK_ENABLED=true
```

**Note:** Semantic embeddings require building the vector index, which happens
automatically in the background. Monitor progress:

```bash
grep "VectorIndex" logs/server.log | tail -20
```

### 4.3 Validation Checkpoint 4: V3 Serving

```bash
# Confirm engine is V3
curl -X POST http://127.0.0.1:8765/mcp/ \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
  | jq '.result.content[0].text | fromjson | .search_engine_active'

# Expected: "lexical" or "hybrid" (not "legacy")
```

---

## Phase 5: FTS5 Decommission (Day 28+)

### 5.1 Remove FTS5 Triggers

Once confident in V3, remove synchronous FTS5 triggers to eliminate write overhead:

```sql
-- Run via sqlite3 or migration script
DROP TRIGGER IF EXISTS fts_messages_ai;
DROP TRIGGER IF EXISTS fts_messages_au;
DROP TRIGGER IF EXISTS fts_messages_ad;
```

**WARNING:** This is irreversible. Ensure Phase 4 validation is complete.

### 5.2 Vacuum FTS5 Table (Optional)

If disk space is a concern, vacuum the FTS5 table:

```sql
-- Optional: reclaim space from FTS5
INSERT INTO fts_messages(fts_messages) VALUES('optimize');
VACUUM;
```

### 5.3 Validation Checkpoint 5: Clean State

```bash
# Confirm no FTS5 triggers
sqlite3 /path/to/agent_mail.db ".schema" | grep -c "fts_messages"

# Expected: 0 triggers, only the virtual table definition remains
```

---

## Rollback Procedures

### Rollback from Any Phase to Legacy

```bash
export AM_SEARCH_ENGINE=legacy
export AM_SEARCH_SHADOW_MODE=off
```

Restart server. FTS5 resumes immediately (no rebuild needed).

### Rollback After FTS5 Decommission

If triggers have been removed, FTS5 data may be stale. Options:

1. **Rebuild FTS5 index:**
   ```sql
   INSERT INTO fts_messages(fts_messages) VALUES('rebuild');
   ```

2. **Restore from backup:**
   Use the most recent database backup with intact FTS5 triggers.

---

## Per-Surface Override Examples

Override engine for specific tools while testing:

```bash
# Use hybrid for search_messages, but legacy for summarize_thread
export AM_SEARCH_ENGINE=legacy
export AM_SEARCH_ENGINE_FOR_SEARCH_MESSAGES=hybrid
export AM_SEARCH_ENGINE_FOR_FETCH_INBOX_PRODUCT=hybrid
```

---

## Troubleshooting

### Index Build Fails

```bash
# Check for corrupted index
rm -rf "${STORAGE_ROOT}/search_index/"

# Restart server to trigger rebuild
```

### V3 Returns Empty Results

1. Check index health: `health_check` tool
2. Verify schema hash: `${STORAGE_ROOT}/search_index/schema_hash.json`
3. Check query syntax: V3 is stricter about wildcard placement

### High Latency in Hybrid Mode

Semantic embedding generation can be slow without GPU:

```bash
# Disable semantic temporarily
export AM_SEARCH_SEMANTIC_ENABLED=false

# Or reduce embedding batch size
export AM_SEARCH_EMBEDDING_BATCH_SIZE=10
```

### Shadow Metrics Show High Divergence

This is expected for:
- Queries using semantic synonyms (V3 finds more)
- Queries with typos (V3 handles better with fuzzy matching)

Investigate specific queries before deciding if divergence is acceptable.

---

## Steady-State Operations (Post-Cutover)

This section is the operational contract for `br-2tnl.8.5` (steady-state operations +
verification). It assumes Search V3 is the active serving engine and focuses on
repeatable verification, incident handling, and evidence capture.

### Operational Guardrails (Source of Truth)

Use `docs/SPEC-search-v3-quality-gates.md` as the canonical threshold source.
The minimum steady-state guardrails to enforce operationally:

| Signal | Target / Threshold | Response if Breached |
|--------|--------------------|----------------------|
| `v3_error_rate` | `< 1%` rolling 15m | Trigger Incident Matrix + open P0 bead |
| `search_index_health` | `ready` | Run Procedure A (lexical rebuild) |
| Hybrid p95 latency drift | `< 20%` vs prior weekly baseline | Run load suite + profile/tune semantic path |
| Relevance drift (weekly) | No sustained regression vs prior artifact set | Open follow-up bead with evidence bundle |
| Queue pressure (`wbq`, `commit_coalescer`) | No sustained warning state | Investigate storage/DB pressure before user impact |

### Daily Verification (Operator Checklist)

1. Confirm serving engine + index health:
   ```bash
   curl -sS -X POST http://127.0.0.1:8765/mcp/ \
     -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
     | jq '.result.content[0].text | fromjson | {search_engine_active,search_index_health,pool_utilization,queues}'
   ```
2. Run a smoke query against production-like scope:
   ```bash
   AM_INTERFACE_MODE=cli mcp-agent-mail mail search \
     --project /abs/path/to/project \
     --limit 10 \
     "coordination" \
     --json | jq 'length'
   ```
3. Verify no sustained queue pressure (`queues.wbq.warning=false`, `queues.commit_coalescer.warning=false`).
4. Record output snapshots in an ops evidence folder (date-stamped) and link them in the active rollout/bead thread.

### Weekly Verification (Quality + Performance)

1. Execute quick CI gates and archive report:
   ```bash
   am ci --quick --json --report tests/artifacts/ci/search_v3_weekly.json
   ```
2. Run the Search V3 steady-state suite bundle:
   - `tests/e2e/test_search_v3_stdio.sh`
   - `tests/e2e/test_search_v3_http.sh`
   - `tests/e2e/test_search_v3_resilience.sh`
   - `tests/e2e/test_search_v3_load_concurrency.sh`
3. Validate artifact bundle integrity:
   ```bash
   source scripts/e2e_lib.sh
   e2e_validate_bundle_tree tests/artifacts
   ```
4. Compare this week’s latency/relevance artifacts against prior weekly baseline artifacts; open a follow-up bead if any guardrail regresses.

### Post-Cutover Verification Windows

Run this schedule after enabling full Search V3 serving:

1. **T+24h**: `stdio` + `http` + `resilience` suites must pass with complete artifacts.
2. **T+7d**: repeat weekly bundle and confirm no sustained error/latency drift.
3. **T+30d**: verify no recurring legacy fallback incidents and confirm operations remain within guardrails.

### Incident Triage Matrix

| Symptom | Immediate Action | Next Step |
|---------|------------------|-----------|
| Empty or near-empty results | Check `search_index_health` + last rebuild timestamp | Rebuild lexical index (Procedure A) |
| Latency spike in hybrid mode | Set `AM_SEARCH_SEMANTIC_ENABLED=false` temporarily | Profile embedding/vector path; tune batch/backpressure |
| Rising divergence in compare/shadow logs | Capture top divergent queries from server log | Run focused replay/parity script; decide rollback/cutover hold |
| V3 error rate >1% | Set `AM_SEARCH_ENGINE=legacy` if user impact is high | Open P0 bug bead with failing queries + artifacts |

### Repair Procedures

#### Procedure A: Lexical Index Rebuild

```bash
# 1) Stop server
# 2) Backup and remove corrupted lexical index
mv "${STORAGE_ROOT}/search_index" "${STORAGE_ROOT}/search_index.corrupt.$(date +%Y%m%d_%H%M%S)"

# 3) Restart server to trigger rebuild
mcp-agent-mail serve --no-tui
```

Post-check: `health_check` must report Search V3 index status `ready` and non-zero docs.

#### Procedure B: Semantic/Vector Cache Repair

```bash
# Temporarily force lexical-only service while repairing semantic assets
export AM_SEARCH_ENGINE=lexical
export AM_SEARCH_SEMANTIC_ENABLED=false
mcp-agent-mail serve --no-tui
```

Then clear/rebuild vector artifacts per deployment layout and re-enable semantic mode only after
latency and overlap checks return to thresholds.

### Follow-Up Bead Triggers

Create a new bead immediately when any of the following is observed:

- `v3_error_pct >= 1%` for more than 15 minutes
- `avg_latency_delta_ms >= 100` sustained across weekly verification
- `equivalent_pct < 75%` or measurable relevance regression on weekly replay/verification checks
- Any cross-project scope or redaction regression in search output

When creating the bead, attach:
- failing query samples,
- health snapshot JSON,
- relevant server log window,
- command transcript used for reproduction,
- artifact paths under `tests/artifacts/search_v3_*`.

---

## Checklist Summary

- [ ] Phase 1: Index built, health check passes
- [ ] Phase 2: Shadow log_only for 14 days, metrics pass thresholds
- [ ] Phase 3: Shadow compare for 7 days, no regressions
- [ ] Phase 4: V3 serving, search functionality verified
- [ ] Phase 5: FTS5 triggers removed (optional, irreversible)
- [ ] Daily verification checklist running with retained artifacts
- [ ] Weekly verification and drift review completed
- [ ] Post-cutover checks complete at T+24h, T+7d, and T+30d
- [ ] Incident triage + repair procedures validated by operator dry run

---

## Related Documentation

- `docs/ADR-003-search-v3-architecture.md` — Architecture decisions
- `docs/SPEC-search-v3-query-contract.md` — Query semantics
- `docs/SPEC-search-v3-quality-gates.md` — Quality metrics
- `docs/search-v3-component-mapping.md` — Component reference
