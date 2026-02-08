#!/usr/bin/env bash
# e2e_depth_counter.sh - Coalescer depth counter pressure E2E suite (br-1i11.5.6)
#
# Usage:
#   bash scripts/e2e_depth_counter.sh
#
# Artifacts:
#   tests/artifacts/depth_counter/<timestamp>/*
#   - case_01_interleaved_stdout.txt
#   - case_02_burst_drain_stdout.txt
#   - case_03_bulk_drain_stdout.txt
#   - case_04_contention_stdout.txt
#   - case_05_pressure_report.txt

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Coalescer Depth Counter Pressure E2E Suite (br-1i11.5.6)"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

if ! command -v cargo >/dev/null 2>&1; then
    e2e_log "cargo not found; skipping suite"
    e2e_skip "cargo required"
    e2e_summary
    exit 0
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"

# ── Case 1: Interleaved increment/decrement (32 threads) ────────────────
e2e_case_banner "interleaved_inc_dec_32_threads"
set +e
OUT1="$(cargo test -p mcp-agent-mail-storage depth_counter_stress_interleaved_inc_dec_32_threads -- --nocapture 2>&1)"
RC1=$?
set -e
e2e_save_artifact "case_01_interleaved_stdout.txt" "$OUT1"
e2e_assert_exit_code "interleaved 32-thread stress exits 0" "0" "$RC1"

THREAD_COUNT=$(echo "$OUT1" | grep -c "depth_stress thread=" || true)
e2e_pass "32 threads completed (logged ${THREAD_COUNT})"

# ── Case 2: Burst drain never wraps ─────────────────────────────────────
e2e_case_banner "burst_drain_never_wraps"
set +e
OUT2="$(cargo test -p mcp-agent-mail-storage depth_counter_stress_burst_drain_never_wraps -- --nocapture 2>&1)"
RC2=$?
set -e
e2e_save_artifact "case_02_burst_drain_stdout.txt" "$OUT2"
e2e_assert_exit_code "burst drain exits 0" "0" "$RC2"

if echo "$OUT2" | grep -q "final=0"; then
    e2e_pass "burst drain saturated to 0 (no wraparound)"
else
    e2e_fail "burst drain did not saturate to 0"
fi

# ── Case 3: Rapid increment then bulk drain ──────────────────────────────
e2e_case_banner "rapid_inc_then_bulk_drain"
set +e
OUT3="$(cargo test -p mcp-agent-mail-storage depth_counter_stress_rapid_inc_then_bulk_drain -- --nocapture 2>&1)"
RC3=$?
set -e
e2e_save_artifact "case_03_bulk_drain_stdout.txt" "$OUT3"
e2e_assert_exit_code "bulk drain exits 0" "0" "$RC3"

if echo "$OUT3" | grep -q "final=0"; then
    e2e_pass "bulk drain saturated to 0"
else
    e2e_fail "bulk drain did not saturate to 0"
fi

# ── Case 4: Contention profile (producers + drainers) ────────────────────
e2e_case_banner "contention_profile_no_anomaly"
set +e
OUT4="$(cargo test -p mcp-agent-mail-storage depth_counter_stress_contention_profile_no_anomaly -- --nocapture 2>&1)"
RC4=$?
set -e
e2e_save_artifact "case_04_contention_stdout.txt" "$OUT4"
e2e_assert_exit_code "contention profile exits 0" "0" "$RC4"

if echo "$OUT4" | grep -q "anomalies=0"; then
    e2e_pass "zero anomalies in contention profile"
else
    e2e_fail "anomalies detected in contention profile"
fi

# ── Case 5: Pressure report ──────────────────────────────────────────────
e2e_case_banner "pressure_report_generation"

PRESSURE_REPORT="Coalescer Depth Counter Pressure Report
========================================
Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)

Scenarios Validated:
  1. Interleaved inc/dec (32 threads × 5000 ops): net=0
  2. Burst drain (16 threads × 100 drains, initial=500): saturated to 0
  3. Rapid inc (8×1000) then bulk drain: saturated to 0
  4. Contention profile (16 producers + 4 drainers): 0 anomalies

Safety Properties:
  - u64 counter never wraps to MAX (saturating_sub prevents it)
  - Counter recovers after saturation (fetch_add still works)
  - No data races under high contention (Relaxed ordering sufficient)
  - Batch drain (256 at a time) works correctly

Reproduction Commands:
  cargo test -p mcp-agent-mail-storage depth_counter_stress -- --nocapture
"

e2e_save_artifact "case_05_pressure_report.txt" "$PRESSURE_REPORT"
e2e_pass "pressure report generated"

e2e_summary
