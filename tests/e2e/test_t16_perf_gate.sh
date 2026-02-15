#!/usr/bin/env bash
# test_t16_perf_gate.sh - Performance budget enforcement gate for T16 (br-1xt0m.1.13.4)
#
# Validates that render/action/memory guardrails are measured and enforced.
# Runs the perf regression suite and validates its artifacts contain the
# required budget structure.

E2E_SUITE="t16_perf_gate"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "T16 Performance Budget Enforcement Gate (br-1xt0m.1.13.4)"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    e2e_summary
    exit 0
fi

PROJECT_ROOT="${E2E_PROJECT_ROOT:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"
SUITES_DIR="${PROJECT_ROOT}/tests/e2e"
PERF_SUITE="${SUITES_DIR}/test_perf_regression.sh"

# ── Case 1: Perf regression suite exists ─────────────────────────────

e2e_case_banner "Perf regression suite availability"

if [ -f "$PERF_SUITE" ] && [ -x "$PERF_SUITE" ]; then
    e2e_pass "test_perf_regression.sh exists and is executable"
else
    e2e_fail "test_perf_regression.sh not found at ${PERF_SUITE}"
    e2e_summary
    exit 1
fi

# ── Case 2: Run perf regression suite ───────────────────────────────

e2e_case_banner "Perf regression execution"
e2e_step_start "perf_suite_run"

PERF_LOG="${E2E_ARTIFACT_DIR}/perf_regression.log"

set +e
SOAK_DURATION_SECS="${SOAK_DURATION_SECS:-5}" bash "$PERF_SUITE" >"$PERF_LOG" 2>&1
perf_rc=$?
set -e

e2e_step_end "perf_suite_run"

e2e_save_artifact "perf_regression.log" "$(cat "$PERF_LOG" 2>/dev/null)"

if [ "$perf_rc" -eq 0 ]; then
    e2e_pass "perf regression suite passes"
else
    e2e_fail "perf regression suite failed (rc=${perf_rc})"
fi

# ── Case 3: Render budget enforcement ───────────────────────────────

e2e_case_banner "Render budget enforcement"

# Find the perf summary artifact.
PERF_ARTIFACT_BASE="${PROJECT_ROOT}/tests/artifacts/tui/perf_baselines"
SUMMARY_JSON="$(find "$PERF_ARTIFACT_BASE" -name summary.json -type f 2>/dev/null | sort -r | head -1)"

if [ -z "$SUMMARY_JSON" ] || [ ! -f "$SUMMARY_JSON" ]; then
    e2e_fail "no perf summary.json artifact found"
    e2e_summary
    exit 1
fi

e2e_copy_artifact "$SUMMARY_JSON" "perf_summary.json"

# Verify render-related budgets exist and are enforced.
SCREEN_RENDER_COUNT="$(jq '[.samples[] | select(.surface=="screen_render")] | length' "$SUMMARY_JSON" 2>/dev/null)"
APP_RENDER_COUNT="$(jq '[.samples[] | select(.surface=="app_render")] | length' "$SUMMARY_JSON" 2>/dev/null)"

if [ "${SCREEN_RENDER_COUNT:-0}" -ge 14 ]; then
    e2e_pass "screen render budgets cover all 14 screens"
else
    e2e_fail "screen render budgets only cover ${SCREEN_RENDER_COUNT} screens (expected 14)"
fi

if [ "${APP_RENDER_COUNT:-0}" -ge 1 ]; then
    e2e_pass "app render budget present"
else
    e2e_fail "app render budget missing"
fi

RENDER_ALL_OK="$(jq '[.samples[] | select(.surface=="screen_render" or .surface=="app_render") | .within_budget] | all' "$SUMMARY_JSON" 2>/dev/null)"
if [ "$RENDER_ALL_OK" = "true" ]; then
    e2e_pass "all render budgets within limits"
else
    e2e_fail "some render budgets exceeded"
fi

# ── Case 4: Action dispatch budget enforcement ──────────────────────

e2e_case_banner "Action dispatch budget enforcement"

TICK_UPDATE="$(jq '[.samples[] | select(.surface=="tick_update")] | length' "$SUMMARY_JSON" 2>/dev/null)"
TICK_CYCLE="$(jq '[.samples[] | select(.surface=="tick_cycle")] | length' "$SUMMARY_JSON" 2>/dev/null)"

if [ "${TICK_UPDATE:-0}" -ge 1 ]; then
    e2e_pass "tick update budget present"
else
    e2e_fail "tick update budget missing"
fi

if [ "${TICK_CYCLE:-0}" -ge 1 ]; then
    e2e_pass "tick cycle budget present"
else
    e2e_fail "tick cycle budget missing"
fi

ACTION_ALL_OK="$(jq '[.samples[] | select(.surface=="tick_update" or .surface=="tick_cycle" or .surface=="key_navigation" or .surface=="search_interaction") | .within_budget] | all' "$SUMMARY_JSON" 2>/dev/null)"
if [ "$ACTION_ALL_OK" = "true" ]; then
    e2e_pass "all action dispatch budgets within limits"
else
    e2e_fail "some action dispatch budgets exceeded"
fi

# ── Case 5: Memory guardrails ───────────────────────────────────────

e2e_case_banner "Memory guardrails"

SOAK_ARTIFACT_BASE="${PROJECT_ROOT}/tests/artifacts/tui/soak_replay"
SOAK_JSON="$(find "$SOAK_ARTIFACT_BASE" -name report.json -type f 2>/dev/null | sort -r | head -1)"

if [ -n "$SOAK_JSON" ] && [ -f "$SOAK_JSON" ]; then
    e2e_copy_artifact "$SOAK_JSON" "soak_report.json"

    RSS_GROWTH="$(jq '.rss_growth_factor' "$SOAK_JSON" 2>/dev/null)"
    VERDICT="$(jq -r '.verdict' "$SOAK_JSON" 2>/dev/null)"
    ERRORS="$(jq '.errors' "$SOAK_JSON" 2>/dev/null)"

    if [ "$VERDICT" = "PASS" ]; then
        e2e_pass "soak verdict: PASS (RSS growth: ${RSS_GROWTH}x, errors: ${ERRORS})"
    elif [ -n "$VERDICT" ]; then
        e2e_fail "soak verdict: ${VERDICT} (RSS growth: ${RSS_GROWTH}x, errors: ${ERRORS})"
    fi
else
    e2e_skip "no soak report found (run perf_regression with soak enabled)"
fi

# ── Case 6: Budget structure completeness ────────────────────────────

e2e_case_banner "Budget structure completeness"

ALL_WITHIN="$(jq '.all_within_budget' "$SUMMARY_JSON" 2>/dev/null)"
BUILD_PROFILE="$(jq -r '.build_profile' "$SUMMARY_JSON" 2>/dev/null)"
SAMPLE_COUNT="$(jq '.samples | length' "$SUMMARY_JSON" 2>/dev/null)"

if [ "$ALL_WITHIN" = "true" ]; then
    e2e_pass "overall budget verdict: all within (${SAMPLE_COUNT} samples, ${BUILD_PROFILE} build)"
else
    e2e_fail "overall budget verdict: exceeded (${SAMPLE_COUNT} samples, ${BUILD_PROFILE} build)"
fi

# Verify each sample has the required structure.
STRUCT_OK="$(jq '[.samples[] | has("surface", "p50_us", "p95_us", "p99_us", "max_us", "budget_p95_us", "within_budget")] | all' "$SUMMARY_JSON" 2>/dev/null)"
if [ "$STRUCT_OK" = "true" ]; then
    e2e_pass "all ${SAMPLE_COUNT} samples have complete budget structure"
else
    e2e_fail "some samples missing budget structure fields"
fi

# ── Summary ──────────────────────────────────────────────────────────

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
