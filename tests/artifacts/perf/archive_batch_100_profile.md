# Archive Batch 100 Profile

## Reproduction
- warm profile: `rch exec -- env CARGO_TARGET_DIR=/data/projects/mcp_agent_mail_rust/target MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch`
- flamegraph: `env CARGO_TARGET_DIR=/data/projects/mcp_agent_mail_rust/target cargo flamegraph -p mcp-agent-mail --bench benchmarks --root -- archive_write_batch`
- dry-run validated: yes
- cross-engineer target: Reproduce within 10% on similar CPU/storage/filesystem/kernel hardware within 30 days

## Environment
- repo root: `/data/projects/mcp_agent_mail_rust`
- cargo target dir: `/data/projects/mcp_agent_mail_rust/target`
- rustc: `rustc 1.97.0-nightly (17584a181 2026-04-13); binary: rustc; commit-hash: 17584a181979f04f2aaad867332c22db1caa511a; commit-date: 2026-04-13; host: x86_64-unknown-linux-gnu; release: 1.97.0-nightly; LLVM version: 22.1.2`
- kernel: `6.17.0-22-generic`
- filesystem: `btrfs`
- mount source: `/dev/mapper/ubuntu--vg-ubuntu--lv`
- mount options: `rw,relatime,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/`

## Batch Comparison
- batch-1: p50=23307us, p95=27101us, p99=28650us, p99.9=28650us, p99.99=28650us, max=28650us, samples=40, throughput=43.71 elems/sec
- batch-10: p50=142453us, p95=154429us, p99=159740us, p99.9=159740us, p99.99=159740us, max=159740us, samples=25, throughput=69.99 elems/sec
- batch-50: p50=682683us, p95=1490815us, p99=1925691us, p99.9=1925691us, p99.99=1925691us, max=1925691us, samples=15, throughput=60.88 elems/sec
- batch-100: p50=1327095us, p95=3491133us, p99=7640550us, p99.9=7640550us, p99.99=7640550us, max=7640550us, samples=12, throughput=45.72 elems/sec
- batch-500: p50=6438268us, p95=7088480us, p99=7088480us, p99.9=7088480us, p99.99=7088480us, max=7088480us, samples=6, throughput=75.96 elems/sec
- batch-1000: p50=13091095us, p95=14461498us, p99=14461498us, p99.9=14461498us, p99.99=14461498us, max=14461498us, samples=4, throughput=74.67 elems/sec
- note: current warm-path sample counts are below 1,000 per scenario, so p99.9/p99.99 act as a conservative worst-observed tail sentinel.

## Structured Logging Requirements
- `perf.profile.run_start`: scenario, rust_version, hardware
- `perf.profile.sample_collected`: sample_count, duration_sec
- `perf.profile.span_summary`: span_name, cumulative_micros, count, p50, p95
- `perf.profile.hypothesis_evaluated`: name, supports_or_rejects, evidence
- `perf.profile.run_complete`: duration_sec, artifacts_written

## Top 10 Spans by Cumulative Duration
- `archive_batch.sample`: cumulative=26244037us, count=12, p50=1327095us, p95=3491133us, avg=2187003us, max=7640551us
- `archive_batch.write_message_batch`: cumulative=25369805us, count=12, p50=1259913us, p95=3423314us, avg=2114150us, max=7558270us
- `archive_batch.write_message_batch_bundle`: cumulative=25369726us, count=12, p50=1259907us, p95=3423308us, avg=2114143us, max=7558264us
- `archive_batch.flush_async_commits`: cumulative=873628us, count=12, p50=72368us, p95=82217us, avg=72802us, max=82513us
- `archive_batch.wbq_flush`: cumulative=0us, count=12, p50=0us, p95=0us, avg=0us, max=0us

## Chrome Trace
- `/data/projects/mcp_agent_mail_rust/tests/artifacts/perf/archive_batch_100_spans.json` includes 60 `traceEvents` records alongside the raw span payloads.

## Hypothesis Evaluation
- `coalescer batching`: rejects (write_message_batch dominates at 25369726us cumulative while flush_async_commits is secondary at 873628us)
- `fsync per msg`: rejects (wbq_flush is only 0us cumulative versus 873628us for flush_async_commits, so the final wait is not the dominant lever)
- `file layout`: supports (per-message archive burst work still dominates the profile at 25369726us cumulative)
- `SQLite per-msg txn`: rejects (no SQLite-specific spans surfaced in the top warm-path categories)
- `hashing`: rejects (hash-oriented spans did not surface in the top categories)
- `lock thrash`: rejects (scaling remains batch-10 p95 is 5.70x batch-1, batch-50 p95 is 55.01x batch-1, batch-100 p95 is 128.82x batch-1, batch-500 p95 is 261.56x batch-1, batch-1000 p95 is 533.61x batch-1; batch-10 amortizes to 0.570x batch-1 per message, batch-50 amortizes to 1.100x batch-1 per message, batch-100 amortizes to 1.288x batch-1 per message, batch-500 amortizes to 0.523x batch-1 per message, batch-1000 amortizes to 0.534x batch-1 per message. Overall scaling remains sublinear through batch-1000., which does not resemble lock-driven blow-up)

## Scaling Law
- batch-10 p95 is 5.70x batch-1, batch-50 p95 is 55.01x batch-1, batch-100 p95 is 128.82x batch-1, batch-500 p95 is 261.56x batch-1, batch-1000 p95 is 533.61x batch-1; batch-10 amortizes to 0.570x batch-1 per message, batch-50 amortizes to 1.100x batch-1 per message, batch-100 amortizes to 1.288x batch-1 per message, batch-500 amortizes to 0.523x batch-1 per message, batch-1000 amortizes to 0.534x batch-1 per message. Overall scaling remains sublinear through batch-1000.
