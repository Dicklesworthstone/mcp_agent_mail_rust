# ATC Perf Budgets

This gate exists to keep ATC instrumentation from quietly inflating the
`send_message` hot path.

## Benchmarks

The checked-in `am bench` catalog now includes three ATC-mode variants:

- `mail_send_no_atc` with `ATC_LEARNING_DISABLED=1`
- `mail_send_atc_shadow` with `AM_ATC_WRITE_MODE=shadow`
- `mail_send_atc_live` with `AM_ATC_WRITE_MODE=live`

All three reuse the same seeded message-delivery fixture so the only intentional
dimension change is the ATC write mode.

The gate runs each benchmark with `3` warmups and `50` measured iterations.
That sample plan matters because a 10-sample p95 collapses to the worst
observed sample and was too noisy for this hot path.

## Gate Policy

The CI gate is defined by:

- `.github/workflows/atc-perf-gate.yml`
- `scripts/bench_atc_perf_gate.sh`
- `benches/atc_perf_baseline.json`

The gate enforces one blocking constraint:

1. `mail_send_atc_shadow` and `mail_send_atc_live` must each stay within a
   `5%` p95 overhead budget relative to `mail_send_no_atc`.

Every run also emits a canary report. The report is intentionally advisory and
does not mutate operator configuration. It records p50/p95/p99 latency for the
three benchmark modes, `PRAGMA quick_check` for the canary database when one is
available, the observed `atc_experiences` row count, and a fallback verdict such
as `canary_passed`, `manual_review`, `hold_live`, `hold_rollout`, or
`disable_live`. A successful latency run with zero ATC experience rows is not a
valid live canary; it holds live rollout until the durable ATC path is actually
exercised.

The full bench path supplies an isolated server-canary database automatically.
After the latency samples complete, the gate starts a short MCP stdio server
against that database and an isolated `STORAGE_ROOT`, registers two canary
agents, and sends one server-side message so the durable ATC experience path is
exercised. The stdio transcript, server stderr, and canary status JSON are
stored beside the benchmark report for replayable operator evidence. Reused
reports can pass `--canary-db <path>`; otherwise the latency verdict is still
computed, but the fallback verdict stays in `manual_review` for successful runs
because database health was not checked. Explicit `--canary-db` inputs are
inspected read-only; the server canary only mutates the auto-created full-run
database.
Each run also publishes a copy to
`$STORAGE_ROOT/atc_perf_gate/latest_canary_report.json` by default, or to
`$AM_ATC_CANARY_REPORT_DIR/latest_canary_report.json` when that override is
set. Robot ATC output and the TUI System Health screen read that latest report
and show the fallback verdict with the original artifact path.

The checked-in baseline file is still passed to `am bench --baseline`, but the
absolute host-to-host delta is advisory context rather than a release-blocking
failure. That keeps the gate aligned with the seam requirement: ATC must not
inflate `send_message` by more than `5%`.

`perf-regression-acknowledged` can waive an intentional, reviewed regression,
but runtime failures still fail the workflow.

## Current Baseline

Initial checked-in capture from `2026-04-18`:

| Benchmark | Baseline p95 | Mean | p99 | Delta vs no_atc | Allowed p95 |
|---|---:|---:|---:|---:|---:|
| `mail_send_no_atc` | `205.78ms` | `189.28ms` | `213.61ms` | `+0.00%` | `205.78ms` |
| `mail_send_atc_shadow` | `206.13ms` | `200.13ms` | `286.77ms` | `+0.17%` | `216.07ms` |
| `mail_send_atc_live` | `214.05ms` | `197.38ms` | `291.76ms` | `+4.02%` | `216.07ms` |

The checked-in flat baseline file is intentionally minimal because `am bench
--baseline` expects benchmark-name to p95 mappings. The richer historical
capture lives in `tests/artifacts/perf/atc_pre_wiring_baseline.json`.

## Checked-In Artifacts

- `tests/artifacts/perf/atc_pre_wiring_baseline.json` preserves the initial
  pre-wire capture used to seed the first guard.
- `tests/artifacts/perf/atc_perf_gate/<run_id>/summary.json` stores each gate
  verdict.
- `tests/artifacts/perf/atc_perf_gate/<run_id>/canary_report.json` stores the
  canary latency, DB health, ATC row count, and fallback recommendation.
- `tests/artifacts/perf/atc_perf_gate/<run_id>/server_canary_status.json`,
  `server_canary_stdio.jsonl`, and `server_canary_stderr.log` preserve the
  MCP stdio canary evidence from the server-wrapped durable ATC path.
- `$STORAGE_ROOT/atc_perf_gate/latest_canary_report.json` stores the latest
  operator-surface copy. Override with `AM_ATC_CANARY_REPORT_DIR`.
- `tests/artifacts/perf/atc_perf_gate/<run_id>/comment.md` is the PR-facing
  summary emitted by the workflow.
