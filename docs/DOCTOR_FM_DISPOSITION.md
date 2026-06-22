# Doctor Failure-Mode (FM) Disposition Table

> **Bead:** `br-bvq1x.2.6` (B6 — Doctor threat-model audit: live registry vs
> historical FM inventories). **Audited:** 2026-06-21.

## Purpose

Reconcile the **live** `am doctor` fixer registry against the **historical FM
inventory** mined from session history, and give every high-risk failure mode
exactly one disposition — so no preventable root cause is silently dropped.

The live registry is the source of truth and it drifts fast. This document is a
point-in-time audit, **not** a second source of truth. For the exact current
registry run:

```bash
am doctor fixers --format json | jq '.fixers[] | {id, severity, op_pattern, auto_fixable}'
```

The registry is wired in `crates/mcp-agent-mail-cli/src/doctor/fixers/mod.rs`
(`registry()`); each FM lives in its own file at
`crates/mcp-agent-mail-cli/src/doctor/fixers/<slug>.rs`. At audit time it held
**~60 FMs across 11 subsystems (23 auto-fixable, 37 detect-only)** spread over
62 fixer modules (incl. `coresident_db_writer` from `br-j3e9m` and
`corrupt_search_index` from `br-2vdg9`).

## Disposition legend

| Disposition | Meaning |
|-------------|---------|
| **AUTO-FIX** | A live FM detects it and repairs it through the `mutate()` chokepoint (reversible via `am doctor undo`). |
| **DETECT-ONLY** | A live FM detects and explains it, but the safe fix needs operator-supplied truth, owner coordination, or an authoritative-side decision. Repair is intentionally not automated. |
| **PREVENT** | The root cause is eliminated at the source (canonical init SQL / pool config / build) and guarded by a regression test, so a runtime doctor FM is unnecessary or architecturally unsound. |
| **COVERED-ELSEWHERE** | Not a doctor mutate-FM concern; surfaced by the runtime liveness / health / metrics surface (Tracks I/K). |
| **OUT-OF-SCOPE** | Out of doctor's remit by design. |
| **GAP → bead** | Genuinely uncovered; routed to a concrete bead. |

## Summary

- 18 of 30 historical FMs map directly to a live FM (plus several name-mismatch
  / partial-coverage cases noted inline).
- **6 P0 gaps from the original inventory:** 4 are now covered (detect-only by
  design), 1 is PREVENTED at source, and the last (concurrent Python-server
  co-resident writes) is now **RESOLVED** by a dedicated detect-only FM
  (`fm-runtime-processes-coresident-db-writer`, P0; `br-j3e9m`).
- **Both gaps originally routed to beads are now RESOLVED** (no open doctor-FM
  gaps remain from this audit):
  - `python-server-coresident-write` → **RESOLVED** (`br-j3e9m`):
    `fm-runtime-processes-coresident-db-writer` flags a live Python
    `mcp_agent_mail` server holding `storage.sqlite3` open (or its advisory
    lock) — the root cause, caught before it corrupts.
  - `search-v3-index-corrupt` → **RESOLVED** (`br-2vdg9`):
    `fm-search-index-state-corrupt-index` (P2, detect-only) probes the
    on-disk frankensearch/Tantivy index for a dangling active link, a
    missing Tantivy `meta.json`, or a failed/incomplete/unparseable
    `checkpoint.json`, and points at the restart-driven rebuild.
  - `busy-timeout-missing` → **PREVENT** (no bead — set in `schema.rs` init SQL,
    test-guarded).
  - `tui-foreground-cpu-spin`, `commit-coalescer-thread-died`,
    `WBQ-writes-uncommitted-after-fork-failure` → **COVERED-ELSEWHERE** (Track
    I/K health & WBQ circuit-breaker surfaces; no doctor mutate-FM needed).

## Reconciliation: historical inventory → live registry

### DB-state FMs

| Historical FM | Disposition | Live FM id / evidence |
|---------------|-------------|------------------------|
| wal-shm-sidecar-drift | DETECT-ONLY (auto-fix deferred) | `fm-db-state-files-wal-shm-sidecar-drift`. Safe-checkpoint needs live-owner coordination; G4 `wal_classify.rs` primitives ready for non-owner adoption. |
| integrity-page-malformed | DETECT-ONLY (fix = gated reconstruct) | `fm-db-state-files-integrity-page-malformed`; reconstruct is the operator-gated fix, hardened by `br-bvq1x.1.6` (canonical integrity_check confirmation before reconstruct). |
| schema-version-mismatch | DETECT-ONLY (operator action) | `fm-db-state-files-schema-version-mismatch`; distinguishes ForwardMigrate (P0, `am serve` restart) vs Newer (P1, binary upgrade). |
| text-timestamp-contamination | DETECT-ONLY | `fm-db-state-files-text-timestamp-contamination` (migration handles conversion). |
| orphan-foreign-key-rows | AUTO-FIX | `fm-db-state-files-orphan-foreign-key-rows` (Op::DbExec). |
| wal-mode-disabled | AUTO-FIX | `fm-db-state-files-wal-mode-disabled` (Op::DbExec). |
| empty-or-truncated-db | DETECT-ONLY (fix = reconstruct) | `fm-db-state-files-empty-or-truncated-db`. |
| legacy-fts-residue | AUTO-FIX | `fm-db-state-files-legacy-fts-residue` (Op::DbExec). |
| retained-autocommit-leak | DETECT-ONLY (source policy) | `fm-db-state-files-retained-autocommit-leak`; verifies pool-init SQL constants (cannot mutate compile-time constants from doctor). |
| inbox-stats-divergence | AUTO-FIX | `fm-db-state-files-inbox-stats-divergence` (Op::DbExec). |
| sqlite-sidecar-symlink | DETECT-ONLY | `fm-db-state-files-sqlite-sidecar-symlink`. |
| **busy-timeout-missing** | **PREVENT** | Set in canonical init SQL `crates/mcp-agent-mail-db/src/schema.rs:224` (`PRAGMA busy_timeout = 60000;`) and guarded by `pragma_busy_timeout_matches_legacy`. The pass-35V doctor detector was correctly reverted: `busy_timeout` is connection-local, so a separate doctor-process connection always reads SQLite defaults — detection from doctor is unsound. |
| **search-v3-index-corrupt** | **DETECT-ONLY (`br-2vdg9` resolved)** | `fm-search-index-state-corrupt-index` (P2, search_index_state). Pure over `$SEARCH_V3_INDEX_DIR`: flags a dangling `active-{engine}` link, a missing Tantivy `meta.json` (lexical), or a failed/incomplete/unparseable `checkpoint.json`. Low-risk (the index is a derived artifact — SQLite is the source of truth; rebuilt from it on restart). `legacy-fts-residue` only cleans old SQLite FTS5 artifacts; this is the live frankensearch-index surface. |
| **python-server-coresident-write** | **DETECT-ONLY (`br-j3e9m` resolved)** | `fm-runtime-processes-coresident-db-writer` (P0, runtime_processes). Pure over the I4 `ProcessOwnerModel`: flags a live Python `mcp_agent_mail` holder of `storage.sqlite3` (open fd → confidence 1.0; advisory lock → 0.9) before it corrupts. Detect-only by design — doctor never kills a foreign process. Sibling FMs cover the *symptom* (`fm-db-state-files-integrity-page-malformed`) and the PID-hint *stale* case (`fm-runtime-processes-stale-python-server-shadow`). Known scope limit (routed to follow-up): a truly foreign writer that is neither the Rust binary nor a recognizable Python shadow is filtered out by `inspect_mailbox_ownership` upstream and is invisible to this pure detector. |

### Runtime-process FMs

| Historical FM | Disposition | Live FM id / evidence |
|---------------|-------------|------------------------|
| stale-listener-pid-hint | AUTO-FIX | `fm-runtime-processes-stale-listener-pid-hint` (Op::Rename). |
| port-bound-by-foreign-process | DETECT-ONLY | `fm-runtime-processes-port-bound-by-foreign-process` (operator must decide). |
| supervisor-respawn-loop | DETECT-ONLY | `fm-runtime-processes-supervisor-respawn-loop`. |
| stale-python-server-shadow | DETECT-ONLY | `fm-runtime-processes-stale-python-server-shadow`. |
| service-manager-divergence | DETECT-ONLY | `fm-runtime-processes-service-manager-divergence`. |
| pid-hint-symlink-toctou | DETECT-ONLY | `fm-runtime-processes-pid-hint-symlink-toctou`. |
| **tui-foreground-cpu-spin** | **COVERED-ELSEWHERE** | TUI loop liveness surfaced via `br-bvq1x.9.2` (robot/doctor health), not a doctor mutate-FM. |
| **commit-coalescer-thread-died** | **COVERED-ELSEWHERE** | Surfaced via health/support-bundle (`br-bvq1x.9.5`) + WBQ circuit-breaker (`br-bvq1x.9.8`). |

### Archive-state FMs

| Historical FM | Disposition | Live FM id / evidence |
|---------------|-------------|------------------------|
| stale .archive.lock | AUTO-FIX | `fm-archive-state-files-stale-archive-lock-from-dead-pid` (Op::Rename). |
| stale .git/index.lock | AUTO-FIX (HEAD/ref locks) | `fm-archive-state-files-stale-head-or-ref-update-lock` (Op::Rename). Covers HEAD/ref locks; index.lock specifically is the same stale-lock class. |
| missing ODB objects / broken git shape | DETECT-ONLY (fix = reconstruct) | `fm-archive-state-files-missing-head-or-broken-git-shape` (P0). |
| duplicate canonical message IDs | DETECT-ONLY | `fm-archive-state-files-duplicate-canonical-message-ids` (P0; quarantine via archive-normalize). |
| malformed project.json | AUTO-FIX (partial) | `fm-archive-state-files-missing-or-malformed-project-json` (Op::WriteFile; rewrites only when canonical human_key is known). |
| suspicious ephemeral roots | DETECT-ONLY | `fm-archive-state-files-suspicious-ephemeral-archive-root`. |
| loose-object bloat | DETECT-ONLY | `fm-archive-state-files-loose-object-bloat-no-pack`. |
| unexpected archive symlinks | AUTO-FIX | `fm-archive-state-files-unexpected-symlink-in-archive` (Op::Rename quarantine). |
| **WBQ writes uncommitted after fork failure** | **COVERED-ELSEWHERE** | Surfaced via WBQ persistent-failure circuit breaker / metrics (`br-bvq1x.9.8`); not a doctor mutate-FM. |

## P0 gaps from the original inventory — final status

| P0 gap | Status | Disposition |
|--------|--------|-------------|
| wal-shm-sidecar-drift | Covered | DETECT-ONLY; auto-checkpoint deferred (needs owner coordination). |
| integrity-page-malformed | Covered | DETECT-ONLY; fix = gated reconstruct (hardened by `br-bvq1x.1.6`). |
| schema-version-mismatch | Covered | DETECT-ONLY; forward = restart/migrate, newer = binary upgrade. |
| retained-autocommit-leak | Covered | DETECT-ONLY; source-policy (pool init SQL + test). |
| busy-timeout-missing | Resolved | PREVENT (schema init SQL + `pragma_busy_timeout_matches_legacy`). |
| python-server-coresident-write | **Closed** | DETECT-ONLY: `fm-runtime-processes-coresident-db-writer` (P0) detects the live Python concurrent writer before it corrupts (`br-j3e9m`). |

## Routing

| Gap | Action |
|-----|--------|
| python-server-coresident-write | **Done** — `br-j3e9m`: `fm-runtime-processes-coresident-db-writer` (P0, detect-only) detects an active Python co-resident DB writer. |
| search-v3-index-corrupt | **Done** — `br-2vdg9`: `fm-search-index-state-corrupt-index` (P2, detect-only) probes the on-disk index and points at the restart-driven rebuild. |

All other historical FMs are AUTO-FIX, DETECT-ONLY, PREVENT, or
COVERED-ELSEWHERE as tabulated above — none are silently dropped.
