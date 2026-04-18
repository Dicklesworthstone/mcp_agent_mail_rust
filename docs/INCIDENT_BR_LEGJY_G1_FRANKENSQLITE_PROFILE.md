# br-legjy.7.1 — frankensqlite Hotpath Profiling Under TUI Traversal

## Scope
Determine whether `/dp/frankensqlite` plausibly contributes to the tab-lag/flashing incident by correlating deterministic traversal measurements with likely DB-engine hotpaths.

## Scenario
- Bead: `br-legjy.7.1`
- Run timestamp (UTC): `2026-03-02T18:43:32Z`
- Deterministic scenario ID: `20260302_184332`
- Primary workload: full traversal harness for the then-current pre-16-screen TUI with baseline profile capture

## Reproduction Commands
```bash
# Primary deterministic traversal + baseline profile artifacts
E2E_CAPTURE_BASELINE_PROFILE=1 bash scripts/e2e_tui_full_traversal.sh

# Additional consolidated profile report run
bash scripts/profile_tui_traversal.sh
```

## Artifact Index
- `tests/artifacts/tui_full_traversal/20260302_184332/traversal_results.json`
- `tests/artifacts/tui_full_traversal/20260302_184332/baseline_profile_summary.json`
- `tests/artifacts/tui_full_traversal/20260302_184332/cross_layer_attribution_report.json`
- `tests/artifacts/tui_full_traversal/20260302_184332/baseline_profile/baseline_forward_strace.log`
- `tests/artifacts/tui_profile/20260302_134332/profile_report.json`
- `tests/artifacts/tui_profile/20260302_134332/proc_profile.json`

## Measured Signals
From `baseline_profile_summary.json` (scenario `20260302_184332`):
- Quiesce latency: `p50=99.99ms`, `p95=103.41ms`, `p99=103.41ms` (15-step forward pass)
- First-byte latency: `p50=1.61ms`, `p95=13.34ms`
- Render window: `p50=17.71ms`, `p95=18.34ms`
- Syscall profile:
  - `statx=1078`, `pread64=515`, `futex=470`, `poll=210`, `write=56`
  - wait syscalls total: `783` (`short_wait<=5ms: 334`)
  - write bytes returned: `212119`

From `proc_profile.json` (`tests/artifacts/tui_profile/20260302_134332`):
- Process CPU: `mean=3.48%`, `p95=9.98%`, `p99=59.45%`
- Voluntary context switches / sample: `p50=2`, `p95=6`, `p99=67`
- Non-voluntary context switches / sample: `p50=0`, `p95=6`, `p99=27`
- Threads observed: `54`

Highest-quiesce screens in profiled run:
1. Messages: `105.29ms` (`31684` bytes)
2. Search: `101.01ms` (`18501` bytes)
3. Explorer: `100.46ms` (`8142` bytes)
4. Attachments: `100.45ms` (`6832` bytes)
5. Dashboard: `100.40ms` (`12922` bytes)

## DB-Relevant Interpretation
The traversal workload is query-heavy and synchronous in TUI poll snapshots:
- `crates/mcp-agent-mail-server/src/tui_poller.rs` (`fetch_snapshot`, CTE + multi-join queries)

Observed `pread64/statx/futex/poll` volume during deterministic traversal is consistent with DB/FS and synchronization overhead participating in tail latency, even though cross-layer attribution still ranks `/dp/asupersync` and `/dp/frankentui` as dominant first-order bottlenecks.

## frankensqlite Hotspot Candidates (Code Anchors)
1. Parse/plan repeated per query execution without prepared-statement reuse
   - `/dp/frankensqlite/crates/fsqlite-core/src/connection.rs` (`query`, `query_with_params`, `execute_with_params`, `parse_statements`)
   - `/dp/frankensqlite/crates/fsqlite-vdbe/src/engine.rs` (`Opcode::Expire` comment path)
2. JOIN/CTE fallback execution paths
   - `execute_statement_dispatch`, `execute_with_ctes`, `execute_join_select`, `execute_single_join`
3. Row decode + materialization/cloning overhead
   - `cursor_column`, `decode_record`, `Opcode::ResultRow`
4. Read statement lifecycle/locking jitter around autocommit resolution
   - `execute_statement`, `resolve_autocommit_txn`

## Ranked Remediation Candidates
1. Route TUI JOIN/CTE shapes to optimized planner/VDBE path first (reduce fallback frequency)
- Expected impact: high (tail latency)
- Risk: high (semantic edge cases)
- Confidence: medium

2. Add prepared-statement cache keyed by SQL + schema epoch + pragma state
- Expected impact: high (steady-state parse/plan overhead reduction)
- Risk: medium (cache invalidation correctness)
- Confidence: high

3. Reduce row decode + clone churn on result emission path
- Expected impact: medium-high (wide result sets)
- Risk: medium
- Confidence: medium

4. Optimize read-only lifecycle (avoid unnecessary contention on commit-resolution path)
- Expected impact: medium (jitter reduction under mixed load)
- Risk: medium
- Confidence: medium

## Behavior-Isomorphism Proof Plan (for Candidate #2)
Goal: prove statement cache changes are behavior-preserving while improving latency.

1. Differential result equivalence:
- Run a fixed query corpus (including CTE/join/fts/order/limit variants) against pre-change vs post-change builds.
- Compare row-count and row-content hashes for exact match.

2. Invalidation correctness:
- Execute schema-changing statements between repeated query executions.
- Assert cached statement invalidation occurs and result parity remains exact.

3. Transaction semantics:
- Verify autocommit and explicit-transaction cases return identical results and error codes before/after cache.

4. Contention behavior:
- Re-run deterministic traversal harness and ensure no regression in wait syscall/error behavior while measuring quiesce improvements.

## Cross-Reference to Incident SLO Budget
- Current quiesce tail in deterministic traversal is near ~100ms (`p95=103.41ms`) and materially above 16ms frame-budget aspirations; DB engine contributions should be reduced only after preserving semantic invariants.
- Recommended ordering aligns with `cross_layer_attribution_report.json` priorities and keeps frankensqlite work scoped as a cross-project accelerator, not the sole root-cause assumption.

## Failure/Diagnostics Notes
- Harness reported one non-fatal validation line: `FAIL baseline profile: missing expected profiler evidence`.
- Despite that line, expected profile artifacts (`baseline_profile_summary.json`, strace log, traversal JSON) were generated and used in this analysis.
