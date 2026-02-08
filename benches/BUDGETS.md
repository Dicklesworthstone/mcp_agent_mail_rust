# Performance Budgets

Baseline performance targets for mcp-agent-mail Rust port.
Updated via `scripts/bench_cli.sh` and `cargo bench`.

## Optimization Workflow

1. **Profile** — measure before changing anything
2. **Change** — apply one optimization
3. **Prove** — verify behavior unchanged (golden outputs) AND performance improved

## Hardware Notes

- Platform: Linux x86_64 (Ubuntu)
- Kernel: 6.17.0
- Target dir: `/data/tmp/cargo-target`
- Build profile: `release` for CLI benchmarks, `bench` for Criterion

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

## Archive Write Budgets

Baseline numbers are taken from the bench harness artifacts emitted by:

```bash
cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write
```

Artifacts (JSON + raw samples) are written under:
- `tests/artifacts/bench/archive/<run_id>/summary.json`

Most recent baseline run (2026-02-08): `tests/artifacts/bench/archive/1770542015_450923/summary.json`.

Golden baseline: `tests/artifacts/bench/baseline/golden_baseline_20260208.json`.

Budgets are set to ~2x the measured baseline p95 to absorb variance.

| Operation | Baseline p50 | Baseline p95 | Baseline p99 | Budget p95 | Budget p99 | Notes |
|-----------|--------------|--------------|--------------|------------|------------|-------|
| Single message (no attachments) | ~17.2ms | ~21.3ms | ~22.1ms | < 25ms | < 30ms | Writes canonical+outbox+1 inbox + git commit flush |
| Single message (inline attachment) | ~22.0ms | ~26.0ms | ~26.7ms | < 25ms | < 30ms | Includes WebP convert + manifest + audit + inline base64 body. **p95 over budget** |
| Single message (file attachment) | ~20.4ms | ~25.2ms | ~29.1ms | < 25ms | < 30ms | Includes WebP convert + manifest + audit + file-path body. **p95 marginal** |
| Batch 100 messages (no attachments) | ~930ms | ~1076ms | ~1076ms | < 250ms | < 300ms | **4x over budget** — commit batching not yet coalescing effectively |

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

Stable surfaces validated via `scripts/bench_golden.sh validate`:

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

## Isomorphism Invariants

Properties that must be preserved across optimizations:

1. **Ordering**: Tool list order in resources matches Python reference
2. **Tie-breaking**: Message sort by (created_ts DESC, id DESC)
3. **Float precision**: saved_percent rounded to 1 decimal
4. **Timestamp format**: ISO-8601 with timezone (microsecond precision)
5. **JSON key order**: Alphabetical within envelope.meta
