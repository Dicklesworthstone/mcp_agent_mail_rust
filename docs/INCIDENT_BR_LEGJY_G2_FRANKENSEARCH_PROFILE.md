# br-legjy.7.2 — frankensearch Hotpath Profiling Under TUI Traversal

## Scope
Assess whether `/dp/frankensearch` contributes to TUI lag/flashing, with explicit separation between:
- active direct `/dp/frankensearch` execution paths in current app flow, and
- local search orchestration overhead in `mcp-agent-mail-db`/`mcp-agent-mail-server` that can mask or amplify search latency.

## Scenario
- Bead: `br-legjy.7.2`
- Deterministic scenario ID: `20260302_184332`
- Run timestamp (UTC): `2026-03-02T18:43:32Z`
- Workload: 15-screen deterministic traversal with baseline profiling artifacts

## Reproduction Commands
```bash
# Deterministic traversal with baseline profile bundle
E2E_CAPTURE_BASELINE_PROFILE=1 bash scripts/e2e_tui_full_traversal.sh

# Consolidated traversal + /proc profile report
bash scripts/profile_tui_traversal.sh
```

## Artifact Index
- `tests/artifacts/tui_full_traversal/20260302_184332/traversal_results.json`
- `tests/artifacts/tui_full_traversal/20260302_184332/baseline_profile_summary.json`
- `tests/artifacts/tui_full_traversal/20260302_184332/cross_layer_attribution_report.json`
- `tests/artifacts/tui_full_traversal/20260302_184332/baseline_profile/baseline_forward_strace.log`
- `tests/artifacts/tui_profile/20260302_134332/proc_profile.json`

## Measured Signals (Search-Relevant)
Search-screen activation metrics from `traversal_results.json`:
- Forward: `first_byte=3.73ms`, `render=17.22ms`, `quiesce=101.40ms`, `output_bytes=19059`
- Backward: `first_byte=5.49ms`, `render=15.36ms`, `quiesce=101.29ms`, `output_bytes=19183`
- Jump: `first_byte=5.81ms`, `render=15.16ms`, `quiesce=101.40ms`, `output_bytes=19059`

Global baseline context (`baseline_profile_summary.json`):
- Quiesce (forward pass): `p50=99.99ms`, `p95=103.41ms`
- Syscalls: `statx=1078`, `pread64=515`, `futex=470`, `poll=210`
- Wait syscalls total: `783` (`short_wait<=5ms: 334`)

## Active-Path Clarification
Direct `/dp/frankensearch` hotpath confirmed in current flow:
- rerank step via `fs::rerank_step` from local search orchestration.

Most search query planning/routing/fallback/index warmup currently executes in local repo code:
- `crates/mcp-agent-mail-db/src/search_service.rs`
- `crates/mcp-agent-mail-db/src/search_v3.rs`
- `crates/mcp-agent-mail-server/src/tui_screens/search.rs`

This matters for attribution: `/dp/frankensearch` still influences latency (especially rerank and potential refresh/rebuild paths), but local orchestration/lifecycle work is a co-dominant factor for current Search-screen tail latency.

## Hotspot Anchors
Local active orchestration hotpaths:
- Search screen execution path: `tui_screens/search.rs::run_unified_search`
- Hybrid orchestration: `search_service.rs::execute_search`, `derive_hybrid_execution_plan`, `orchestrate_hybrid_results`
- Rerank integration + candidate shaping: `search_service.rs::maybe_apply_hybrid_rerank`
- Fallback/bootstrap: `ensure_lexical_bridge_initialized`, `run_lexical_backfill_for_pool`
- Index cadence: `search_v3.rs::index_message`, `backfill_from_db`, `choose_backfill_plan`

`/dp/frankensearch` anchors:
- Active rerank: `/dp/frankensearch/crates/frankensearch-rerank/src/pipeline.rs::rerank_step`
- Likely secondary hotspots (if enabled in deployment path):
  - `frankensearch-fusion/src/searcher.rs::{run_phase1, run_phase2}`
  - `frankensearch-fusion/src/rrf.rs::rrf_fuse_with_graph`
  - `frankensearch-fusion/src/refresh.rs::{run_cycle, rebuild_index}`
  - `frankensearch-index/src/search.rs::search_top_k_internal`

## Ranked Remediation Candidates
1. Reuse long-lived pool/runtime in Search screen (avoid per-query bootstrap)
- Expected impact: high
- Risk: low
- Parity constraint: no ranking or search-semantic change

2. Reduce rerank-stage cloning and pre-materialization before `/dp` rerank
- Expected impact: high on larger candidate sets
- Risk: medium
- Parity constraint: preserve candidate set, blend policy/weights, deterministic tie-breaks (`score desc`, stable ID tie-break)

3. Defer expensive preview/snippet shaping until visible rows/selection
- Expected impact: medium-high
- Risk: medium
- Parity constraint: rendered snippet content and `match_count` remain identical when displayed

4. Move lexical bridge warmup/backfill off first interactive query path
- Expected impact: medium (cold-start spike reduction)
- Risk: low-medium
- Parity constraint: keep strict no-ad-hoc-SQL fallback behavior and freshness semantics

5. Reduce `/dp/frankensearch` refresh/rebuild churn (incremental update policy)
- Expected impact: medium
- Risk: medium-high
- Parity constraint: index contents and ranking determinism must match full rebuild behavior

## Search-Semantics/Rerank Parity Proof Plan
Target candidate: #2 (rerank-stage cloning/materialization changes)

1. Candidate-set equivalence:
- For fixed query corpus, assert pre-rerank candidate IDs are identical pre/post optimization.

2. Ranking equivalence:
- Assert ordered result IDs and scores are identical (or within explicit float tolerance, if score representation changes but ordering is invariant).

3. Tie-break determinism:
- Verify stable ordering for tied scores on repeated runs.

4. Surface-level parity:
- Compare Search screen rendered result order + top-N snippets for deterministic fixtures.

5. Regression harness:
- Re-run deterministic traversal and verify no degradation in quiesce/first-byte tails while preserving search output parity.

## Mapping to Incident Budget
- Search screen consistently sits around `~101ms` quiesce in all traversal directions and contributes to >16ms frame-latency risk.
- Improvements should prioritize lifecycle + candidate-shaping reductions first, then `/dp` refresh/rebuild churn, while enforcing strict search-semantic/ranking parity gates.

## Failure/Diagnostics Notes
- Baseline harness logs contain a non-fatal line: `FAIL baseline profile: missing expected profiler evidence`.
- Required artifacts were present and analyzable (`baseline_profile_summary.json`, strace log, traversal results).
