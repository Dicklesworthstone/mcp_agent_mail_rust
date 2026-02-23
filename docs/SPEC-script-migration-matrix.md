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
| `am` | `am serve-http` | **Migrated** | Native `am serve-http` is authoritative; `scripts/am` remains as optional local-dev compatibility glue. |

### CI & Quality Gates

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `ci.sh` | `am ci` | **Migrated** | Native `am ci` command with `--quick`, `--report` flags |
| `bench_cli.sh` | `am bench` | **Migrated** | Native bench command is authoritative; legacy script removed after cutover |
| `bench_golden.sh` | `am golden verify` | **Migrated** | Native `am golden verify` command; legacy script removed after cutover |

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
| `e2e_test.sh` | `am e2e run --project .` | **Retained** | Compatibility shim only; delegates to native runner by default |
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

### Legacy Compatibility Hooks

| Script | Native Equivalent | Status | Notes |
|--------|-------------------|--------|-------|
| `legacy/hooks/check_inbox.sh` | `am check-inbox` | **Migrated** | Compatibility shim only; emits deprecation warning and forwards all args to native command |

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

All previously identified gaps have been closed. The following native commands
are now available:

| Former Gap | Native Command | Status |
|------------|----------------|--------|
| CI runner | `am ci --quick --report <path>` | **Migrated** |
| Golden benchmarks | `am golden verify` | **Migrated** |

### Native Command Reference

**`am ci` command:**
- Runs all quality gates (fmt, clippy, test, E2E)
- `--quick` flag to skip long-running E2E
- `--report <path>` to emit machine-readable JSON

**`am bench` cluster:**
- `am bench` — CLI operation latency benchmarks (`--quick`, `--json`, `--baseline`, `--save-baseline`)
- `am golden verify` — Regression tests against golden outputs
- `am bench stress` — Load testing (future)

---

## Performance Guardrails Matrix (T10.9)

Canonical workload/budget matrix for migrated command surfaces. This is the
authoritative mapping consumed by `perf_guardrails.rs` and release audit checks.

| Surface | Native workload (guarded) | Legacy baseline source | Guardrail decision path | Artifact path |
|--------|----------------------------|------------------------|-------------------------|---------------|
| `ci_help` | `am ci --help` | `scripts/ci.sh --help` (usually unavailable: script removed) | Native p95 budget + optional native-vs-legacy delta when script exists | `tests/artifacts/cli/perf_guardrails/` |
| `bench_help` | `am bench --help` | `scripts/bench_cli.sh --help` (usually unavailable: script removed) | Native p95 budget + optional native-vs-legacy delta when script exists | `tests/artifacts/cli/perf_guardrails/` |
| `golden_verify_help` | `am golden verify --help` | `scripts/bench_golden.sh --help` (usually unavailable: script removed) | Native p95 budget + optional native-vs-legacy delta when script exists | `tests/artifacts/cli/perf_guardrails/` |
| `flake_triage_help` | `am flake-triage --help` | `scripts/flake_triage.sh --help` (usually unavailable: script removed) | Native p95 budget + optional native-vs-legacy delta when script exists | `tests/artifacts/cli/perf_guardrails/` |
| `check_inbox_help` | `am check-inbox --help` | `legacy/hooks/check_inbox.sh --help` | Native p95 budget + native-vs-legacy delta budget | `tests/artifacts/cli/perf_guardrails/` |
| `serve_http_help` | `am serve-http --help` | `scripts/am --help` | Native p95 budget + native-vs-legacy delta budget | `tests/artifacts/cli/perf_guardrails/` |
| `e2e_run_help` | `am e2e run --help` | `scripts/e2e_test.sh --help` | Native p95 budget + native-vs-legacy delta budget | `tests/artifacts/cli/perf_guardrails/` |
| `share_wizard_help` | `am share wizard --help` | N/A (legacy E2E harness, no direct CLI wrapper) | Native p95 budget only + explicit unavailable rationale | `tests/artifacts/cli/perf_guardrails/` |
| `share_deploy_verify_live_help` | `am share deploy verify-live --help` | N/A (legacy E2E harness, no direct CLI wrapper) | Native p95 budget only + explicit unavailable rationale | `tests/artifacts/cli/perf_guardrails/` |

Notes:
- Per-surface overrides are supported via `PERF_GUARDRAIL_NATIVE_BUDGET_P95_US_<SURFACE>`
  and `PERF_GUARDRAIL_MAX_DELTA_P95_US_<SURFACE>`.
- When a legacy script is no longer present, the suite records deterministic
  `legacy unavailable` rationale in the artifact instead of failing for missing files.

---

## Cross-Platform Native Command Matrix (T10.8)

Canonical portability gate for migrated native command surfaces runs in
`.github/workflows/ci.yml` job `native-command-matrix` across:

- `ubuntu-latest`
- `macos-latest`
- `windows-latest`

### Matrix Coverage

| Case ID | Command | Sensitivity exercised |
|---------|---------|-----------------------|
| `ci_help` | `am ci --help` | Core command wiring + clap help rendering |
| `flake_triage_help` | `am flake-triage --help` | Subcommand registration + output stability |
| `bench_help` | `am bench --help` | Nested command parsing |
| `check_inbox_help` | `am check-inbox --help` | Legacy-hook replacement command surface |
| `golden_verify_help` | `am golden verify --help` | Nested migration command path |
| `serve_http_help` | `am serve-http --help` | Serve options + transport flags |
| `e2e_run_help` | `am e2e run --help` | Native E2E entrypoint contract |
| `share_wizard_help` | `am share wizard --help` | Share wizard native flow |
| `share_verify_live_help` | `am share deploy verify-live --help` | Deploy validation command surface |
| `doctor_check_help` | `am doctor check --help` | Diagnostics command path |
| `check_inbox_direct_json` | `am check-inbox --direct --json --project <cwd> --agent MatrixProbe --rate-limit 0` | Project path normalization + JSON/newline output stability |
| `config_set_port_envfile` | `am config set-port 8765 --env-file "<artifact_root>/tmp env file.env"` | Filesystem path quoting + env-file handling |
| `config_show_port` | `am config show-port` | Config read path portability after env-file mutation |

This matrix intentionally mixes help-surface checks with functional probes so
platform-sensitive behavior is exercised across path normalization, quoting,
encoding/newline-safe JSON output, and env-file read/write flows.

### Artifact Contract

Per-OS artifacts are required under:
`tests/artifacts/cli/native_command_matrix/<os>/`

Required files:
- `environment.json` (runner fingerprint + timestamp)
- `summary.json` (total/pass/fail + per-case timing)
- `commands/<case_id>/command.txt`
- `commands/<case_id>/stdout.txt`
- `commands/<case_id>/stderr.txt`
- `commands/<case_id>/exit_code.txt`
- `commands/<case_id>/timing_ms.txt`

Failure policy:
- Any non-zero `exit_code` in any case fails the matrix job.
- `skip-guard` treats `native-command-matrix` as required for CI success.

---

## UX Consistency Smoke Suite (T10.7)

To prevent migration regressions in operator-facing UX, run:

```bash
am e2e run --project . migration_ux_smoke
# compatibility fallback:
# AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh migration_ux_smoke
```

Suite file:
- `tests/e2e/test_migration_ux_smoke.sh`

Coverage focus:
- Cross-command consistency for migrated native commands (`--help` content, invalid-flag exit semantics, remediation hint consistency).
- Onboarding smoke paths (first-run success path + common failure paths with machine-readable diagnostics).

Artifact contract (under `tests/artifacts/migration_ux_smoke/<timestamp>/`):
- Per-case: `command.txt`, `stdout.txt`, `stderr.txt`, `exit_code.txt`, `timing_ms.txt`, `environment.json`, `result.json`
- Suite-level: `ux_consistency_report.json` and optional `ux_findings.jsonl`

Governance use:
- T10.6 closure readiness must reference the latest `ux_consistency_report.json`.
- Any warning/error in `ux_findings.jsonl` must be resolved via docs/runbook updates or follow-up beads before closure.

---

## Deprecation and Rollback Policy (T10.5)

This policy governs every legacy script shim that remains after native command migration.

### Authoritative Path

- Native `am` commands are authoritative.
- Legacy script shims are compatibility-only and must emit explicit deprecation guidance.
- Any discrepancy between shim behavior and native output is treated as a migration bug.

### Deprecation Stages

| Stage | Operator expectation | Minimum window |
|-------|----------------------|----------------|
| **Stage A: Announce** | Before/after command mapping published, shim warns at runtime | 1 release cycle |
| **Stage B: Default Shift** | CI/runbook examples use native path only; shim remains as fallback | 30 days |
| **Stage C: Removal Eligible** | Shim callers audited/migrated; rollback plan validated | 1 release cycle after Stage B |

### Fallback and Rollback Conditions

Rollback to shim-first guidance is allowed only when a native-path regression is confirmed:

- P0/P1 operational breakage in native command path.
- Deterministic correctness mismatch versus prior shim contract.
- Security/privacy regression requiring immediate mitigation.

If rollback is triggered:

1. Open incident bead linked to failing command/thread.
2. Re-enable compatibility guidance in runbook + release checklist.
3. Capture reproduction artifacts (command/stdout/stderr/exit/timing) in `tests/artifacts/`.
4. Publish recovery ETA and owner; re-run migration gates before returning to native-first.

### Operator Troubleshooting Baseline

- Prefer native reproduction first:
  - `am <command> --help`
  - `RUST_LOG=debug am <command> ...`
- For shim behavior:
  - run shim once with current args
  - compare output/exit code against native invocation
- Debug artifact locations:
  - `tests/artifacts/` (suite-level machine-readable traces)
  - command-specific E2E suites under `tests/e2e/`

### Verification Checklist (Re-audit Ready)

- [ ] Every retained shim prints deprecation + native replacement mapping.
- [ ] Migration guide examples are native-first and do not require script-first paths.
- [ ] Release checklist references rollback trigger/steps and artifact evidence requirements.
- [ ] CI/gate commands remain native-first with no contradictory script-only workflow.
- [ ] At least one deterministic reproduction path exists per shim.

---

## CI Workflow Cutover Status (br-2rfq)

**Date:** 2026-02-23

GitHub Actions workflow jobs have been cut over to native `am e2e run` invocations
where safe and where the native runner supports the required artifact semantics.

### `.github/workflows/ci.yml`

| Job | Step | Before | After | Status |
|-----|------|--------|-------|--------|
| `e2e-dual-mode` | E2E dual-mode suite | `bash scripts/e2e_dual_mode.sh` | `am e2e run --project . dual_mode` | **Migrated** |
| `e2e-dual-mode` | E2E mode matrix suite | `bash scripts/e2e_mode_matrix.sh` | `am e2e run --project . mode_matrix` | **Migrated** |
| `e2e-dual-mode` | E2E web parity suite | `bash tests/e2e/test_http.sh` | `am e2e run --project . http` | **Migrated** |
| `e2e-dual-mode` | E2E static export | `bash tests/e2e/test_share.sh` | `am e2e run --project . share` | **Migrated** |
| `perf-security` | Soak harness (quick) | `bash tests/e2e/test_soak_harness.sh --quick` | `am e2e run --project . soak_harness` (env vars) | **Migrated** |
| `search-v3-e2e` | Search V3 suites | 5× `bash tests/e2e/test_search_v3_*.sh` | `am e2e run --project . search_v3_*` (single invocation) | **Migrated** |

### `.github/workflows/search-v3-weekly.yml`

| Step | Before | After | Status |
|------|--------|-------|--------|
| Weekly bundle | `scripts/e2e_search_v3_weekly_bundle.sh --strict-freshness` | (unchanged) | **Retained** |

**Rationale for retaining weekly bundle:** The orchestration script provides
`--strict-freshness` validation and weekly-specific rollup reports that the native
runner does not replicate. Individual suites within it are exercised by the native
runner in the main CI workflow.

### Artifact Validation Steps

`source scripts/e2e_lib.sh; e2e_validate_bundle_tree tests/artifacts` calls remain
unchanged across all workflow jobs. No native `am e2e validate-bundle` command
exists; the bash utility function is the authoritative artifact validator.

### Rollback Path

If any native runner invocation regresses versus the prior script-based execution:
1. Replace `am e2e run --project . <suite>` with `bash tests/e2e/test_<suite>.sh`
   (or `bash scripts/e2e_<suite>.sh` for scripts/ directory suites).
2. Open incident bead documenting the regression.
3. Revalidate artifact contract before restoring native path.

---

## Summary

| Category | Migrated | Partial | Gap | Retained |
|----------|----------|---------|-----|----------|
| Server/Wrappers | 1 | 0 | 0 | 0 |
| CI/Quality | 3 | 0 | 0 | 0 |
| Flake Triage | 1 | 0 | 0 | 0 |
| Legacy Hooks | 1 | 0 | 0 | 0 |
| E2E Harnesses | 0 | 0 | 0 | 15 |
| TUI Tests | 0 | 0 | 0 | 5 |
| Utility | 0 | 0 | 0 | 2 |
| tests/e2e/ | 0 | 0 | 0 | 30 |
| **Total** | **6** | **0** | **0** | **52** |

### Key Insights

1. **Most scripts are intentionally retained** as test harnesses — they exercise the
   native commands and should remain as bash scripts.

2. **The native `am serve-http` command is authoritative.** The `scripts/am`
   wrapper remains optional compatibility glue for local development.

3. **All gaps are now closed:**
   - `ci.sh` is migrated to `am ci`
   - `bench_golden.sh` is migrated to `am golden verify`

4. **`flake_triage.sh` is fully migrated** to `am flake-triage` with subcommands
   `scan`, `reproduce`, and `detect`.

5. **Legacy `check_inbox` hook path is compatibility-only** and now delegates to
   native `am check-inbox` with explicit deprecation messaging.

---

## References

- [ADR-001: Dual-Mode Invariants](ADR-001-dual-mode-invariants.md)
- [ADR-002: Single Binary CLI Opt-In](ADR-002-single-binary-cli-opt-in.md)
- [SPEC: MCP-to-CLI Parity Matrix](SPEC-parity-matrix.md)
- [Migration Guide](MIGRATION_GUIDE.md)
