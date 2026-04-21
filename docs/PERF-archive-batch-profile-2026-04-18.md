# Archive Batch Profile — 2026-04-18

Beads: `br-8qdh0.1` and `br-8qdh0.10`

## Scope

`br-8qdh0.1` established the archive batch profile and root-cause direction.
`br-8qdh0.10` seals the supplement requirements for that profile task:

- complete artifact inventory, including dated copies
- Chrome-tracing-compatible span JSON with `traceEvents`
- six-point scaling coverage at `1/10/50/100/500/1000`
- explicit structured logging contract for future reruns
- exact reproduction commands and a cross-engineer replay target

## Artifact Set

Canonical paths:

- `tests/artifacts/perf/archive_batch_100_flamegraph.svg`
- `tests/artifacts/perf/archive_batch_100_profile.md`
- `tests/artifacts/perf/archive_batch_100_spans.json`
- `tests/artifacts/perf/archive_batch_scaling.csv`

Dated copies added by the supplement:

- `tests/artifacts/perf/archive_batch_100_flamegraph_2026-04-18.svg`
- `tests/artifacts/perf/archive_batch_100_spans_2026-04-18.json`
- `tests/artifacts/perf/archive_batch_scaling_2026-04-18.csv`

Reference cold harness:

- `tests/artifacts/bench/archive/1776505951_367469/summary.json`

## Reproduction

Canonical warm profile command:

```bash
rch exec -- env MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 \
  cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch
```

Targeted flamegraph command:

```bash
env CARGO_TARGET_DIR=/data/projects/mcp_agent_mail_rust/target \
  cargo flamegraph -p mcp-agent-mail --bench benchmarks --root -- archive_write_batch
```

Artifact-emitting verification lane used for this supplement:

```bash
rch exec -- cargo test -p mcp-agent-mail --test archive_perf_reporting -- --nocapture
```

Cross-engineer target:

- Reproduce within `10%` on similar CPU, storage, filesystem, and kernel within `30` days.

## Hardware Fingerprint

Reference benchmark host from [benches/BUDGETS.md](/data/projects/mcp_agent_mail_rust/benches/BUDGETS.md):

- CPU: AMD Ryzen Threadripper PRO 5995WX, 64 cores / 128 threads
- RAM: `499 GiB`
- Storage: `/data` on Samsung SSD 9100 PRO 4TB NVMe
- Filesystem: `btrfs`
- Mount options: `rw,noatime,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/`
- OS / kernel: Ubuntu 25.10, `6.17.0-19-generic`
- Toolchain: nightly Rust 2024 edition; reference compiler `rustc 1.97.0-nightly`

Artifact collection host for this supplement rerun:

- Worker: `ts2`
- Cargo target dir: `/data/projects/mcp_agent_mail_rust/target`
- Filesystem: `btrfs`
- Mount source: `/dev/mapper/ubuntu--vg-ubuntu--lv`
- Mount options: `rw,relatime,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/`
- Kernel: `6.17.0-22-generic`
- Compiler: `rustc 1.97.0-nightly (17584a181 2026-04-13)`

## Warm-Path Measurements

Warm steady-state artifact set from `archive_batch_scaling.csv`:

| Scenario | p50 | p95 | p99 | Throughput |
|----------|-----|-----|-----|------------|
| batch-1 | 22.757 ms | 26.953 ms | 37.847 ms | 43.49 msg/s |
| batch-10 | 146.567 ms | 279.335 ms | 553.122 ms | 58.23 msg/s |
| batch-50 | 685.172 ms | 707.707 ms | 740.782 ms | 72.87 msg/s |
| batch-100 | 1333.646 ms | 2496.186 ms | 5719.149 ms | 54.79 msg/s |
| batch-500 | 6529.399 ms | 6598.762 ms | 6598.762 ms | 77.59 msg/s |
| batch-1000 | 13794.204 ms | 13822.614 ms | 13822.614 ms | 75.40 msg/s |

Scaling law from the supplemented run:

- `batch-10 p95 / batch-1 p95 = 10.36x`
- `batch-50 p95 / batch-1 p95 = 26.26x`
- `batch-100 p95 / batch-1 p95 = 92.61x`
- `batch-500 p95 / batch-1 p95 = 244.82x`
- `batch-1000 p95 / batch-1 p95 = 512.84x`
- Per-message amortization improves again at `50`, `500`, and `1000`, which keeps the overall shape sublinear even though `batch-100` shows a severe tail outlier.

Cold fresh-repo harness reference (`tests/artifacts/bench/archive/1776505951_367469/summary.json`):

| Scenario | p50 | p95 | p99 |
|----------|-----|-----|-----|
| batch-1 | 19.646 ms | 20.666 ms | 21.101 ms |
| batch-10 | 108.389 ms | 113.545 ms | 114.171 ms |
| batch-100 | 1117.878 ms | 1316.594 ms | 1316.594 ms |

The cold harness is still useful for "fresh repo" cost, but the warm-path profile remains the decisive signal for operator traffic.

## Structured Logging Contract

Future archive profile reruns must emit these events:

- `perf.profile.run_start { scenario, rust_version, hardware }`
- `perf.profile.sample_collected { sample_count, duration_sec }`
- `perf.profile.span_summary { span_name, cumulative_micros, count, p50, p95 }`
- `perf.profile.hypothesis_evaluated { name, supports_or_rejects, evidence }`
- `perf.profile.run_complete { duration_sec, artifacts_written }`

The JSON supplement now stores Chrome tracing under `traceEvents` and records `parent` plus `count_per_request` for each span, which makes the file directly ingestible by Chrome trace viewers and downstream analysis scripts.

## Root Cause

The supplemented span summary still points to the same dominant hot path:

- `archive_batch.write_message_batch`: `21.031s` cumulative across 12 `batch-100` samples
- `archive_batch.flush_async_commits`: `868.220ms` cumulative
- `archive_batch.wbq_flush`: `0us` cumulative

That keeps the core diagnosis intact:

- the dominant cost is inside the per-message archive burst itself
- async commit flushing is material but secondary
- final `wbq_flush` waiting is not the next optimization lever

The dated flamegraph copy (`archive_batch_100_flamegraph_2026-04-18.svg`) remains the visual companion for this diagnosis.

## Hypothesis Evaluation

The supplemented JSON now records explicit pass/reject calls for the main perf hypotheses:

- `coalescer batching`: rejected; `write_message_batch` dominates while `flush_async_commits` is secondary
- `fsync per msg`: rejected; `wbq_flush` is effectively absent in the hot sample
- `file layout`: supported; per-message archive burst work still dominates
- `SQLite per-msg txn`: rejected; no SQLite-specific span surfaced in the top categories
- `hashing`: rejected; no hash-heavy span surfaced in the top categories
- `lock thrash`: rejected; the aggregate curve remains sublinear rather than contention-shaped

## Recommended Fix Direction

1. Add a true multi-message archive write path so a burst can reuse manifest/path/staging work instead of repeating the full bundle path for every message.
2. Reduce archive-side allocation and file-layout churn inside the burst before micro-optimizing the terminal flush.
3. Keep `flush_async_commits` instrumentation because it is still the largest secondary component.
4. Do not prioritize `wbq_flush` changes off this data; the supplement makes it even clearer that it is not the bottleneck.

## Notes

- The benchmark binary still has unrelated post-benchmark failure noise in other paths, so this supplement used the targeted `archive_perf_reporting` test lane to materialize the updated CSV and JSON artifacts cleanly.
- `rch` did not sync repo-side artifact files back automatically; the checked-in supplement files were copied from the exact verified worker mirror after the passing test run so the repository now matches the validated output.
