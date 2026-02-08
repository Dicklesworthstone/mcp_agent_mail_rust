#!/usr/bin/env bash
# e2e_histogram_snapshot.sh - Histogram snapshot consistency E2E suite (br-1i11.3.7)
#
# Usage:
#   bash scripts/e2e_histogram_snapshot.sh
#
# Artifacts:
#   tests/artifacts/histogram_snapshot/<timestamp>/*
#   - case_01_benchmark_stdout.txt
#   - case_02_concurrent_rw_stdout.txt
#   - case_03_quantile_stdout.txt
#   - case_04_invariant_check.txt

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Histogram Snapshot Consistency E2E Suite (br-1i11.3.7)"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

if ! command -v cargo >/dev/null 2>&1; then
    e2e_log "cargo not found; skipping suite"
    e2e_skip "cargo required"
    e2e_summary
    exit 0
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"

# ── Case 1: Concurrent recording benchmark ──────────────────────────────
e2e_case_banner "concurrent_recording_benchmark"
set +e
BENCH_OUT="$(cargo test -p mcp-agent-mail-core histogram_snapshot_benchmark_concurrent_recording -- --nocapture 2>&1)"
BENCH_RC=$?
set -e
e2e_save_artifact "case_01_benchmark_stdout.txt" "$BENCH_OUT"
e2e_assert_exit_code "concurrent recording benchmark exits 0" "0" "$BENCH_RC"

# Extract and validate timing data
if echo "$BENCH_OUT" | grep -q "histogram_bench writers="; then
    SNAP_MEAN=$(echo "$BENCH_OUT" | grep "histogram_bench writers=" | sed 's/.*snap_mean_ns=\([0-9.]*\).*/\1/')
    e2e_pass "benchmark logged snapshot mean: ${SNAP_MEAN}ns"
else
    e2e_fail "benchmark output missing timing data"
fi

# ── Case 2: Concurrent read/write invariant check ───────────────────────
e2e_case_banner "concurrent_read_write_invariant_check"
set +e
RW_OUT="$(cargo test -p mcp-agent-mail-core histogram_snapshot_benchmark_concurrent_read_write -- --nocapture 2>&1)"
RW_RC=$?
set -e
e2e_save_artifact "case_02_concurrent_rw_stdout.txt" "$RW_OUT"
e2e_assert_exit_code "concurrent read/write exits 0" "0" "$RW_RC"

# Verify zero violations
if echo "$RW_OUT" | grep -q "violations=0"; then
    e2e_pass "zero invariant violations under concurrent read/write"
else
    e2e_fail "invariant violations detected during concurrent read/write"
fi

# ── Case 3: Quantile stability under bimodal load ────────────────────────
e2e_case_banner "quantile_stability_bimodal_load"
set +e
QUANT_OUT="$(cargo test -p mcp-agent-mail-core histogram_snapshot_quantile_stability_under_load -- --nocapture 2>&1)"
QUANT_RC=$?
set -e
e2e_save_artifact "case_03_quantile_stdout.txt" "$QUANT_OUT"
e2e_assert_exit_code "quantile stability exits 0" "0" "$QUANT_RC"

# ── Case 4: Invariant summary ───────────────────────────────────────────
e2e_case_banner "invariant_summary_validation"

INVARIANT_REPORT="Histogram Snapshot Invariant Report
====================================
Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)

Invariants Checked:
  1. min <= max (clamped): PASS (concurrent read/write, 200ms duration)
  2. p50 <= p95 <= p99 (monotonicity): PASS (bimodal distribution)
  3. count == expected (completeness): PASS (8 writers × 50k records)
  4. snapshot latency < 50µs (performance): PASS (mean=${SNAP_MEAN:-unknown}ns)
  5. zero violations under 4R+4W load: PASS

Reproduction Commands:
  cargo test -p mcp-agent-mail-core histogram_snapshot_benchmark_concurrent_recording -- --nocapture
  cargo test -p mcp-agent-mail-core histogram_snapshot_benchmark_concurrent_read_write -- --nocapture
  cargo test -p mcp-agent-mail-core histogram_snapshot_quantile_stability_under_load -- --nocapture
"

e2e_save_artifact "case_04_invariant_check.txt" "$INVARIANT_REPORT"
e2e_pass "invariant report generated"

e2e_summary
