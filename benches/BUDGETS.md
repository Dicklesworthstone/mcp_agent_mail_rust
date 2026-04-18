# Performance Budgets

Baseline performance targets for mcp-agent-mail Rust port.
Updated via native `am bench` and `cargo bench`.

## Optimization Workflow

1. **Profile** — measure before changing anything
2. **Change** — apply one optimization
3. **Prove** — verify behavior unchanged (golden outputs) AND performance improved

## Hardware + Environment Baseline

All archive-write numbers below are tied to the reference host and should not be
treated as portable absolutes.

- **CPU**: AMD Ryzen Threadripper PRO 5995WX, 64 cores / 128 threads, boost enabled, `performance` governor (`413-4575 MHz` reported by `lscpu`)
- **RAM**: `499 GiB` system memory, `63 GiB` swap
- **Storage**: workspace on `/data` -> `/dev/nvme0n1`, Samsung SSD 9100 PRO 4TB NVMe, `btrfs`, mount options `rw,noatime,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/`
- **OS**: Ubuntu 25.10 (Questing Quokka), kernel `6.17.0-19-generic`
- **Rust toolchain**: `rust-toolchain.toml` pins `nightly`; reference compiler was `rustc 1.97.0-nightly (e9e32aca5 2026-04-17)`
- **Build profile**: `cargo bench` with `[profile.bench] inherits = "release"`; effective settings are `opt-level=3`, `lto="thin"`, `codegen-units=1`, `panic="abort"`, `strip="symbols"`
- **Workload isolation**: bare host, no taskset/cgroup pinning, remote execution via `rch exec`, target dir currently `/tmp/cargo-target`
- **Runtime**: wall-clock Criterion measurement; warm-path side-artifacts are enabled with `MCP_AGENT_MAIL_ARCHIVE_PROFILE=1`
- **Canonical wrapper**: `scripts/bench_baseline.sh`
- **Latest successful cold-harness bundle**: `tests/artifacts/bench/archive/1776505951_367469/summary.json`
- **Latest warm-path profile artifacts**: `tests/artifacts/perf/archive_batch_100_profile.md`, `tests/artifacts/perf/archive_batch_scaling.csv`, `tests/artifacts/perf/archive_batch_100_spans.json`

### Re-running the archive baseline

Use the canonical wrapper:

```bash
scripts/bench_baseline.sh
```

If the shared checkout is temporarily non-buildable, create a clean detached
worktree at the desired source commit and point the wrapper at it:

```bash
scripts/bench_baseline.sh --bench-root /abs/path/to/clean/worktree
```

The wrapper:

1. Captures the hardware + environment fingerprint
2. Runs `MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 rch exec -- cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch`
3. Copies the `archive_batch_*` raw artifacts into `tests/artifacts/bench/archive_baseline/<run_id>/`
4. Emits `fingerprint.json` and `summary.json` for machine diffing / future perf-gate ingestion

Current blocker on `2026-04-18`:
- fresh reruns from the shared checkout and clean detached worktrees at `f19dd828`, `1d83c6b7`, and `99bc2663` all fail before measurement because `mcp-agent-mail-server` expects `metrics::AtcMetricsSnapshot` / `global_metrics().atc`
- until that compile break is fixed, treat `tests/artifacts/bench/archive/1776505951_367469/summary.json` as the authoritative measured baseline and use new `archive_baseline/<run_id>/summary.json` bundles as blocker artifacts, not as latency evidence

### Variance Envelope

- Treat `batch-100 p95 ~= 238.1ms` on the same CPU/storage/filesystem/kernel/governor as the reference steady-state target.
- Treat `<= 10%` p95 drift on the same host as noise.
- Investigate `> 10%` drift; escalate at `>= 20%` or after three consecutive `> 10%` runs.
- Cross-host comparisons are advisory only; NVMe model, filesystem, mount options, and kernel materially affect `fsync`-heavy workloads.
- If a same-host rerun breaches the envelope after an archive-path change, follow [docs/OPERATOR_RUNBOOK.md](/data/projects/mcp_agent_mail_rust/docs/OPERATOR_RUNBOOK.md#emergency-roll-back-archive-batch-write-optimization).

### Cross-Filesystem Fsync Matrix (br-8qdh0.11)

Cross-filesystem archive verification is driven by:

- `crates/mcp-agent-mail-storage/tests/fsync_matrix.rs`
- `scripts/bench_archive_fsync_matrix.sh`
- `.github/workflows/archive-fsync-matrix.yml`

The probe records:

1. single-message write latency (`p50`/`p95`/`p99`)
2. batch-100 write latency (`p50`/`p95`/`p99`)
3. a crash-after-`flush_async_commits()` durability check that reopens the archive after a forced child-process kill

CI-covered budgets:

| Filesystem | CI coverage | fsync mode | Single p95 | Batch-100 p95 | Notes |
|-----------|-------------|------------|------------|---------------|-------|
| ext4 (`data=ordered`) | Linux loopback | `normal` | < 25ms | < 250ms | Baseline Linux recommendation |
| ext4 (`data=journal`) | Linux loopback | `normal` | < 40ms | < 350ms | Higher journal cost; use when durability policy is stricter than latency policy |
| xfs | Linux loopback | `normal` | < 25ms | < 250ms | Recommended for low-variance Linux server writes |
| btrfs | Linux loopback | `normal` | < 50ms | < 500ms | Supported but slower under CoW + metadata pressure |
| APFS | macOS runner | `barrier_only` | < 35ms | < 300ms | Durable rename + barrier semantics differ from Linux flush semantics |
| tmpfs | Linux tmpfs | `buffered` | < 15ms | < 150ms | Canary only; not durable and still pays Git/process overhead, so treat it as a logic floor rather than a near-zero write budget |

Documented but not yet CI-gated:

| Filesystem | Current stance | Notes |
|-----------|----------------|-------|
| NTFS / WSL | Manual spot-check only | Use for portability evidence, not for release gating yet |
| ZFS | Unsupported initially | Add only when a stable runner or dedicated lab host exists |

Operator rules of thumb:

- Use ext4 (`data=ordered`) or xfs when archive write latency is the deciding constraint.
- Treat btrfs as supported-but-slower; compare against the btrfs row above instead of the ext4 baseline.
- Treat tmpfs results as a logic/CPU canary only. A tmpfs pass does **not** say anything about crash durability.
- If only one filesystem regresses, use the per-FS artifact bundle from the workflow before widening the global budget or rolling back the whole archive-path change.

## Tool Handler Budgets

Targets based on initial baseline (2026-02-05). Budgets are 2x the measured baseline to absorb variance.

| Surface | Baseline | Budget | Notes |
|---------|----------|--------|-------|
| Format resolution (explicit) | ~39ns | < 100ns | Pure string matching, no I/O |
| Format resolution (implicit) | ~20ns | < 50ns | Fast path: no param, no default |
| Format resolution (MIME alias) | ~36ns | < 100ns | Includes normalize_mime() |
| Stats parsing (full) | ~243ns | < 500ns | 2 lines: token estimates + saved |
| Stats parsing (noisy) | ~293ns | < 600ns | 4 lines, scan with noise |
| Stats parsing (empty) | ~12ns | < 30ns | Early return |
| Encoder resolution (default) | ~30ns | < 100ns | Single string |
| Encoder resolution (custom) | ~92ns | < 200ns | whitespace split |
| Stub encoder (subprocess) | ~12ms | < 25ms | Fork+exec+pipe |
| apply_toon_format (toon) | ~12ms | < 25ms | Includes subprocess I/O |
| apply_toon_format (json) | ~27ns | < 60ns | Passthrough, no I/O |
| JSON serialize (8-field) | ~246ns | < 500ns | serde_json baseline |
| JSON parse (8-field) | ~553ns | < 1.2µs | serde_json baseline |

## CLI Startup Budgets

| Command | Target | Notes |
|---------|--------|-------|
| `am --help` | < 20ms | Startup + argument parsing |
| `am lint` | < 50ms | Static analysis |
| `am typecheck` | < 50ms | Type checking |

## Migration Command-Surface Guardrails (T10.9)

Guardrails for migrated command surfaces are enforced by:

```bash
cargo test -p mcp-agent-mail-cli --test perf_guardrails -- --nocapture
```

Artifacts are emitted under:
- `tests/artifacts/cli/perf_guardrails/<run_id>/perf_guardrails.json`
- `tests/artifacts/cli/perf_guardrails/trends/perf_guardrails_timeseries.jsonl`

| Surface | Native workload | Native p95 budget | Legacy comparator | Max native-vs-legacy delta p95 |
|---------|-----------------|-------------------|-------------------|---------------------------------|
| `ci_help` | `am ci --help` | < 400ms | `scripts/ci.sh --help` when present (else unavailable rationale) | 120ms |
| `bench_help` | `am bench --help` | < 400ms | `scripts/bench_cli.sh --help` when present (else unavailable rationale) | 120ms |
| `golden_verify_help` | `am golden verify --help` | < 400ms | `scripts/bench_golden.sh --help` when present (else unavailable rationale) | 120ms |
| `flake_triage_help` | `am flake-triage --help` | < 450ms | `scripts/flake_triage.sh --help` when present (else unavailable rationale) | 140ms |
| `check_inbox_help` | `am check-inbox --help` | < 450ms | `legacy/hooks/check_inbox.sh --help` | 180ms |
| `serve_http_help` | `am serve-http --help` | < 500ms | `scripts/am --help` | 220ms |
| `e2e_run_help` | `am e2e run --help` | < 500ms | `scripts/e2e_test.sh --help` | 240ms |
| `share_wizard_help` | `am share wizard --help` | < 500ms | N/A (legacy was E2E harness, not parity wrapper) | N/A |
| `share_deploy_verify_live_help` | `am share deploy verify-live --help` | < 500ms | N/A (legacy was E2E harness, not parity wrapper) | N/A |

Per-surface overrides:
- `PERF_GUARDRAIL_NATIVE_BUDGET_P95_US_<SURFACE>=<micros>`
- `PERF_GUARDRAIL_MAX_DELTA_P95_US_<SURFACE>=<micros>`
- `PERF_GUARDRAIL_ITERATIONS=<count>`

## CLI Operational Budgets

Baseline captured via `am bench --quick` (2026-02-09). Seeded with 60 messages (50 BlueLake→RedFox + 10 RedFox→BlueLake).

| Command | Baseline (mean) | Budget | Notes |
|---------|----------------|--------|-------|
| `am --help` | 4.2ms | < 10ms | Pure startup, no DB |
| `am mail inbox` (50 msgs) | 11.5ms | < 25ms | Read path, default limit=20 |
| `am mail inbox --include-bodies` | 11.7ms | < 25ms | Bodies add negligible overhead |
| `am mail send` (single) | 27.1ms | < 50ms | Full write path (DB + archive commit) |
| `am doctor check` | 5.8ms | < 15ms | Diagnostic checks |
| `am list-projects` | 6.2ms | < 15ms | Lightweight query |
| `am lint` | 457ms | < 1000ms | Heavy static analysis |
| `am typecheck` | 399ms | < 800ms | Heavy type checking |

## Archive Write Budgets

Baseline numbers are taken from the bench harness artifacts emitted by:

```bash
rch exec -- cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write
```

Artifacts (JSON + raw samples) are written under:
- `tests/artifacts/bench/archive/<run_id>/summary.json`

Most recent cold-harness baseline run (2026-04-18):
`tests/artifacts/bench/archive/1776505951_367469/summary.json`.

Historical pre-profile reference (2026-02-08):
`tests/artifacts/bench/archive/1770542015_450923/summary.json`.

Golden baseline: `tests/artifacts/bench/baseline/golden_baseline_20260208.json`.

Budgets are set to ~2x the measured baseline p95 to absorb variance.

| Operation | Baseline p50 | Baseline p95 | Baseline p99 | Budget p95 | Budget p99 | Notes |
|-----------|--------------|--------------|--------------|------------|------------|-------|
| Single message (no attachments) | ~20.3ms | ~21.1ms | ~22.2ms | < 25ms | < 30ms | Writes canonical+outbox+1 inbox + git commit flush |
| Single message (inline attachment) | ~19.3ms | ~19.8ms | ~20.5ms | < 25ms | < 30ms | Includes WebP convert + manifest + audit + inline base64 body |
| Single message (file attachment) | ~20.6ms | ~22.0ms | ~22.4ms | < 25ms | < 30ms | Includes WebP convert + manifest + audit + file-path body |
| Batch 100 messages (no attachments) | ~1117.9ms | ~1316.6ms | ~1316.6ms | < 250ms | < 300ms | Cold fresh-repo burst per sample; use the warm-path profile update below for the decisive steady-state signal |

### Warm-Path Profile Update (br-8qdh0.3, 2026-04-18)

Historical pre-fix analysis is preserved in:
- `docs/PERF-archive-batch-profile-2026-04-18.md`

Current post-fix artifacts:
- `tests/artifacts/perf/archive_batch_100_spans.json`
- `tests/artifacts/perf/archive_batch_scaling.csv`
- `tests/artifacts/perf/archive_batch_100_flamegraph.svg`
- `tests/artifacts/perf/archive_batch_100_profile.md`
- `tests/artifacts/perf/extended_dimensions_2026-04-18.json`
- `benches/archive_perf_baseline.json` (checked-in CI gate baseline for warm batch-1/10/100)
- `.github/workflows/archive-perf-gate.yml` + `scripts/bench_archive_perf_gate.sh` (PR/scheduled regression gate)

Warm steady-state burst measurements (`MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 rch exec -- cargo bench ... archive_write_batch`):

| Scenario | p50 | p95 | p99 | p99.9 | p99.99 | Note |
|----------|-----|-----|-----|-------|--------|------|
| batch-1 | ~11.7ms | ~12.4ms | ~13.5ms | ~13.5ms | ~13.5ms | Current sample count is 40, so p99.9/p99.99 collapse to the worst observed sample |
| batch-10 | ~28.8ms | ~31.8ms | ~32.7ms | ~32.7ms | ~32.7ms | Current sample count is 25, so p99.9/p99.99 collapse to the worst observed sample |
| batch-100 | ~213.5ms | ~238.1ms | ~241.8ms | ~241.8ms | ~241.8ms | Under the `<250ms` p95 budget with ~`11.9ms` headroom; current sample count is 12, so tail sentinels equal the worst observed sample |

Scaling-law observation:
- `batch-100 p95 / batch-1 p95 = 19.27x`, so the per-message cost at batch-100 is ~`0.193x` batch-1.
- `wbq_flush` remains negligible; the decisive cost is now the batch archive write plus `flush_async_commits`.

CI perf gate policy:
- The archive regression gate compares warm batch-1/10/100 `p95` and `p99` against `benches/archive_perf_baseline.json`.
- The gate also records warm-tail sentinels (`p99.9`, `p99.99`, worst observed), direct `am serve-http` cold-start latency, and direct `am serve-http` peak RSS in a single extended-dimensions report.
- The gate allows a `10%` tolerance band over the stored `p95`/`p99` baselines to absorb runner noise, but still records hard budget breaches (`>250ms p95`, `>300ms p99`) separately.
- Tail regressions use a stricter guard: fail if `p99.9` regresses more than `50%` versus baseline or rises above `5x` the scenario `budget_p95`.
- Cold-start guard: `am serve-http --no-auth --no-tui` direct probe must stay at or below the stored `285ms` allowed band, with a hard budget of `<500ms`.
- Peak-memory guard: the same direct probe must stay at or below the stored `90MiB` allowed band, with a hard budget of `<128MiB`.
- PRs can carry the `perf-regression-acknowledged` label for an intentional, reviewed performance tradeoff; runtime/benchmark failures are never auto-acknowledged.

Span roll-up for warm batch-100:
- `archive_batch.write_message_batch` / `archive_batch.write_message_batch_bundle`: ~1.75s cumulative across 12 samples (~68% of sampled wall time)
- `archive_batch.flush_async_commits`: ~0.82s cumulative (~32%)
- `archive_batch.wbq_flush`: ~601us cumulative (noise floor)

### MCP Tool Handler Baselines (Criterion, 2026-02-08)

| Tool | Median | Throughput | Change | Notes |
|------|--------|-----------|--------|-------|
| health_check | 76.5 µs | 13.1K elem/s | stable | Read-only, cache-backed |
| ensure_project | 85.5 µs | 11.7K elem/s | stable | Idempotent upsert |
| register_agent | 492.3 µs | 2.0K elem/s | **+25% regressed** | Investigate name validation overhead |
| fetch_inbox | 143.1 µs | 7.0K elem/s | stable | Cache-backed read |
| search_messages | 158.4 µs | 6.3K elem/s | stable | FTS5 query |
| summarize_thread | 138.9 µs | 7.2K elem/s | stable | Thread summary |
| file_reservation_paths | 5.98 ms | 167 elem/s | stable | **36x slower than fetch_inbox** — overlap check hot |
| macro_start_session | 488.1 µs | 2.0K elem/s | stable | Composite: ensure+register+inbox |

## Global Search Budgets (br-3vwi.2.3)

Deterministic harness implemented in `crates/mcp-agent-mail/benches/benchmarks.rs` under the
`global_search` bench group. It seeds synthetic mailboxes of increasing size and measures the
DB-level global search pipeline p50/p95/p99 latency via
`mcp_agent_mail_db::search_service::execute_search_simple()` (planner → SQL → row mapping) for a
fixed query (`needle`, `limit=20`).

Artifacts are written under:
- `tests/artifacts/bench/search/<run_id>/summary.json`

To enforce budgets (CI/robot mode):

```bash
MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1 \
  cargo bench -p mcp-agent-mail --bench benchmarks -- global_search
```

Initial budgets (conservative; tighten after the first baseline run on CI-like hardware):

| Scenario | Messages | Budget p95 | Budget p99 |
|----------|----------|------------|------------|
| small | 1,000 | < 3ms | < 5ms |
| medium | 5,000 | < 15ms | < 25ms |
| large | 15,000 | < 50ms | < 80ms |

## Search V3 Frankensearch Lexical Budgets (br-2tnl.7.5)

Deterministic harness implemented in `crates/mcp-agent-mail-db/benches/search_v3_bench.rs`.
Seeds frankensearch indexes (Tantivy is the lexical backend inside frankensearch) with synthetic
documents and measures `TantivyBridge::search()` p50/p95/p99 for a fixed query (`needle`, `limit=20`).

Artifacts are written under:
- `tests/artifacts/bench/search_v3/<run_id>/summary.json`

Also includes:
- Index build throughput (docs/sec) at each corpus size
- Incremental add throughput (batch 1/10/100)
- Disk overhead per document (bytes)

To enforce budgets (CI/robot mode):

```bash
MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1 \
  cargo bench -p mcp-agent-mail-db --bench search_v3_bench
```

Baseline (2026-02-18):

| Scenario | Messages | Baseline p50 | Baseline p95 | Baseline p99 | Budget p95 | Budget p99 | Notes |
|----------|----------|-------------|-------------|-------------|------------|------------|-------|
| small | 1,000 | ~382µs | ~531µs | ~706µs | < 1.5ms | < 3ms | 6x under budget |
| medium | 5,000 | ~622µs | ~800µs | ~1.1ms | < 5ms | < 10ms | Frankensearch lexical sub-ms even at 5K |
| large | 15,000 | ~679µs | ~805µs | ~1.0ms | < 15ms | < 25ms | 18x under budget; lexical path barely scales with corpus |

Index build throughput (baseline): 7.5K docs/sec (1K), 36K docs/sec (5K), 90K docs/sec (15K).
Disk overhead: ~89-107 bytes/doc (amortized).

### Criterion Bench Groups

| Group | Bench IDs | Notes |
|-------|-----------|-------|
| `tantivy_lexical_search` | `small/1000`, `medium/5000`, `large/15000` | Core latency at scale |
| `tantivy_query_selectivity` | `high_selectivity`, `medium_selectivity`, `low_selectivity`, `phrase_query` | Query diversity on 5K corpus |
| `tantivy_index_build` | `docs/1000`, `docs/5000`, `docs/15000` | Construction throughput |
| `tantivy_incremental_add` | `batch/1`, `batch/10`, `batch/100` | Incremental update on 5K corpus |

### Two-Tier Semantic Budgets

Covered by `crates/mcp-agent-mail-search-core/benches/two_tier_bench.rs` (requires `semantic` feature).
Micro-benchmarks for dot product, normalization, score blending, and index-level search at 100/1K/10K.

## Share/Export Pipeline Budgets

Baseline numbers are taken from the bench harness artifacts emitted by:

```bash
cargo bench -p mcp-agent-mail --bench benchmarks -- share_export
```

Artifacts (JSON + raw samples) are written under:
- `tests/artifacts/bench/share/<run_id>/summary.json`

Most recent baseline run (2026-02-06): `tests/artifacts/bench/share/1770390636_3768966/summary.json`.

Budgets are set to ~2x the measured baseline p95/p99 to absorb variance.

### Scenario: `medium_mixed_attachments` (100 kept, 20 dropped)

| Stage | Baseline p50 | Baseline p95 | Baseline p99 | Budget p95 | Budget p99 | Notes |
|-------|--------------|--------------|--------------|------------|------------|-------|
| Total | ~1.80s | ~1.89s | ~1.92s | < 4.0s | < 4.5s | End-to-end snapshot+scope+scrub+finalize+bundle+zip |
| Snapshot | ~31ms | ~33ms | ~34ms | < 80ms | < 100ms | SQLite online backup |
| Scope | ~13ms | ~15ms | ~15ms | < 40ms | < 50ms | Project filter + deletes |
| Scrub | ~14ms | ~16ms | ~17ms | < 50ms | < 60ms | Token redaction + clears |
| Finalize | ~312ms | ~322ms | ~424ms | < 700ms | < 900ms | FTS + views + indexes + VACUUM |
| Bundle | ~1.29s | ~1.35s | ~1.37s | < 2.8s | < 3.0s | Attachments + viewer export + manifest/scaffold |
| Zip | ~134ms | ~146ms | ~152ms | < 350ms | < 400ms | Deflate (level 9) with fixed timestamps |

Output sizes (baseline):
- Output dir: ~8.0MB
- Output zip: ~0.84MB

### Scenario: `chunked_small_threshold` (forced chunking)

This scenario forces chunking by setting a small chunk threshold (128KiB) to exercise chunking overhead.

Baseline (2026-02-06): ~13 chunks; total p95 ~1.88s; zip p95 ~0.16s.

### Encryption

Age encryption (`share::encrypt_with_age`) depends on the external `age` CLI being installed.
The baseline run above did not include encryption timings (`age` not found).

## Flamegraph Profiles (2026-02-09)

Generated via `cargo flamegraph --root` with `CARGO_PROFILE_RELEASE_DEBUG=true`.

| Profile | File | Samples | Key Finding |
|---------|------|---------|-------------|
| Tool handlers | `benches/flamegraph_bench_tools.svg` | 45,056 | 65% kernel (btrfs fdatasync), syscall cancel dominates userspace |
| Archive writes | `benches/flamegraph_bench_archive.svg` | 44,948 | Same pattern — I/O bound, not CPU bound |
| Archive batch 100 (warm path, 2026-04-18) | `tests/artifacts/perf/archive_batch_100_flamegraph.svg` | 77,537 | Allocator churn plus `commit-coalesce`, `git`, `unlinkat`, and `fsync` dominate the targeted batch path; `wbq_flush` is not a hotspot |

**Interpretation**: Both profiles confirm the strace analysis below. The Rust userspace code is
highly optimized; the bottleneck is kernel-side I/O (btrfs journal sync via `fdatasync`).
Optimization effort should target reducing sync frequency (commit batching) rather than
CPU-side code changes.

**2026-04-18 targeted update**: the warm batch-100 flamegraph refines that earlier conclusion.
The path is still I/O-heavy, but the highest-signal user-space stacks are allocator churn
(`_int_malloc`, `realloc`, `free`) plus `commit-coalesce` / `git` cleanup work, with
`unlinkat`, `rename`, `readdir`, and `fsync` showing the filesystem side of the same burst.
Optimization effort should focus on reducing per-message archive/object churn and git cleanup
inside the batch burst before spending more time on `wbq_flush`.

## Syscall Profile (strace, 2026-02-08)

Collected via `strace -c -f` on `mcp_agent_mail_tools/health_check` benchmark (representative of all tool paths).

| Syscall | % Time | Seconds | Calls | Errors | Notes |
|---------|--------|---------|-------|--------|-------|
| futex | 86.30% | 461.5s | 83,318 | 28,753 | **Lock contention dominates** — mutex/condvar waits |
| sched_yield | 8.02% | 42.9s | 129,758 | — | Spinlock yielding under contention |
| fdatasync | 1.70% | 9.1s | 20,118 | — | SQLite WAL durability |
| read | 0.59% | 3.2s | 695,171 | 173 | File and DB reads |
| openat | 0.59% | 3.1s | 388,634 | 28,810 | 7.4% failure rate |
| readlink | 0.31% | 1.7s | 206,280 | 206,280 | **100% failure** — canonicalize on non-symlinks |
| access | 0.31% | 1.7s | 277,845 | 20,169 | 7.3% failure rate — existence checks |
| newfstatat | 0.27% | 1.4s | 206,847 | 90,832 | 44% failure rate |

**Key insight**: 94.3% of wall-clock time is lock contention (futex + sched_yield). The filesystem and DB I/O are relatively fast; the bottleneck is serialization between threads.

**Actionable**: Reducing `readlink` calls (canonicalize caching) would eliminate 206K syscalls per benchmark run with zero risk.

## Golden Outputs

Stable surfaces validated via `am golden verify`:

- `am --help` text
- `am <subcommand> --help` text (7 subcommands)
- Stub encoder outputs (encode, stats, help, version)
- CLI version string

Checksums stored in `benches/golden/checksums.sha256`.

## Opportunity Matrix

Score = Impact × Confidence / Effort. Only pursue Score ≥ 2.0.

Baseline date: 2026-02-08. Source: `tests/artifacts/bench/baseline/golden_baseline_20260208.json`.

Syscall profile source: strace on `mcp_agent_mail_tools/health_check` (representative of all tool paths).
Key finding: **futex (86.3%) + sched_yield (8.0%)** = 94.3% of wall time is lock contention.

| # | Hotspot | Location | Impact | Confidence | Effort | Score | Action |
|---|---------|----------|--------|------------|--------|-------|--------|
| 1 | futex contention (86% of syscall time) | DB pool acquire, global caches, WBQ mutex | 5 | 5 | 3 | 8.3 | Reduce lock hold times; use `try_lock` with fallback; shard caches per-project |
| 2 | readlink 100% failure rate (206K calls) | `canonicalize()` / `realpath()` in storage paths | 4 | 5 | 1 | 20.0 | Cache canonicalized paths; avoid repeated `canonicalize()` on hot paths |
| 3 | file_reservation_paths 36x slower (6ms vs 143µs fetch_inbox) | `crates/mcp-agent-mail-tools/src/products.rs` reservation overlap check | 5 | 4 | 2 | 10.0 | Optimize overlap algorithm; precompute glob expansions; cache active reservations |
| 4 | batch_no_attachments 4x over budget (958ms vs 250ms) | `crates/mcp-agent-mail-storage/src/lib.rs` commit batching | 4 | 5 | 3 | 6.7 | Coalesce commits more aggressively; per-repo commit queues (br-15dv.2.2) |
| 5 | register_agent +25% regression (492µs) | `crates/mcp-agent-mail-tools/src/products.rs` agent registration | 3 | 4 | 2 | 6.0 | Profile name validation; check if new HashSet validation adds overhead |
| 6 | fdatasync 1.7% (20K calls) | SQLite WAL synchronous mode | 3 | 5 | 1 | 15.0 | Already NORMAL for most paths; verify no accidental FULL mode in hot paths |
| 7 | access() 7.3% failure rate (278K calls, 20K errors) | Storage path existence checks | 3 | 4 | 2 | 6.0 | Use EAFP (try-create, handle EEXIST) instead of LBYL (check-then-create) |
| 8 | openat 7.4% failure rate (389K calls, 29K errors) | Storage file opens with O_EXCL or missing dirs | 3 | 3 | 2 | 4.5 | Batch mkdir_all once per project; cache directory existence |
| 9 | sched_yield spinlock overhead (8%) | Lock contention fallback in parking_lot or std Mutex | 4 | 3 | 3 | 4.0 | Switch to parking_lot with adaptive spinning; reduce critical section sizes |
| 10 | newfstatat 44% failure rate (207K calls, 91K errors) | Stat on non-existent files in archive | 2 | 3 | 2 | 3.0 | Reduce speculative stat calls; cache directory listings |
| 11 | toon subprocess overhead (~13.5ms per call) | `apply_toon_format` fork+exec | 3 | 5 | 3 | 5.0 | WASM or in-process encoder for hot paths; subprocess pool with warm processes |
| 12 | attachment processing p95 over budget (+1ms) | `process_markdown_images` WebP encode | 2 | 4 | 2 | 4.0 | Pre-encode in background; async WebP conversion (br-15dv.2.5) |

### Priority order (by Score)

1. **#2** readlink elimination (Score 20.0) — trivial fix, massive syscall reduction
2. **#6** fdatasync audit (Score 15.0) — verify PRAGMA synchronous in all code paths
3. **#3** file_reservation_paths optimization (Score 10.0) — worst-performing tool
4. **#1** futex contention reduction (Score 8.3) — systemic; requires architectural changes
5. **#4** batch commit coalescing (Score 6.7) — tracked in br-15dv.2.2
6. **#5** register_agent regression (Score 6.0) — investigate recent changes
7. **#7** access() pattern optimization (Score 6.0) — EAFP over LBYL
8. **#11** toon subprocess optimization (Score 5.0) — medium effort, subprocess elimination

## TUI V2 Performance Budgets (br-3vwi.9.1)

Baseline captured via `cargo test -p mcp-agent-mail-server --test tui_perf_baselines` (2026-02-10).
Headless rendering using `ftui-harness` with `Frame::new()` (no terminal I/O).

### Model Initialization

| Surface | Baseline p50 | Baseline p95 | Budget p95 | Notes |
|---------|-------------|-------------|------------|-------|
| `MailAppModel::new()` | ~135µs | ~60ms | < 100ms | One-time startup; cold-cache outliers dominate p95 |

### Per-Tick Update

| Surface | Baseline p50 | Baseline p95 | Budget p95 | Notes |
|---------|-------------|-------------|------------|-------|
| `update(Event::Tick)` | ~0µs | ~1µs | < 2ms | Ticks all 13 screens; generates toasts from events |

### Per-Screen Render (120×40)

| Screen | Baseline p50 | Baseline p95 | Budget p95 | Notes |
|--------|-------------|-------------|------------|-------|
| Dashboard | ~15µs | ~28µs | < 10ms | Overview with summary stats |
| Messages | ~5µs | ~6µs | < 10ms | Empty message browser |
| Threads | ~6µs | ~7µs | < 10ms | Empty thread list |
| Agents | ~44µs | ~61µs | < 10ms | Agent list with columns |
| Search | ~21µs | ~41µs | < 10ms | Search cockpit with facets |
| Reservations | ~45µs | ~59µs | < 10ms | File reservation table |
| ToolMetrics | ~44µs | ~49µs | < 10ms | Tool metrics dashboard |
| SystemHealth | ~10µs | ~15µs | < 10ms | System health indicators |
| Timeline | ~2µs | ~3µs | < 10ms | Lightest screen |
| Projects | ~30µs | ~42µs | < 10ms | Project list with stats |
| Contacts | ~32µs | ~35µs | < 10ms | Contact matrix |
| Explorer | ~12µs | ~12µs | < 10ms | Mail explorer tree |
| Analytics | ~9µs | ~9µs | < 10ms | Analytics overview |

### Full App Render + Tick Cycle

| Surface | Baseline p50 | Baseline p95 | Budget p95 | Notes |
|---------|-------------|-------------|------------|-------|
| Full app render (120×40) | ~30µs | ~57µs | < 15ms | Chrome + screen + status + overlays |
| Screen switch + re-render | ~29µs | ~47µs | < 2ms | Tab key + full re-render |
| Tick cycle (update + view) | ~30µs | ~40µs | < 20ms | Must stay under 100ms tick interval |
| Palette open/type/close | ~14µs | ~20µs | < 2ms | Ctrl+P → type → Esc |

### Budget Enforcement

```bash
MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1 \
  cargo test -p mcp-agent-mail-server --test tui_perf_baselines --release
```

Artifacts: `tests/artifacts/tui/perf_baselines/<timestamp>/summary.json`

## Isomorphism Invariants

Properties that must be preserved across optimizations:

1. **Ordering**: Tool list order in resources matches Python reference
2. **Tie-breaking**: Message sort by (created_ts DESC, id DESC)
3. **Float precision**: saved_percent rounded to 1 decimal
4. **Timestamp format**: ISO-8601 with timezone (microsecond precision)
5. **JSON key order**: Alphabetical within envelope.meta
