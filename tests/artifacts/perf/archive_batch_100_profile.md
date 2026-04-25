# Archive Batch 100 Profile

## Reproduction
- warm profile: `rch exec -- env CARGO_TARGET_DIR=/data/tmp/cargo-target MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch`
- flamegraph: `env CARGO_TARGET_DIR=/data/tmp/cargo-target cargo flamegraph -p mcp-agent-mail --bench benchmarks --root -- archive_write_batch`
- dry-run validated: yes
- cross-engineer target: Reproduce within 10% on similar CPU/storage/filesystem/kernel hardware within 30 days

## Environment
- repo root: `/data/projects/mcp_agent_mail_rust`
- cargo target dir: `/data/tmp/cargo-target`
- rustc: `rustc 1.97.0-nightly (36ba2c771 2026-04-23); binary: rustc; commit-hash: 36ba2c7712052d731a7082d0eba5ed3d9d56c133; commit-date: 2026-04-23; host: x86_64-unknown-linux-gnu; release: 1.97.0-nightly; LLVM version: 22.1.2`
- kernel: `6.17.0-19-generic`
- filesystem: `btrfs`
- mount source: `/dev/nvme0n1`
- mount options: `rw,noatime,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/`
- storage model: `Samsung SSD 9100 PRO 4TB`
- storage transport: `nvme`

## Batch Comparison
- batch-1: p50=5527us, p95=5695us, p99=10788us, p99.9=10788us, p99.99=10788us, max=10788us, samples=40, throughput=176.39 elems/sec
- batch-10: p50=17515us, p95=18844us, p99=18957us, p99.9=18957us, p99.99=18957us, max=18957us, samples=25, throughput=645.85 elems/sec
- batch-50: p50=46656us, p95=53356us, p99=56611us, p99.9=56611us, p99.99=56611us, max=56611us, samples=15, throughput=1026.87 elems/sec
- batch-100: p50=91010us, p95=97604us, p99=104435us, p99.9=104435us, p99.99=104435us, max=104435us, samples=12, throughput=1118.46 elems/sec
- batch-500: p50=392984us, p95=401047us, p99=401047us, p99.9=401047us, p99.99=401047us, max=401047us, samples=6, throughput=1302.75 elems/sec
- batch-1000: p50=750625us, p95=774037us, p99=774037us, p99.9=774037us, p99.99=774037us, max=774037us, samples=4, throughput=1343.26 elems/sec
- note: current warm-path sample counts are below 1,000 per scenario, so p99.9/p99.99 act as a conservative worst-observed tail sentinel.

## Structured Logging Requirements
- `perf.profile.run_start`: scenario, rust_version, hardware
- `perf.profile.sample_collected`: sample_count, duration_sec
- `perf.profile.span_summary`: span_name, cumulative_micros, count, p50, p95
- `perf.profile.hypothesis_evaluated`: name, supports_or_rejects, evidence
- `perf.profile.run_complete`: duration_sec, artifacts_written

## Top 10 Spans by Cumulative Duration
- `archive_batch.sample`: cumulative=1072909us, count=12, p50=91011us, p95=97604us, avg=89409us, max=104435us
- `archive_batch.flush_async_commits`: cumulative=815845us, count=12, p50=71574us, p95=76865us, avg=67987us, max=81882us
- `archive_batch.write_message_batch`: cumulative=256640us, count=12, p50=21273us, p95=22554us, avg=21386us, max=23178us
- `archive_batch.write_message_batch_bundle`: cumulative=256576us, count=12, p50=21266us, p95=22547us, avg=21381us, max=23173us
- `archive_batch.wbq_flush`: cumulative=0us, count=12, p50=0us, p95=0us, avg=0us, max=0us

## Chrome Trace
- `/data/projects/mcp_agent_mail_rust/tests/artifacts/perf/archive_batch_100_spans.json` includes 60 `traceEvents` records alongside the raw span payloads.

## Hypothesis Evaluation
- `coalescer batching`: supports (flush_async_commits remains material at 815845us cumulative versus 256576us in write_message_batch)
- `fsync per msg`: rejects (wbq_flush is only 0us cumulative versus 815845us for flush_async_commits, so the final wait is not the dominant lever)
- `file layout`: rejects (layout work does not dominate; commit/flush work is larger at 815845us)
- `SQLite per-msg txn`: rejects (no SQLite-specific spans surfaced in the top warm-path categories)
- `hashing`: rejects (hash-oriented spans did not surface in the top categories)
- `lock thrash`: rejects (scaling remains batch-10 p95 is 3.31x batch-1, batch-50 p95 is 9.37x batch-1, batch-100 p95 is 17.14x batch-1, batch-500 p95 is 70.42x batch-1, batch-1000 p95 is 135.92x batch-1; batch-10 amortizes to 0.331x batch-1 per message, batch-50 amortizes to 0.187x batch-1 per message, batch-100 amortizes to 0.171x batch-1 per message, batch-500 amortizes to 0.141x batch-1 per message, batch-1000 amortizes to 0.136x batch-1 per message. Overall scaling remains sublinear through batch-1000., which does not resemble lock-driven blow-up)

## Scaling Law
- batch-10 p95 is 3.31x batch-1, batch-50 p95 is 9.37x batch-1, batch-100 p95 is 17.14x batch-1, batch-500 p95 is 70.42x batch-1, batch-1000 p95 is 135.92x batch-1; batch-10 amortizes to 0.331x batch-1 per message, batch-50 amortizes to 0.187x batch-1 per message, batch-100 amortizes to 0.171x batch-1 per message, batch-500 amortizes to 0.141x batch-1 per message, batch-1000 amortizes to 0.136x batch-1 per message. Overall scaling remains sublinear through batch-1000.
