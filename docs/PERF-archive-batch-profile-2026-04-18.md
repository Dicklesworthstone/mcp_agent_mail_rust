# Archive Batch Profile — 2026-04-18

Bead: `br-8qdh0.1`

## Scope

Profile the `archive_write_batch/batch_no_attachments/100` path and explain why archive batch
writes were still near or above budget depending on measurement methodology.

## Artifacts

- `tests/artifacts/perf/archive_batch_100_spans.json`
- `tests/artifacts/perf/archive_batch_scaling.csv`
- `tests/artifacts/perf/archive_batch_100_flamegraph.svg`
- `tests/artifacts/perf/archive_batch_100_profile.md`
- `tests/artifacts/bench/archive/1776505951_367469/summary.json`

## Key Measurements

Warm steady-state profile (`MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch`):

| Scenario | p50 | p95 | p99 |
|----------|-----|-----|-----|
| batch-1 | 11.916 ms | 12.701 ms | 14.964 ms |
| batch-10 | 33.073 ms | 38.239 ms | 38.879 ms |
| batch-100 | 220.397 ms | 264.709 ms | 265.006 ms |

Scaling law:

- `batch-10 p95 / batch-1 p95 = 3.01x`
- `batch-100 p95 / batch-1 p95 = 20.84x`
- Per-message cost at batch-100 is ~`0.208x` batch-1, so batching is helping materially.

Cold fresh-repo harness (`tests/artifacts/bench/archive/1776505951_367469/summary.json`):

| Scenario | p50 | p95 | p99 |
|----------|-----|-----|-----|
| batch-1 | 19.646 ms | 20.666 ms | 21.101 ms |
| batch-10 | 108.389 ms | 113.545 ms | 114.171 ms |
| batch-100 | 1117.878 ms | 1316.594 ms | 1316.594 ms |

The cold harness is still far above budget because each sample uses a fresh repo and pays
burst-local archive/git churn from scratch. The warm-path profile is the more decisive signal for
steady-state operator traffic.

## Root Cause

The span trace shows the warm batch-100 path is not blocked on `wbq_flush`:

- `archive_batch.write_message_loop`: ~2.04s cumulative across 12 samples
- `archive_batch.flush_async_commits`: ~0.61s cumulative across 12 samples
- `archive_batch.wbq_flush`: ~420us cumulative across 12 samples

That means the dominant cost is still inside the per-message archive write burst itself, with the
explicit commit flush remaining material but clearly secondary.

The targeted flamegraph (`tests/artifacts/perf/archive_batch_100_flamegraph.svg`) reinforces that:

- Allocator churn is the largest visible userspace family: `_int_malloc` ~5.66%, `realloc`
  ~1.53%, `free` / `_int_free_chunk` ~2.19%.
- `commit-coalesce` is visible at ~2.73%.
- `git` stacks are visible at ~1.41%.
- Filesystem cleanup/sync stacks remain significant: `unlinkat` ~1.08%, `fsync` ~0.71%,
  `rename`, `readdir`, and `open64` all show up in the targeted path.

Interpretation:

- The batch path is no longer "zero amortization"; batching already removes a large amount of
  per-message cost.
- The remaining over-budget tail is mainly per-message archive/object churn plus git/coalescer
  cleanup work inside the burst.
- `wbq_flush` is not the next lever.

## Recommended Fix Direction

1. Add a true multi-message archive write path so one burst can reuse path computation, manifest
   assembly, and archive staging work instead of repeating the full `write_message_bundle` flow
   100 times.
2. Reduce git cleanup churn inside the burst. The flamegraph showing `commit-coalesce`, `git`,
   `unlinkat`, and `rename` means the next meaningful win is to stage more changes before git-side
   cleanup/index work, not to micro-optimize the final wait.
3. Attack allocator pressure in the burst. The warm flamegraph makes `_int_malloc` / `realloc` /
   `free` too visible to ignore; pre-sizing and reusing per-message buffers is a plausible next
   lever once the batch write boundary is explicit.
4. Leave `wbq_flush` alone for now. The span trace makes it clear that it is in the noise floor.

## Notes

- The benchmark binary still panics after the selected archive benchmark in an unrelated share
  benchmark path (`apply_project_scope` at `crates/mcp-agent-mail/benches/benchmarks.rs:2251`).
  The archive profiling artifacts were emitted before that panic, and the flamegraph was captured
  with `cargo flamegraph --ignore-status` for that reason.
