#!/usr/bin/env bash
# test_tui_v3_charts.sh - TUI V3 chart visualization E2E suite (br-gk8vs)
#
# Coverage goals:
#   1. Dashboard throughput chart path after ToolCallEnd events
#   2. ToolMetrics bar/latency ribbon path with multi-tool data
#   3. Dashboard heatmap path for mixed event kinds
#   4. Chart rendering on compact terminals (80x24-class checks)
#   5. Chart rendering on wide terminals (200x50-class checks)
#   6. Empty-state chart rendering paths
#   7. Chart transition smoothing path

set -euo pipefail

# Safety: default to keeping temp dirs so shared harness cleanup does not run
# destructive deletion commands in constrained environments.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="tui_v3_charts"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

# Use a suite-specific target dir to avoid lock contention with other agents.
if [ -z "${CARGO_TARGET_DIR:-}" ] || [ "${CARGO_TARGET_DIR}" = "/data/tmp/cargo-target" ]; then
    export CARGO_TARGET_DIR="/data/tmp/cargo-target-${E2E_SUITE}-$$"
    mkdir -p "${CARGO_TARGET_DIR}"
fi

e2e_init_artifacts
e2e_banner "TUI V3 Charts E2E Suite (br-gk8vs)"
e2e_log "cargo target dir: ${CARGO_TARGET_DIR}"

TIMING_REPORT="${E2E_ARTIFACT_DIR}/chart_timing.tsv"
{
    echo -e "case_id\telapsed_ms"
} > "${TIMING_REPORT}"

run_cargo_with_rch_only() {
    local out_file="$1"
    shift
    local -a cargo_args=("$@")

    {
        echo "[cmd] cargo ${cargo_args[*]}"
        echo "[runner] rch"
    } >>"${out_file}"

    if ! command -v rch >/dev/null 2>&1; then
        {
            echo "[error] rch is required but not found in PATH"
        } >>"${out_file}"
        return 127
    fi

    timeout "${E2E_RCH_TIMEOUT_SECONDS:-300}" \
        rch exec -- cargo "${cargo_args[@]}" >>"${out_file}" 2>&1
}

run_chart_case() {
    local case_id="$1"
    local description="$2"
    local fixture_payload="$3"
    local expected_render="$4"
    shift 4
    local -a cargo_args=("$@")

    e2e_case_banner "${case_id}"
    e2e_log "description: ${description}"
    e2e_log "fixture payload: ${fixture_payload}"
    e2e_log "expected rendering: ${expected_render}"

    e2e_save_artifact "${case_id}_fixture.txt" "${fixture_payload}"
    e2e_save_artifact "${case_id}_expected.txt" "${expected_render}"

    local out_file="${E2E_ARTIFACT_DIR}/${case_id}.log"
    local start_ms end_ms elapsed_ms
    start_ms="$(_e2e_now_ms)"

    if run_cargo_with_rch_only "${out_file}" "${cargo_args[@]}"; then
        end_ms="$(_e2e_now_ms)"
        elapsed_ms=$((end_ms - start_ms))
        echo -e "${case_id}\t${elapsed_ms}" >> "${TIMING_REPORT}"
        e2e_pass "${description}"

        if grep -q "test result: ok" "${out_file}"; then
            e2e_pass "${case_id}: cargo reported test result ok"
        else
            e2e_fail "${case_id}: cargo output missing success marker"
            tail -n 120 "${out_file}" 2>/dev/null || true
        fi
    else
        end_ms="$(_e2e_now_ms)"
        elapsed_ms=$((end_ms - start_ms))
        echo -e "${case_id}\t${elapsed_ms}" >> "${TIMING_REPORT}"
        e2e_fail "${description}"
        e2e_log "command failed for ${case_id}; tail follows"
        tail -n 160 "${out_file}" 2>/dev/null || true
    fi
}

# Case 1: Dashboard LineChart path after ToolCallEnd events.
run_chart_case \
    "case01_dashboard_throughput_linechart" \
    "Dashboard throughput provider updates when ToolCallEnd events are ingested" \
    "Synthetic ToolCallEnd events in the same bucket to drive non-empty throughput series." \
    "Throughput data points become non-zero and renderable for chart series." \
    test -p mcp-agent-mail-server --lib \
    tui_widgets::tests::throughput_multiple_events_same_bucket -- --nocapture

# Case 2: ToolMetrics BarChart/latency ribbon path with multi-tool data.
run_chart_case \
    "case02_tool_metrics_barchart_multi_tool" \
    "ToolMetrics dashboard view renders chart path with multiple tool rows" \
    "Events for send_message + fetch_inbox populate per-tool percentiles." \
    "Latency ribbon/bar-chart path renders with populated samples." \
    test -p mcp-agent-mail-server --lib \
    tui_screens::tool_metrics::tests::dashboard_view_renders_with_data -- --nocapture

# Case 3: Dashboard heatmap path with mixed event kinds.
run_chart_case \
    "case03_dashboard_heatmap_mixed_events" \
    "Heatmap provider maps mixed event kinds into chart grid buckets" \
    "ToolCallEnd and MessageSent events in one bucket." \
    "Heatmap grid contains expected non-zero counts for mapped event-kind rows." \
    test -p mcp-agent-mail-server --lib \
    tui_widgets::tests::heatmap_provider_counts_by_kind -- --nocapture

# Case 4: Compact terminal rendering path.
run_chart_case \
    "case04_compact_terminal_scaling" \
    "Dashboard chart layout renders at 80x24 compact terminal size" \
    "Frame size 80x24." \
    "Chart and related panel layout paths execute without panic in compact mode." \
    test -p mcp-agent-mail-server --lib \
    tui_screens::dashboard::tests::dashboard_screen_renders_at_minimum_size -- --nocapture

# Case 5: Wide terminal rendering path.
run_chart_case \
    "case05_wide_terminal_scaling" \
    "Dashboard chart layout renders at 200x50 wide terminal size" \
    "Frame size 200x50." \
    "Chart and related panel layout paths execute without panic in wide mode." \
    test -p mcp-agent-mail-server --lib \
    tui_screens::dashboard::tests::dashboard_screen_renders_at_large_size -- --nocapture

# Case 6a: Empty-state for tool metrics charts.
run_chart_case \
    "case06a_empty_state_tool_metrics" \
    "ToolMetrics dashboard renders empty-state chart path gracefully" \
    "No tool data ingested." \
    "Empty-state visualization path renders without panic." \
    test -p mcp-agent-mail-server --lib \
    tui_screens::tool_metrics::tests::dashboard_view_renders_empty -- --nocapture

# Case 6b: Empty-state for heatmap provider.
run_chart_case \
    "case06b_empty_state_heatmap_provider" \
    "Heatmap provider returns empty columns with stable row dimensions" \
    "Event ring buffer left empty." \
    "Provider emits zero columns and fixed row count without panic." \
    test -p mcp-agent-mail-server --lib \
    tui_widgets::tests::heatmap_provider_empty_buffer -- --nocapture

# Case 7: Chart transition smoothness.
run_chart_case \
    "case07_chart_transition_smoothing" \
    "Chart transition interpolates smoothly between previous and target values" \
    "Transition duration 200ms with midpoint sample." \
    "Midpoint values are interpolated and clamped as expected by easing policy." \
    test -p mcp-agent-mail-server --lib \
    tui_widgets::tests::chart_transition_uses_ease_out_interpolation -- --nocapture

e2e_save_artifact "chart_timing.tsv" "$(cat "${TIMING_REPORT}")"

e2e_summary
