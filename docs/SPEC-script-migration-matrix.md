# Spec: Script-to-Native Command Migration Matrix

**Bead:** br-3rls
**Depends on:** ADR-001, ADR-002, SPEC-parity-matrix
**Date:** 2026-02-12

## Scope

This document maps every operational shell script to its native `am` CLI equivalent
(or gap), establishing the canonical migration status for moving from shell-based
workflows to native Rust commands.

## Classification Labels

| Label | Meaning |
|-------|---------|
| **Migrated** | Full native equivalent exists; script can be deprecated |
| **Partial** | Native equivalent covers main use cases; edge cases remain in script |
| **In Progress** | Native implementation underway (tracked by bead) |
| **Retained** | Intentionally kept as script (test harness, convenience wrapper, etc.) |
| **Gap** | No native equivalent; should be implemented |

---

## Scripts Directory (`scripts/`)

### Server & Convenience Wrappers

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `am` | `mcp-agent-mail serve` | **Retained** | Convenience wrapper with defaults (auth discovery, port reuse). Keep for operator UX. |

### CI & Quality Gates

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `ci.sh` | -- | **Gap** | Add `am ci` command with `--quick`, `--report` flags |
| `bench_cli.sh` | -- | **Gap** | Add `am bench cli` subcommand |
| `bench_golden.sh` | -- | **Gap** | Add `am bench golden` subcommand |

### Flake Triage

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `flake_triage.sh` | `am flake-triage scan/reproduce/detect` | **Migrated** | Full parity achieved |

### E2E Test Harnesses

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `e2e_lib.sh` | -- | **Retained** | Shared bash utilities for E2E tests; no native equivalent needed |
| `e2e_cli.sh` | -- | **Retained** | CLI integration tests; run as bash test suite |
| `e2e_http.sh` | -- | **Retained** | HTTP transport tests; run as bash test suite |
| `e2e_archive.sh` | -- | **Retained** | Archive tests; run as bash test suite |
| `e2e_console.sh` | -- | **Retained** | Console tests; run as bash test suite |
| `e2e_dual_mode.sh` | -- | **Retained** | Dual-mode verification tests |
| `e2e_mode_matrix.sh` | -- | **Retained** | Mode matrix verification tests |
| `e2e_mcp_api_parity.sh` | -- | **Retained** | MCP/API parity tests |
| `e2e_share.sh` | -- | **Retained** | Share/export tests |
| `e2e_test.sh` | -- | **Retained** | Generic E2E entry point |
| `e2e_depth_counter.sh` | -- | **Retained** | Test infrastructure |
| `e2e_histogram_snapshot.sh` | -- | **Retained** | Test infrastructure |
| `e2e_spill_determinism.sh` | -- | **Retained** | Test infrastructure |

### TUI Test Harnesses

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `e2e_tui_startup.sh` | -- | **Retained** | TUI startup verification |
| `e2e_tui_interaction.sh` | -- | **Retained** | TUI interaction tests |
| `e2e_tui_interactions.sh` | -- | **Retained** | Extended TUI interaction tests |
| `e2e_tui_compat_matrix.sh` | -- | **Retained** | Terminal compatibility matrix |
| `e2e_tui_a11y.sh` | -- | **Retained** | Accessibility tests |

### Utility Scripts

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `toon_stub_encoder.sh` | -- | **Retained** | Test stub; not operational |
| `toon_stub_encoder_fail.sh` | -- | **Retained** | Test stub; not operational |

---

## Tests Directory (`tests/e2e/`)

### Core E2E Test Scripts

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `test_archive.sh` | -- | **Retained** | Archive feature tests |
| `test_artifacts_schema.sh` | -- | **Retained** | Artifact schema validation |
| `test_bearer_auth.sh` | -- | **Retained** | Bearer token auth tests |
| `test_bench_smoke.sh` | -- | **Retained** | Benchmark smoke tests |
| `test_cli.sh` | -- | **Retained** | CLI integration tests |
| `test_concurrent_agents.sh` | -- | **Retained** | Multi-agent concurrency tests |
| `test_concurrent_conflicts_e2e.sh` | -- | **Retained** | Conflict resolution tests |
| `test_console.sh` | -- | **Retained** | Console tests |
| `test_contact_policy.sh` | -- | **Retained** | Contact policy tests |
| `test_crash_restart.sh` | -- | **Retained** | Crash recovery tests |
| `test_cross_project.sh` | -- | **Retained** | Cross-project tests |
| `test_db_corruption.sh` | -- | **Retained** | DB corruption resilience tests |
| `test_db_migration_e2e.sh` | -- | **Retained** | Schema migration tests |
| `test_dual_mode.sh` | -- | **Retained** | Dual-mode tests |
| `test_fault_injection.sh` | -- | **Retained** | Fault injection tests |
| `test_guard.sh` | -- | **Retained** | Pre-commit guard tests |
| `test_http.sh` | -- | **Retained** | HTTP transport tests |
| `test_http_streamable.sh` | -- | **Retained** | HTTP streaming tests |
| `test_jwt.sh` | -- | **Retained** | JWT authentication tests |
| `test_large_inputs.sh` | -- | **Retained** | Large input handling tests |
| `test_llm.sh` | -- | **Retained** | LLM integration tests |
| `test_macros.sh` | -- | **Retained** | Macro workflow tests |
| `test_mail_ui.sh` | -- | **Retained** | Mail UI tests |
| `test_malformed_rpc.sh` | -- | **Retained** | Malformed RPC handling tests |
| `test_mcp_api_parity.sh` | -- | **Retained** | MCP/API parity tests |
| `test_mode_matrix.sh` | -- | **Retained** | Mode matrix tests |
| `test_notifications.sh` | -- | **Retained** | Notification tests |
| `test_null_fields.sh` | -- | **Retained** | Null field handling tests |

---

## Native CLI Command Inventory (`am`)

### Top-Level Commands (Already Native)

| Command | Description | Status |
|---------|-------------|--------|
| `am serve-http` | Start HTTP server | Native |
| `am serve-stdio` | Start stdio MCP server | Native |
| `am share *` | Export/snapshot/bundle | Native |
| `am archive *` | Archive operations | Native |
| `am guard *` | Pre-commit guard | Native |
| `am file_reservations *` | File reservation queries | Native |
| `am acks *` | Acknowledgment queries | Native |
| `am config *` | Configuration management | Native |
| `am amctl *` | Server control | Native |
| `am projects *` | Project management | Native |
| `am mail *` | Messaging operations | Native |
| `am products *` | Product bus operations | Native |
| `am docs *` | Documentation generation | Native |
| `am doctor *` | Diagnostics | Native |
| `am agents *` | Agent management | Native |
| `am tooling *` | Dev tooling | Native |
| `am macros *` | Workflow macros | Native |
| `am contacts *` | Contact management | Native |
| `am beads *` | Issue tracker integration | Native |
| `am setup *` | Agent detection & config | Native |
| `am flake-triage *` | Flake analysis | Native |
| `am robot *` | Agent-optimized output | Native |
| `am migrate` | Schema migration | Native |
| `am lint` | Lint checks | Native |
| `am typecheck` | Type checking | Native |

### Robot Subcommands (Agent-First)

| Command | Description |
|---------|-------------|
| `am robot status` | Dashboard synthesis |
| `am robot inbox` | Actionable inbox |
| `am robot timeline` | Events since last check |
| `am robot overview` | Cross-project summary |
| `am robot thread` | Thread rendering |
| `am robot search` | Full-text search |
| `am robot message` | Single message view |
| `am robot navigate` | Resource URI resolution |
| `am robot reservations` | Reservation status |
| `am robot metrics` | Tool performance |
| `am robot health` | System diagnostics |
| `am robot analytics` | Anomaly insights |
| `am robot agents` | Agent roster |
| `am robot contacts` | Contact graph |
| `am robot projects` | Project summary |
| `am robot attachments` | Attachment inventory |

---

## Gap Analysis & Implementation Priority

### High Priority Gaps

| Gap | Proposed Command | Rationale | Owner | Risk |
|-----|------------------|-----------|-------|------|
| CI runner | `am ci --quick --report <path>` | Frequently used in dev workflow | TBD | Low |
| CLI benchmarks | `am bench cli` | Performance regression tracking | TBD | Low |
| Golden benchmarks | `am bench golden` | Performance regression tracking | TBD | Low |

### Design Considerations

**`am ci` command:**
- Run all quality gates (fmt, clippy, test, E2E)
- `--quick` flag to skip long-running E2E
- `--report <path>` to emit machine-readable JSON
- Should invoke existing Rust tooling, not shell out to cargo

**`am bench` cluster:**
- `am bench cli` — CLI operation latency benchmarks
- `am bench golden` — Regression tests against golden outputs
- `am bench stress` — Load testing (future)

---

## Summary

| Category | Migrated | Partial | Gap | Retained |
|----------|----------|---------|-----|----------|
| Server/Wrappers | 0 | 0 | 0 | 1 |
| CI/Quality | 0 | 0 | 3 | 0 |
| Flake Triage | 1 | 0 | 0 | 0 |
| E2E Harnesses | 0 | 0 | 0 | 15 |
| TUI Tests | 0 | 0 | 0 | 5 |
| Utility | 0 | 0 | 0 | 2 |
| tests/e2e/ | 0 | 0 | 0 | 30 |
| **Total** | **1** | **0** | **3** | **53** |

### Key Insights

1. **Most scripts are intentionally retained** as test harnesses — they exercise the
   native commands and should remain as bash scripts.

2. **The `scripts/am` wrapper is retained** for operator convenience — it handles
   auth token discovery and port reuse logic that would be awkward in the binary.

3. **Only 3 gaps identified:**
   - `ci.sh` → `am ci`
   - `bench_cli.sh` → `am bench cli`
   - `bench_golden.sh` → `am bench golden`

4. **`flake_triage.sh` is fully migrated** to `am flake-triage` with subcommands
   `scan`, `reproduce`, and `detect`.

---

## References

- [ADR-001: Dual-Mode Invariants](ADR-001-dual-mode-invariants.md)
- [ADR-002: Single Binary CLI Opt-In](ADR-002-single-binary-cli-opt-in.md)
- [SPEC: MCP-to-CLI Parity Matrix](SPEC-parity-matrix.md)
- [Migration Guide](MIGRATION_GUIDE.md)
