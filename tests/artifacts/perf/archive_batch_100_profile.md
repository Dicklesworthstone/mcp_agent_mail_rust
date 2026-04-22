# Archive Batch 100 Profile

## Reproduction
- warm profile: `rch exec -- env MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch`
- flamegraph: `env CARGO_TARGET_DIR=/data/tmp/cargo-target cargo flamegraph -p mcp-agent-mail --bench benchmarks --root -- archive_write_batch`
- dry-run validated: yes
- cross-engineer target: Reproduce within 10% on similar CPU/storage/filesystem/kernel hardware within 30 days

## Environment
- repo root: `/data/projects/mcp_agent_mail_rust`
- cargo target dir: `/data/tmp/cargo-target`
- rustc: `rustc 1.97.0-nightly (17584a181 2026-04-13); binary: rustc; commit-hash: 17584a181979f04f2aaad867332c22db1caa511a; commit-date: 2026-04-13; host: x86_64-unknown-linux-gnu; release: 1.97.0-nightly; LLVM version: 22.1.2`
- kernel: `6.17.0-22-generic`
- filesystem: `btrfs`
- mount source: `/dev/mapper/ubuntu--vg-ubuntu--lv`
- mount options: `rw,relatime,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/`

## Batch Comparison
- batch-1: p50=73962us, p95=89011us, p99=98489us, p99.9=98489us, p99.99=98489us, max=98489us, samples=40, throughput=13.31 elems/sec
- batch-10: p50=865822us, p95=1408371us, p99=1479396us, p99.9=1479396us, p99.99=1479396us, max=1479396us, samples=25, throughput=10.82 elems/sec
- batch-50: p50=709445us, p95=3740131us, p99=4354253us, p99.9=4354253us, p99.99=4354253us, max=4354253us, samples=15, throughput=33.85 elems/sec
- batch-100: p50=1357254us, p95=1409432us, p99=1413710us, p99.9=1413710us, p99.99=1413710us, max=1413710us, samples=12, throughput=73.23 elems/sec
- batch-500: p50=6421551us, p95=7002085us, p99=7002085us, p99.9=7002085us, p99.99=7002085us, max=7002085us, samples=6, throughput=76.90 elems/sec
- batch-1000: p50=13231423us, p95=13235783us, p99=13235783us, p99.9=13235783us, p99.99=13235783us, max=13235783us, samples=4, throughput=76.59 elems/sec
- note: current warm-path sample counts are below 1,000 per scenario, so p99.9/p99.99 act as a conservative worst-observed tail sentinel.

## Structured Logging Requirements
- `perf.profile.run_start`: scenario, rust_version, hardware
- `perf.profile.sample_collected`: sample_count, duration_sec
- `perf.profile.span_summary`: span_name, cumulative_micros, count, p50, p95
- `perf.profile.hypothesis_evaluated`: name, supports_or_rejects, evidence
- `perf.profile.run_complete`: duration_sec, artifacts_written

## Top 10 Spans by Cumulative Duration
- `archive_batch.sample`: cumulative=16386716us, count=12, p50=1357254us, p95=1409432us, avg=1365559us, max=1413710us
- `archive_batch.write_message_batch`: cumulative=15505801us, count=12, p50=1282347us, p95=1332131us, avg=1292150us, max=1346568us
- `archive_batch.write_message_batch_bundle`: cumulative=15505721us, count=12, p50=1282340us, p95=1332124us, avg=1292143us, max=1346562us
- `archive_batch.flush_async_commits`: cumulative=880388us, count=12, p50=77251us, p95=77812us, avg=73365us, max=77970us
- `archive_batch.wbq_flush`: cumulative=1us, count=12, p50=0us, p95=0us, avg=0us, max=1us

## Chrome Trace
- `/data/projects/mcp_agent_mail_rust/tests/artifacts/perf/archive_batch_100_spans.json` includes 60 `traceEvents` records alongside the raw span payloads.

## Hypothesis Evaluation
- `coalescer batching`: rejects (write_message_batch dominates at 15505721us cumulative while flush_async_commits is secondary at 880388us)
- `fsync per msg`: rejects (wbq_flush is only 1us cumulative versus 880388us for flush_async_commits, so the final wait is not the dominant lever)
- `file layout`: supports (per-message archive burst work still dominates the profile at 15505721us cumulative)
- `SQLite per-msg txn`: rejects (no SQLite-specific spans surfaced in the top warm-path categories)
- `hashing`: rejects (hash-oriented spans did not surface in the top categories)
- `lock thrash`: rejects (scaling remains batch-10 p95 is 15.82x batch-1, batch-50 p95 is 42.02x batch-1, batch-100 p95 is 15.83x batch-1, batch-500 p95 is 78.67x batch-1, batch-1000 p95 is 148.70x batch-1; batch-10 amortizes to 1.582x batch-1 per message, batch-50 amortizes to 0.840x batch-1 per message, batch-100 amortizes to 0.158x batch-1 per message, batch-500 amortizes to 0.157x batch-1 per message, batch-1000 amortizes to 0.149x batch-1 per message. Overall scaling remains sublinear through batch-1000., which does not resemble lock-driven blow-up)

## Scaling Law
- batch-10 p95 is 15.82x batch-1, batch-50 p95 is 42.02x batch-1, batch-100 p95 is 15.83x batch-1, batch-500 p95 is 78.67x batch-1, batch-1000 p95 is 148.70x batch-1; batch-10 amortizes to 1.582x batch-1 per message, batch-50 amortizes to 0.840x batch-1 per message, batch-100 amortizes to 0.158x batch-1 per message, batch-500 amortizes to 0.157x batch-1 per message, batch-1000 amortizes to 0.149x batch-1 per message. Overall scaling remains sublinear through batch-1000.
