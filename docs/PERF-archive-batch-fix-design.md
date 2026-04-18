# Archive Batch Write Perf Fix Design

Bead: `br-8qdh0.2`

Inputs:
- `docs/PERF-archive-batch-profile-2026-04-18.md`
- `tests/artifacts/perf/archive_batch_100_spans.json`
- `tests/artifacts/perf/archive_batch_100_profile.md`
- `tests/artifacts/perf/archive_batch_100_flamegraph.svg`

## Problem Statement

`br-8qdh0.1` established that archive batch writes are close to the warm steady-state budget but
still over it:

| Scenario | p50 | p95 | p99 |
|----------|-----|-----|-----|
| batch-1 | 11.916 ms | 12.701 ms | 14.964 ms |
| batch-100 | 220.397 ms | 264.709 ms | 265.006 ms |

The warm-path `batch-100` p95 misses the current `< 250ms` budget by ~`14.7ms`.

The cold fresh-repo harness is much worse (`1316.594ms` p95), but that path pays repo bootstrap and
filesystem churn from scratch on every sample. The implementation bead should optimize the warm
steady-state path first, because that is the operator-facing hot path and the decisive signal from
the profile.

## Root Cause

The profile does not support a “just tune the coalescer” diagnosis.

Warm-path span rollup:
- `archive_batch.write_message_loop`: `2039448us` cumulative, `77.0%` of sampled wall time
- `archive_batch.write_message_bundle`: `2035491us` cumulative, `76.8%`
- `archive_batch.flush_async_commits`: `608879us` cumulative, `23.0%`
- `archive_batch.wbq_flush`: `420us`, effectively noise

Code inspection explains why the write loop still dominates:
- [`write_message_bundle`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs:4315) expands every message independently.
- For each message it computes archive paths via [`message_paths`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs:4237).
- It then performs separate canonical/outbox/inbox writes, each going through [`write_text`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs:7361) and atomic temp-file write/rename with `sync_data()`.
- Only after all that does it call [`enqueue_async_commit`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs:2932).

The coalescer is therefore downstream of the real bottleneck. In the benchmark path it already has
the ability to merge distinct relpaths into one commit via
[`coalescer_commit_batch`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs:2548),
but it receives 100 already-expanded requests after the storage layer has paid almost all of the
per-message work.

The flamegraph is consistent with that interpretation:
- allocator churn is prominent (`_int_malloc`, `realloc`, `_int_free_chunk`)
- git/coalescer work is real but secondary (`commit-coalesce`, `git`, `unlinkat`, `rename`, `fsync`)
- `wbq_flush` is not a hotspot

## Design Goal

Move the batching boundary earlier, into the storage message-write path, so one logical batch:
- writes all message artifacts in one storage call
- builds one merged relpath list
- enqueues one async commit request instead of 100
- reuses buffers and path bookkeeping across the burst

This should turn the coalescer into a complement to batching instead of the first place batching is
attempted.

## Fix A: Add A Native Batch Message Write API

Recommended entrypoint:
- `write_message_batch_bundle(...)` in `mcp-agent-mail-storage`

Mechanism:
- accept a slice of prevalidated message/body records instead of one message
- compute all `MessageArchivePaths` in one pass
- pre-size `rel_paths` for the full batch instead of growing many small vectors
- write canonical/outbox/inbox copies in a tight loop
- enqueue exactly one async commit for the merged relpath set

Implementation outline:
1. Extract the shared per-message planning work from `write_message_bundle` into an internal helper that returns a write plan rather than immediately writing and enqueueing.
2. Add a batch entrypoint that consumes many plans, writes all files, accumulates relpaths, and issues one combined commit request.
3. Keep the single-message path as a thin caller of the same internal helper so the semantics stay identical while the batch path gets the new fast lane.
4. Update the batch benchmark and any true batch call sites to use the batch API instead of looping over `write_message_bundle`.

Why this is the primary lever:
- the measured hotspot is the write loop itself, not post-loop waiting
- `write_message_bundle` currently performs repeated path computation, vector growth, commit message construction, and atomic file-write setup per message
- reducing that repeated work by even `15-20%` lowers total warm `batch-100` p95 by roughly `30-40ms`, which is enough to get materially under budget

Expected gain:
- warm `batch-100` p95 from `264.7ms` to roughly `220-235ms`

## Fix B: Collapse Per-Request Commit Overhead Inside The Batch API

Mechanism:
- once the batch API has the merged relpath set, build one combined commit message and call `enqueue_async_commit` once
- avoid creating 100 `CoalescerCommitFields`, 100 small message summaries, and 100 per-request relpath vectors only to have the coalescer merge them later

Why this is complementary rather than sufficient alone:
- `flush_async_commits` is only `23%` of sampled wall time
- reducing coalescer work without changing the storage write loop leaves the dominant `76.8%` untouched

Expected gain:
- roughly `8-20ms` p95 on warm `batch-100`
- best used together with Fix A, not as a standalone implementation bead

## Recommended Fix For br-8qdh0.3

Recommended scope: `Fix A + Fix B` together.

Concrete recommendation:
- implement a native batch storage API
- make the batch benchmark call that API directly
- issue one async commit request per logical batch

This is the smallest change that is directly supported by the profile data and should create
meaningful budget headroom rather than barely shaving `15ms`.

## Non-Goals

- Do not relax archive durability guarantees just to avoid `sync_data()`.
- Do not redesign the git-backed archive format.
- Do not rewrite the commit coalescer worker model.
- Do not optimize `wbq_flush`; the profile says it is noise.
- Do not fold attachment-pipeline work into this bead unless the new batch API needs a narrow hook for future use.

## Risks

- If the batch API changes write ordering, archive history or thread rendering could drift.
- If the batch API merges relpaths incorrectly, the git commit may omit files or hide deletions.
- If a future batch includes repeated `thread_id` updates, thread digest writes may become a path-conflict edge case even though the current benchmark path does not exercise it.

## Test Plan

Existing coverage to preserve:
- [`test_write_message_bundle`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs:8426)
- BCC/privacy and whitespace preservation tests in the same module
- coalescer batch tests around [`coalescer_commit_batch`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs:13193)
- `archive_write_batch` benchmark in [benchmarks.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail/benches/benchmarks.rs:1321)

New coverage required for `br-8qdh0.3`:
- `write_message_batch_bundle_matches_single_message_layout`
- `write_message_batch_bundle_uses_single_async_commit_request`
- `write_message_batch_bundle_preserves_bcc_redaction_and_body_bytes`
- `write_message_batch_bundle_handles_thread_digest_updates_in_order`

Verification plan for the implementation bead:
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_ivsummit_perf cargo check -p mcp-agent-mail --bench benchmarks`
- `rch exec -- cargo test -p mcp-agent-mail-storage`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_ivsummit_perf cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch`
- update `benches/BUDGETS.md` only after the new numbers are captured

## Decision

Selected fix direction for `br-8qdh0.3`:
- implement a storage-native batch message write API
- reuse shared internal planning logic with the single-message path
- enqueue a single async commit per logical batch

This is the highest-confidence route from the current `264.7ms` warm p95 to a stable under-budget
result without changing archive semantics.
