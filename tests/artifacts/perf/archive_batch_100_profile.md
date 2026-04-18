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
- batch-1: p50=22757us, p95=26953us, p99=37847us, p99.9=37847us, p99.99=37847us, max=37847us, samples=40, throughput=43.49 elems/sec
- batch-10: p50=146567us, p95=279335us, p99=553122us, p99.9=553122us, p99.99=553122us, max=553122us, samples=25, throughput=58.23 elems/sec
- batch-50: p50=685172us, p95=707707us, p99=740782us, p99.9=740782us, p99.99=740782us, max=740782us, samples=15, throughput=72.87 elems/sec
- batch-100: p50=1333646us, p95=2496186us, p99=5719149us, p99.9=5719149us, p99.99=5719149us, max=5719149us, samples=12, throughput=54.79 elems/sec
- batch-500: p50=6529399us, p95=6598762us, p99=6598762us, p99.9=6598762us, p99.99=6598762us, max=6598762us, samples=6, throughput=77.59 elems/sec
- batch-1000: p50=13794204us, p95=13822614us, p99=13822614us, p99.9=13822614us, p99.99=13822614us, max=13822614us, samples=4, throughput=75.40 elems/sec
- note: current warm-path sample counts are below 1,000 per scenario, so p99.9/p99.99 act as a conservative worst-observed tail sentinel.

## Structured Logging Requirements
- `perf.profile.run_start`: scenario, rust_version, hardware
- `perf.profile.sample_collected`: sample_count, duration_sec
- `perf.profile.span_summary`: span_name, cumulative_micros, count, p50, p95
- `perf.profile.hypothesis_evaluated`: name, supports_or_rejects, evidence
- `perf.profile.run_complete`: duration_sec, artifacts_written

## Top 10 Spans by Cumulative Duration
- `archive_batch.sample`: cumulative=21900159us, count=12, p50=1333646us, p95=2496187us, avg=1825013us, max=5719149us
- `archive_batch.write_message_batch`: cumulative=21031425us, count=12, p50=1267296us, p95=2412823us, avg=1752618us, max=5632277us
- `archive_batch.write_message_batch_bundle`: cumulative=21031359us, count=12, p50=1267291us, p95=2412818us, avg=1752613us, max=5632271us
- `archive_batch.flush_async_commits`: cumulative=868220us, count=12, p50=71572us, p95=83343us, avg=72351us, max=86849us
- `archive_batch.wbq_flush`: cumulative=0us, count=12, p50=0us, p95=0us, avg=0us, max=0us

## Chrome Trace
- `/data/projects/mcp_agent_mail_rust/tests/artifacts/perf/archive_batch_100_spans.json` includes 60 `traceEvents` records alongside the raw span payloads.

## Hypothesis Evaluation
- `coalescer batching`: rejects (write_message_batch dominates at 21031359us cumulative while flush_async_commits is secondary at 868220us)
- `fsync per msg`: rejects (wbq_flush is only 0us cumulative versus 868220us for flush_async_commits, so the final wait is not the dominant lever)
- `file layout`: supports (per-message archive burst work still dominates the profile at 21031359us cumulative)
- `SQLite per-msg txn`: rejects (no SQLite-specific spans surfaced in the top warm-path categories)
- `hashing`: rejects (hash-oriented spans did not surface in the top categories)
- `lock thrash`: rejects (scaling remains batch-10 p95 is 10.36x batch-1, batch-50 p95 is 26.26x batch-1, batch-100 p95 is 92.61x batch-1, batch-500 p95 is 244.82x batch-1, batch-1000 p95 is 512.84x batch-1; batch-10 amortizes to 1.036x batch-1 per message, batch-50 amortizes to 0.525x batch-1 per message, batch-100 amortizes to 0.926x batch-1 per message, batch-500 amortizes to 0.490x batch-1 per message, batch-1000 amortizes to 0.513x batch-1 per message. Overall scaling remains sublinear through batch-1000., which does not resemble lock-driven blow-up)

## Scaling Law
- batch-10 p95 is 10.36x batch-1, batch-50 p95 is 26.26x batch-1, batch-100 p95 is 92.61x batch-1, batch-500 p95 is 244.82x batch-1, batch-1000 p95 is 512.84x batch-1; batch-10 amortizes to 1.036x batch-1 per message, batch-50 amortizes to 0.525x batch-1 per message, batch-100 amortizes to 0.926x batch-1 per message, batch-500 amortizes to 0.490x batch-1 per message, batch-1000 amortizes to 0.513x batch-1 per message. Overall scaling remains sublinear through batch-1000.
