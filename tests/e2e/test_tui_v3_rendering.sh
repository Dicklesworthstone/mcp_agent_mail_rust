#!/usr/bin/env bash
# test_tui_v3_rendering.sh - TUI V3 rich rendering E2E suite (br-1cees)
#
# Coverage goals:
#   1. Markdown headings render in preview surfaces
#   2. JSON code fences retain syntax-rendered structure
#   3. Thread tree hierarchy construction for multi-reply chains
#   4. Tree expand/collapse navigation behavior
#   5. LogViewer severity presentation path
#   6. LogViewer filtering path (search/filter flow)
#   7. LogViewer auto-follow behavior
#   8. Hostile markdown sanitization (script removal)
#   9. Empty thread rendering path
#   10. Large-thread/tree rendering performance envelope
#
# Notes:
# - Uses existing server crate rendering tests as black-box end-to-end checks.
# - Prefers remote offload (`rch exec -- cargo ...`) with local fallback when
#   remote execution is unavailable/stalled.

set -euo pipefail

# Safety: default to keeping temp dirs so shared harness cleanup does not run
# destructive deletion commands in constrained environments.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="tui_v3_rendering"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

# Use a suite-specific target dir to avoid lock contention with other agents.
if [ -z "${CARGO_TARGET_DIR:-}" ] || [ "${CARGO_TARGET_DIR}" = "/data/tmp/cargo-target" ]; then
    export CARGO_TARGET_DIR="/data/tmp/cargo-target-${E2E_SUITE}-$$"
    mkdir -p "${CARGO_TARGET_DIR}"
fi

e2e_init_artifacts
e2e_banner "TUI V3 Rendering E2E Suite (br-1cees)"
e2e_log "cargo target dir: ${CARGO_TARGET_DIR}"

TIMING_REPORT="${E2E_ARTIFACT_DIR}/frame_render_timing.tsv"
{
    echo -e "case_id\telapsed_ms"
} > "${TIMING_REPORT}"

run_cargo_with_rch_fallback() {
    local out_file="$1"
    shift
    local -a cargo_args=("$@")
    local rc=0

    {
        echo "[cmd] cargo ${cargo_args[*]}"
    } >>"${out_file}"

    if command -v rch >/dev/null 2>&1; then
        {
            echo "[runner] rch"
        } >>"${out_file}"
        set +e
        timeout "${E2E_RCH_TIMEOUT_SECONDS:-240}" \
            rch exec -- cargo "${cargo_args[@]}" >>"${out_file}" 2>&1
        rc=$?
        set -e

        if [ "${rc}" -ne 0 ] && [ "${E2E_ALLOW_LOCAL_FALLBACK:-1}" = "1" ]; then
            {
                echo "[fallback] rch failed or stalled (rc=${rc}); retrying locally"
                echo "[runner] local"
            } >>"${out_file}"
            set +e
            cargo "${cargo_args[@]}" >>"${out_file}" 2>&1
            rc=$?
            set -e
        fi
    else
        {
            echo "[runner] local (rch missing)"
        } >>"${out_file}"
        set +e
        cargo "${cargo_args[@]}" >>"${out_file}" 2>&1
        rc=$?
        set -e
    fi

    return "${rc}"
}

run_render_case() {
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

    if run_cargo_with_rch_fallback "${out_file}" "${cargo_args[@]}"; then
        end_ms="$(_e2e_now_ms)"
        elapsed_ms=$((end_ms - start_ms))
        echo -e "${case_id}\t${elapsed_ms}" >> "${TIMING_REPORT}"
        e2e_pass "${description}"

        if grep -q "test result: ok" "${out_file}"; then
            e2e_pass "${case_id}: cargo reported test result ok"
        else
            e2e_fail "${case_id}: cargo output missing success marker"
            tail -n 80 "${out_file}" 2>/dev/null || true
        fi
    else
        end_ms="$(_e2e_now_ms)"
        elapsed_ms=$((end_ms - start_ms))
        echo -e "${case_id}\t${elapsed_ms}" >> "${TIMING_REPORT}"
        e2e_fail "${description}"
        e2e_log "command failed for ${case_id}; tail follows"
        tail -n 120 "${out_file}" 2>/dev/null || true
    fi
}

# Case 1
run_render_case \
    "case01_markdown_headings_preview" \
    "Markdown headings render with styled preview semantics" \
    $'# Release Plan\n\n## Checklist\n- [ ] migrate\n- [ ] verify' \
    "Heading tokens and list structure remain visible in rendered output." \
    test -p mcp-agent-mail-server --lib tui_markdown::tests::markdown_heading_renders -- --nocapture

# Case 2
run_render_case \
    "case02_json_code_fence_preview" \
    "JSON fenced block renders through markdown pipeline" \
    $'```json\n{\"service\":\"mail\",\"enabled\":true,\"retries\":3}\n```' \
    "Code-fence content survives rendering with language-aware formatting path." \
    test -p mcp-agent-mail-server --lib tui_markdown::tests::code_fence_priority_languages_render_content -- --nocapture

# Case 3
run_render_case \
    "case03_thread_tree_hierarchy" \
    "Thread tree hierarchy builds and preserves reply ordering" \
    $'root\n|- reply-1\n|- reply-2\n   `- reply-2a\n`- reply-3' \
    "Tree node nesting is stable and sorted for multi-reply structures." \
    test -p mcp-agent-mail-server --lib tui_screens::threads::tests::thread_tree_builder_nests_reply_chains_and_sorts_children -- --nocapture

# Case 4
run_render_case \
    "case04_tree_expand_collapse_keys" \
    "Tree expand/collapse responds to directional navigation inputs" \
    "Input sequence: Right to expand branch, Left to collapse branch." \
    "Visible node set changes as branches are expanded/collapsed." \
    test -p mcp-agent-mail-server --lib tui_screens::threads::tests::left_and_right_collapse_and_expand_selected_branch -- --nocapture

# Case 5
run_render_case \
    "case05_logviewer_severity_path" \
    "LogViewer timeline path preserves severity-tier visibility semantics" \
    "Fixture includes mixed severities (debug/info/warn/error)." \
    "Severity filtering includes and excludes rows according to verbosity tier." \
    test -p mcp-agent-mail-server --lib tui_screens::timeline::tests::verbosity_includes_severity_correctness -- --nocapture

# Case 6
run_render_case \
    "case06_logviewer_filtering" \
    "LogViewer filter/search flow narrows visible entries" \
    "Entry counts logged: before filter=all fixture rows, after filter=matching subset." \
    "Filtered result set is strictly smaller when query/filter is active." \
    test -p mcp-agent-mail-server --lib console::tests::timeline_pane_search_flow -- --nocapture

# Case 7
run_render_case \
    "case07_logviewer_autofollow" \
    "LogViewer auto-follow tracks newest event under streaming updates" \
    "Live append fixture with follow mode enabled." \
    "Cursor follows tail when new events are ingested." \
    test -p mcp-agent-mail-server --lib console::tests::timeline_pane_follow_tracks_new_events -- --nocapture

# Case 8
run_render_case \
    "case08_markdown_sanitization" \
    "Hostile markdown is sanitized (script tags removed)" \
    $'<script>alert("xss")</script>\n# Safe Header\nText remains.' \
    "No executable script survives; safe markdown content remains renderable." \
    test -p mcp-agent-mail-server --lib tui_markdown::tests::hostile_script_tag_safe_in_terminal -- --nocapture

# Case 9
run_render_case \
    "case09_empty_thread_placeholder_path" \
    "Empty-thread rendering path remains stable with no message rows" \
    "Thread detail receives empty message set." \
    "No-message rendering path executes without panic and preserves placeholder branch." \
    test -p mcp-agent-mail-server --lib tui_screens::threads::tests::render_full_screen_empty_no_panic -- --nocapture

# Case 10 (composite): 100+ tree build path + render budget gate.
e2e_case_banner "case10_tree_perf_budget"
e2e_log "description: large thread-tree render/build performance envelope"
e2e_log "tree structure: 100-message chain and per-screen render budget enforcement"
e2e_save_artifact "case10_fixture.txt" "Tree fixture: 100-message chain + screen render budget gate."
e2e_save_artifact "case10_expected.txt" "Tree build and render checks stay within enforced budgets."

CASE10_LOG_A="${E2E_ARTIFACT_DIR}/case10_tree_100_messages.log"
CASE10_LOG_B="${E2E_ARTIFACT_DIR}/case10_screen_render_budget.log"
case10_start="$(_e2e_now_ms)"

case10_ok=1
if ! run_cargo_with_rch_fallback \
    "${CASE10_LOG_A}" \
    test -p mcp-agent-mail-server --lib tui_screens::threads::tests::tree_100_messages_builds_quickly -- --nocapture; then
    case10_ok=0
fi

if [ "${case10_ok}" -eq 1 ]; then
    export MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1
    if ! run_cargo_with_rch_fallback \
        "${CASE10_LOG_B}" \
        test -p mcp-agent-mail-server --test tui_perf_baselines perf_screen_render_80x24 -- --nocapture; then
        case10_ok=0
    fi
    unset MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS
fi

case10_end="$(_e2e_now_ms)"
case10_elapsed=$((case10_end - case10_start))
echo -e "case10_tree_perf_budget\t${case10_elapsed}" >> "${TIMING_REPORT}"

if [ "${case10_ok}" -eq 1 ]; then
    e2e_pass "Tree 100+ build path and screen render budget checks passed"
    if grep -q "test result: ok" "${CASE10_LOG_A}" && grep -q "test result: ok" "${CASE10_LOG_B}"; then
        e2e_pass "case10_tree_perf_budget: cargo reported success for both checks"
    else
        e2e_fail "case10_tree_perf_budget: missing cargo success marker in logs"
    fi
else
    e2e_fail "Tree 100+ build path and/or render budget checks failed"
    e2e_log "case10 tree log tail:"
    tail -n 80 "${CASE10_LOG_A}" 2>/dev/null || true
    e2e_log "case10 budget log tail:"
    tail -n 80 "${CASE10_LOG_B}" 2>/dev/null || true
fi

e2e_save_artifact "frame_render_timing.tsv" "$(cat "${TIMING_REPORT}")"

e2e_summary
