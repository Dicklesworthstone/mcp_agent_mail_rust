#!/usr/bin/env bash
# test_diff_strategy.sh - Dedicated E2E gate for br-f1e0m diff-strategy wiring.
#
# Exercises the public MailAppModel render-loop telemetry, validates the TUI
# perf summary sample, and runs the focused Criterion frame-bench comparison
# that emits the diff-strategy benchmark artifact.

E2E_SUITE="diff_strategy"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Diff Strategy E2E Suite (br-f1e0m)"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq not found; skipping diff strategy suite"
    e2e_summary
    exit 0
fi

PROJECT_ROOT="${E2E_PROJECT_ROOT:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"
PERF_ARTIFACT_BASE="${PROJECT_ROOT}/tests/artifacts/tui/perf_baselines"
BENCH_ARTIFACT_BASE="${PROJECT_ROOT}/tests/artifacts/perf"

find_latest_artifact() {
    local base_dir="$1" name="$2"
    find "$base_dir" -name "$name" -type f 2>/dev/null | sort -r | head -1
}

# Case 1: Compile the focused frame benchmark.

e2e_case_banner "Build diff strategy frame benchmark"
e2e_step_start "build_frame_bench"

FRAME_BENCH_BUILD_LOG="${E2E_ARTIFACT_DIR}/diagnostics/frame_bench_build.log"
set +e
e2e_run_cargo bench -p mcp-agent-mail-server --bench frame_bench --no-run \
    2>"$FRAME_BENCH_BUILD_LOG"
frame_bench_build_rc=$?
set -e

e2e_step_end "build_frame_bench"
e2e_save_artifact "diagnostics/frame_bench_build.log" "$(cat "$FRAME_BENCH_BUILD_LOG" 2>/dev/null)"

if [ "$frame_bench_build_rc" -eq 0 ]; then
    e2e_pass "frame_bench compiles with diff strategy comparison benches"
else
    e2e_fail "frame_bench failed to compile (rc=${frame_bench_build_rc})"
    e2e_summary
    exit 1
fi

# Case 2: Public render-loop integration test.

e2e_case_banner "Render-loop telemetry integration"
e2e_step_start "run_alien_render_loop"

ALIEN_STDOUT="${E2E_ARTIFACT_DIR}/alien_render_loop_stdout.txt"
ALIEN_STDERR="${E2E_ARTIFACT_DIR}/alien_render_loop_stderr.txt"
set +e
e2e_run_cargo test -p mcp-agent-mail-server --test alien_integration \
    alien_e2e_bayesian_render_loop_telemetry_from_mail_app_view -- --nocapture \
    >"$ALIEN_STDOUT" 2>"$ALIEN_STDERR"
alien_rc=$?
set -e

e2e_step_end "run_alien_render_loop"
e2e_save_artifact "alien_render_loop_stderr.txt" "$(cat "$ALIEN_STDERR" 2>/dev/null)"

if [ "$alien_rc" -eq 0 ]; then
    e2e_pass "MailAppModel render-loop diff telemetry integration passes"
else
    e2e_fail "MailAppModel render-loop diff telemetry integration failed (rc=${alien_rc})"
    e2e_summary
    exit 1
fi

# Case 3: Structured perf summary includes diff strategy sample.

e2e_case_banner "Structured perf summary"
e2e_step_start "run_tui_perf_summary"

PERF_STDOUT="${E2E_ARTIFACT_DIR}/tui_perf_stdout.txt"
PERF_STDERR="${E2E_ARTIFACT_DIR}/tui_perf_stderr.txt"
set +e
MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1 \
    e2e_run_cargo test -p mcp-agent-mail-server --test tui_perf_baselines \
    -- z_perf_baseline_report --nocapture \
    >"$PERF_STDOUT" 2>"$PERF_STDERR"
perf_rc=$?
set -e

e2e_step_end "run_tui_perf_summary"
e2e_save_artifact "tui_perf_stderr.txt" "$(cat "$PERF_STDERR" 2>/dev/null)"

SUMMARY_JSON="$(find_latest_artifact "$PERF_ARTIFACT_BASE" "summary.json")"
if [ -n "$SUMMARY_JSON" ] && [ -f "$SUMMARY_JSON" ]; then
    e2e_copy_artifact "$SUMMARY_JSON" "tui_perf_summary.json"
else
    e2e_fail "TUI perf baseline did not produce summary.json"
    e2e_summary
    exit 1
fi

if [ "$perf_rc" -eq 0 ]; then
    e2e_pass "TUI perf summary exits cleanly"
else
    e2e_fail "TUI perf summary failed (rc=${perf_rc})"
fi

DIFF_P95="$(jq '[.samples[] | select(.surface=="diff_strategy_render_loop") | .p95_us][0] // "absent"' "$SUMMARY_JSON")"
DIFF_DETAIL="$(jq -r '[.samples[] | select(.surface=="diff_strategy_render_loop") | .detail][0] // ""' "$SUMMARY_JSON")"

if [ "$DIFF_P95" != "absent" ] && [ "$DIFF_P95" -lt 15000 ]; then
    e2e_pass "diff strategy render-loop p95: ${DIFF_P95}us < 15ms"
elif [ "$DIFF_P95" = "absent" ]; then
    e2e_fail "diff strategy render-loop sample missing from perf summary"
else
    e2e_fail "diff strategy render-loop p95: ${DIFF_P95}us exceeds 15ms"
fi

case "$DIFF_DETAIL" in
    *shadow_incremental_actions=*)
        e2e_pass "diff strategy perf sample reports Bayesian shadow action coverage"
        ;;
    *)
        e2e_fail "diff strategy perf sample missing Bayesian shadow action coverage"
        ;;
esac

# Case 4: Focused Criterion comparison and artifact contract.

e2e_case_banner "Criterion wired-vs-baseline comparison"
e2e_step_start "run_frame_bench_comparison"

BENCH_STDOUT="${E2E_ARTIFACT_DIR}/frame_bench_stdout.txt"
BENCH_STDERR="${E2E_ARTIFACT_DIR}/frame_bench_stderr.txt"
set +e
e2e_run_cargo bench -p mcp-agent-mail-server --bench frame_bench \
    bench_render_1000_frames -- \
    --sample-size 10 --warm-up-time 0.1 --measurement-time 0.2 \
    >"$BENCH_STDOUT" 2>"$BENCH_STDERR"
bench_rc=$?
set -e

e2e_step_end "run_frame_bench_comparison"
e2e_save_artifact "frame_bench_stderr.txt" "$(cat "$BENCH_STDERR" 2>/dev/null)"

if [ "$bench_rc" -eq 0 ]; then
    e2e_pass "focused diff strategy Criterion comparison exits cleanly"
else
    e2e_fail "focused diff strategy Criterion comparison failed (rc=${bench_rc})"
    e2e_summary
    exit 1
fi

BENCH_JSON="$(find_latest_artifact "$BENCH_ARTIFACT_BASE" "diff_strategy_bench_*.json")"
if [ -n "$BENCH_JSON" ] && [ -f "$BENCH_JSON" ]; then
    e2e_copy_artifact "$BENCH_JSON" "diff_strategy_bench.json"
else
    e2e_fail "Criterion comparison did not produce diff_strategy_bench artifact"
    e2e_summary
    exit 1
fi

FRAMES="$(jq '.frames_per_iteration // 0' "$BENCH_JSON")"
STRATEGY_SKIPS="$(jq '(.strategy.actual.incremental // 0) + (.strategy.actual.deferred // 0)' "$BENCH_JSON")"
BASELINE_FULL="$(jq '.baseline.actual.full // 0' "$BENCH_JSON")"
BASELINE_SKIPS="$(jq '(.baseline.actual.incremental // 0) + (.baseline.actual.deferred // 0)' "$BENCH_JSON")"
RESIZE_FULL="$(jq '.strategy.resize_full_actions // 0' "$BENCH_JSON")"
STRATEGY_MISMATCHES="$(jq '.strategy.consecutive_audit_mismatches // 0' "$BENCH_JSON")"

if [ "$STRATEGY_SKIPS" -gt 0 ]; then
    e2e_pass "strategy mode applies ${STRATEGY_SKIPS} non-full frame decision(s)"
else
    e2e_fail "strategy mode did not apply any non-full frame decisions"
fi

if [ "$BASELINE_FULL" -eq "$FRAMES" ] && [ "$BASELINE_SKIPS" -eq 0 ]; then
    e2e_pass "baseline mode renders all ${FRAMES} frames through the full path"
else
    e2e_fail "baseline mode mismatch: full=${BASELINE_FULL} skips=${BASELINE_SKIPS} frames=${FRAMES}"
fi

if [ "$RESIZE_FULL" -gt 0 ]; then
    e2e_pass "resize frames stay on the full render path"
else
    e2e_fail "resize frames were not observed on the full render path"
fi

if [ "$STRATEGY_MISMATCHES" -eq 0 ]; then
    e2e_pass "strategy benchmark recorded zero consecutive audit mismatches"
else
    e2e_fail "strategy benchmark recorded ${STRATEGY_MISMATCHES} audit mismatch(es)"
fi

e2e_summary
