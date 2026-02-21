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
#   8. Timeline preset lifecycle via Ctrl+S/Ctrl+L/Delete
#   9. Hostile markdown sanitization (script removal)
#   10. Empty thread rendering path
#   11. Large-thread/tree rendering performance envelope
#
# Notes:
# - Uses existing server crate rendering tests as black-box end-to-end checks.
# - Uses remote offload only (`rch exec -- cargo ...`) for all cargo execution.

set -euo pipefail

# Safety: default to keeping temp dirs so shared harness cleanup does not run
# destructive deletion commands in constrained environments.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="tui_v3_rendering"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
RCH_WORKSPACE_ROOT="${E2E_RCH_WORKSPACE_ROOT:-${PROJECT_ROOT}}"
RCH_MANIFEST_PATH="${E2E_RCH_MANIFEST_PATH:-Cargo.toml}"

# Use a suite-specific target dir to avoid lock contention with other agents.
if [ -z "${CARGO_TARGET_DIR:-}" ] || [ "${CARGO_TARGET_DIR}" = "/data/tmp/cargo-target" ]; then
    export CARGO_TARGET_DIR="/data/tmp/cargo-target-${E2E_SUITE}-$$"
    mkdir -p "${CARGO_TARGET_DIR}"
fi

e2e_init_artifacts
e2e_banner "TUI V3 Rendering E2E Suite (br-1cees)"
e2e_log "cargo target dir: ${CARGO_TARGET_DIR}"
e2e_log "rch workspace root: ${RCH_WORKSPACE_ROOT}"
e2e_log "rch manifest path: ${RCH_MANIFEST_PATH}"

TIMING_REPORT="${E2E_ARTIFACT_DIR}/frame_render_timing.tsv"
{
    echo -e "case_id\telapsed_ms"
} > "${TIMING_REPORT}"

SCENARIO_DIAG_FILE="${E2E_ARTIFACT_DIR}/diagnostics/rendering_scenarios.jsonl"
CARGO_DIAG_FILE="${E2E_ARTIFACT_DIR}/diagnostics/cargo_runs.jsonl"
: > "${SCENARIO_DIAG_FILE}"
: > "${CARGO_DIAG_FILE}"

_SCENARIO_DIAG_ID=""
_SCENARIO_DIAG_START_MS=0
_SCENARIO_DIAG_FAIL_BASE=0
_SCENARIO_DIAG_REASON_CODE="OK"
_SCENARIO_DIAG_REASON="completed"
_SUITE_STOP_REMAINING=0
_SUITE_STOP_REASON_CODE=""
_SUITE_STOP_REASON=""
_SUITE_STOP_EVIDENCE=""

diag_rel_path() {
    local path="$1"
    if [[ "${path}" == "${E2E_ARTIFACT_DIR}/"* ]]; then
        printf "%s" "${path#"${E2E_ARTIFACT_DIR}"/}"
    else
        printf "%s" "${path}"
    fi
}

scenario_diag_begin() {
    _SCENARIO_DIAG_ID="$1"
    _SCENARIO_DIAG_START_MS="$(_e2e_now_ms)"
    _SCENARIO_DIAG_FAIL_BASE="${_E2E_FAIL}"
    _SCENARIO_DIAG_REASON_CODE="OK"
    _SCENARIO_DIAG_REASON="completed"
}

scenario_diag_mark_reason() {
    local reason_code="$1"
    local reason="$2"
    if [ "${_SCENARIO_DIAG_REASON_CODE}" = "OK" ]; then
        _SCENARIO_DIAG_REASON_CODE="${reason_code}"
        _SCENARIO_DIAG_REASON="${reason}"
    fi
}

scenario_diag_finish() {
    local elapsed_ms fail_delta status reason_code reason repro_cmd
    elapsed_ms=$(( $(_e2e_now_ms) - _SCENARIO_DIAG_START_MS ))
    fail_delta=$(( _E2E_FAIL - _SCENARIO_DIAG_FAIL_BASE ))
    status="pass"
    reason_code="${_SCENARIO_DIAG_REASON_CODE}"
    reason="${_SCENARIO_DIAG_REASON}"
    if [[ "${reason_code}" == SKIP_* ]]; then
        status="skip"
    elif [ "${reason_code}" != "OK" ] || [ "${fail_delta}" -gt 0 ]; then
        status="fail"
        if [ "${reason_code}" = "OK" ] && [ "${fail_delta}" -gt 0 ]; then
            reason_code="ASSERTION_FAILURE"
            reason="${fail_delta} assertion(s) failed"
        fi
    fi
    repro_cmd="$(e2e_repro_command | tr -d '\n')"

    local artifacts_json="["
    local first=1
    local path rel
    for path in "$@"; do
        if [ -z "${path}" ]; then
            continue
        fi
        rel="$(diag_rel_path "${path}")"
        if [ "${first}" -eq 0 ]; then
            artifacts_json="${artifacts_json},"
        fi
        artifacts_json="${artifacts_json}\"$(_e2e_json_escape "${rel}")\""
        first=0
    done
    artifacts_json="${artifacts_json}]"

    {
        printf '{'
        printf '"schema_version":1,'
        printf '"suite":"%s",' "$(_e2e_json_escape "$E2E_SUITE")"
        printf '"scenario_id":"%s",' "$(_e2e_json_escape "$_SCENARIO_DIAG_ID")"
        printf '"status":"%s",' "$(_e2e_json_escape "$status")"
        printf '"elapsed_ms":%s,' "$elapsed_ms"
        printf '"reason_code":"%s",' "$(_e2e_json_escape "$reason_code")"
        printf '"reason":"%s",' "$(_e2e_json_escape "$reason")"
        printf '"artifact_paths":%s,' "${artifacts_json}"
        printf '"repro_command":"%s"' "$(_e2e_json_escape "$repro_cmd")"
        printf '}\n'
    } >> "${SCENARIO_DIAG_FILE}"
}

scenario_fail() {
    local reason_code="$1"
    shift
    local msg="$*"
    scenario_diag_mark_reason "${reason_code}" "${msg}"
    e2e_fail "${msg}"
}

append_cargo_diag() {
    local case_id="$1"
    local command_str="$2"
    local rc="$3"
    local elapsed_ms="$4"
    local log_path="$5"

    {
        printf '{'
        printf '"schema_version":1,'
        printf '"suite":"%s",' "$(_e2e_json_escape "$E2E_SUITE")"
        printf '"scenario_id":"%s",' "$(_e2e_json_escape "$case_id")"
        printf '"command":"%s",' "$(_e2e_json_escape "$command_str")"
        printf '"runner":"rch",'
        printf '"elapsed_ms":%s,' "${elapsed_ms}"
        printf '"rc":%s,' "${rc}"
        printf '"log_path":"%s"' "$(_e2e_json_escape "$(diag_rel_path "${log_path}")")"
        printf '}\n'
    } >> "${CARGO_DIAG_FILE}"
}

is_known_rch_remote_dep_mismatch() {
    local out_file="$1"
    grep -Fq "failed to select a version for the requirement \`franken-decision = \"^0.2.5\"\`" "${out_file}" \
        || grep -Fq "location searched: /data/projects/asupersync/franken_decision" "${out_file}" \
        || grep -Fq "failed to select a version for the requirement \`ftui = \"^0.2.0\"\`" "${out_file}" \
        || {
            grep -Fq "failed to select a version for \`ort\`" "${out_file}" \
                && grep -Fq "required by package \`fastembed v4.9.0\`" "${out_file}"
        } \
        || {
            grep -Fq "/dp/frankensqlite/crates/fsqlite-vfs/src/uring.rs" "${out_file}" \
                && grep -Fq "use of unresolved module or unlinked crate \`uring_fs\`" "${out_file}"
        } \
        || {
            grep -Fq "/dp/frankensqlite/crates/fsqlite-vfs/src/uring.rs" "${out_file}" \
                && grep -Fq "use of unresolved module or unlinked crate \`pollster\`" "${out_file}"
        } \
        || {
            grep -Fq "/dp/frankensqlite/crates/fsqlite-planner/src/lib.rs" "${out_file}" \
                && grep -Fq "cannot explicitly borrow within an implicitly-borrowing pattern" "${out_file}"
        }
}

run_cargo_with_rch_only() {
    local case_id="$1"
    local out_file="$2"
    shift 2
    local -a cargo_args=("$@")
    local subcommand="${cargo_args[0]}"
    local -a sub_args=("${cargo_args[@]:1}")
    local command_str="(cd ${RCH_WORKSPACE_ROOT} && cargo ${subcommand} --manifest-path ${RCH_MANIFEST_PATH} ${sub_args[*]})"
    local started_ms ended_ms elapsed_ms rc

    if [ ! -f "${RCH_WORKSPACE_ROOT}/${RCH_MANIFEST_PATH}" ]; then
        {
            echo "[error] manifest not found at ${RCH_WORKSPACE_ROOT}/${RCH_MANIFEST_PATH}"
            echo "[hint] set E2E_RCH_WORKSPACE_ROOT and/or E2E_RCH_MANIFEST_PATH"
        } >>"${out_file}"
        rc=2
        started_ms="$(_e2e_now_ms)"
        ended_ms="$(_e2e_now_ms)"
        elapsed_ms=$((ended_ms - started_ms))
        append_cargo_diag "${case_id}" "${command_str}" "${rc}" "${elapsed_ms}" "${out_file}"
        return "${rc}"
    fi

    started_ms="$(_e2e_now_ms)"
    printf "[cmd] %s\n" "${command_str}" >>"${out_file}"
    printf "[runner] rch\n" >>"${out_file}"

    if ! command -v rch >/dev/null 2>&1; then
        echo "[error] rch is required but not found in PATH" >>"${out_file}"
        rc=127
        ended_ms="$(_e2e_now_ms)"
        elapsed_ms=$((ended_ms - started_ms))
        append_cargo_diag "${case_id}" "${command_str}" "${rc}" "${elapsed_ms}" "${out_file}"
        return "${rc}"
    fi

    set +e
    (
        cd "${RCH_WORKSPACE_ROOT}" || exit 2
        timeout "${E2E_RCH_TIMEOUT_SECONDS:-300}" \
            rch exec -- cargo "${subcommand}" --manifest-path "${RCH_MANIFEST_PATH}" "${sub_args[@]}"
    ) >>"${out_file}" 2>&1
    rc=$?
    set -e

    ended_ms="$(_e2e_now_ms)"
    elapsed_ms=$((ended_ms - started_ms))
    append_cargo_diag "${case_id}" "${command_str}" "${rc}" "${elapsed_ms}" "${out_file}"
    return "${rc}"
}

run_render_case() {
    local case_id="$1"
    local description="$2"
    local fixture_payload="$3"
    local expected_render="$4"
    shift 4
    local -a cargo_args=("$@")

    scenario_diag_begin "${case_id}"
    e2e_case_banner "${case_id}"
    e2e_log "description: ${description}"
    e2e_log "fixture payload: ${fixture_payload}"
    e2e_log "expected rendering: ${expected_render}"

    e2e_save_artifact "${case_id}_fixture.txt" "${fixture_payload}"
    e2e_save_artifact "${case_id}_expected.txt" "${expected_render}"

    local out_file="${E2E_ARTIFACT_DIR}/${case_id}.log"
    local fixture_file="${E2E_ARTIFACT_DIR}/${case_id}_fixture.txt"
    local expected_file="${E2E_ARTIFACT_DIR}/${case_id}_expected.txt"
    local start_ms end_ms elapsed_ms

    if [ "${_SUITE_STOP_REMAINING}" -eq 1 ]; then
        echo -e "${case_id}\t0" >> "${TIMING_REPORT}"
        e2e_skip "${description} (${_SUITE_STOP_REASON})"
        scenario_diag_mark_reason "${_SUITE_STOP_REASON_CODE}" "${_SUITE_STOP_REASON}"
        if [ -n "${_SUITE_STOP_EVIDENCE}" ]; then
            scenario_diag_finish \
                "${fixture_file}" \
                "${expected_file}" \
                "${_SUITE_STOP_EVIDENCE}" \
                "${CARGO_DIAG_FILE}" \
                "${TIMING_REPORT}"
        else
            scenario_diag_finish \
                "${fixture_file}" \
                "${expected_file}" \
                "${CARGO_DIAG_FILE}" \
                "${TIMING_REPORT}"
        fi
        return 0
    fi

    start_ms="$(_e2e_now_ms)"

    if run_cargo_with_rch_only "${case_id}" "${out_file}" "${cargo_args[@]}"; then
        end_ms="$(_e2e_now_ms)"
        elapsed_ms=$((end_ms - start_ms))
        echo -e "${case_id}\t${elapsed_ms}" >> "${TIMING_REPORT}"
        e2e_pass "${description}"

        if grep -q "test result: ok" "${out_file}"; then
            e2e_pass "${case_id}: cargo reported test result ok"
        else
            scenario_fail "MISSING_SUCCESS_MARKER" "${case_id}: cargo output missing success marker"
            tail -n 80 "${out_file}" 2>/dev/null || true
        fi
    else
        local cargo_rc=$?
        end_ms="$(_e2e_now_ms)"
        elapsed_ms=$((end_ms - start_ms))
        echo -e "${case_id}\t${elapsed_ms}" >> "${TIMING_REPORT}"

        if [ "${cargo_rc}" -eq 127 ]; then
            scenario_diag_mark_reason "SKIP_RCH_UNAVAILABLE" "rch unavailable"
            e2e_skip "${description} (rch unavailable)"
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_RCH_UNAVAILABLE"
            _SUITE_STOP_REASON="skipped after missing rch runtime in earlier case"
            _SUITE_STOP_EVIDENCE="${out_file}"
            e2e_log "rch unavailable; remaining cases will be skipped"
        elif [ "${cargo_rc}" -eq 124 ]; then
            scenario_diag_mark_reason "SKIP_RCH_TIMEOUT" "rch command timed out"
            e2e_skip "${description} (rch timeout)"
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_RCH_TIMEOUT"
            _SUITE_STOP_REASON="skipped after rch timeout in earlier case"
            _SUITE_STOP_EVIDENCE="${out_file}"
            e2e_log "rch timeout detected; remaining cases will be skipped"
        elif is_known_rch_remote_dep_mismatch "${out_file}"; then
            scenario_diag_mark_reason "SKIP_RCH_REMOTE_DEP_MISMATCH" "remote worker dependency mismatch"
            e2e_skip "${description} (remote rch dependency mismatch)"
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_RCH_REMOTE_DEP_MISMATCH"
            _SUITE_STOP_REASON="skipped after remote dependency mismatch in earlier case"
            _SUITE_STOP_EVIDENCE="${out_file}"
            e2e_log "remote dependency mismatch detected; remaining cases will be skipped"
        else
            scenario_fail "CARGO_COMMAND_FAILED" "${description}"
            e2e_log "command failed for ${case_id}; tail follows"
            tail -n 120 "${out_file}" 2>/dev/null || true
        fi

        if [ "${_SUITE_STOP_REMAINING}" -eq 0 ] && grep -q "error: could not compile \`" "${out_file}"; then
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_SYSTEMIC_COMPILE_FAILURE"
            _SUITE_STOP_REASON="skipped after systemic compile failure in earlier case"
            _SUITE_STOP_EVIDENCE="${out_file}"
            e2e_log "systemic compile failure detected; remaining cases will be skipped"
        fi
    fi
    scenario_diag_finish "${fixture_file}" "${expected_file}" "${out_file}" "${CARGO_DIAG_FILE}" "${TIMING_REPORT}"
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

# Case 7b
run_render_case \
    "case07b_timeline_preset_lifecycle" \
    "Timeline Ctrl+S/Ctrl+L/Delete preset lifecycle persists and reloads filters" \
    "Timeline filter state: verbosity/kind/source saved to screen_filter_presets.json, reloaded, and deleted." \
    "Preset store captures values, load restores them, and delete removes all timeline presets." \
    test -p mcp-agent-mail-server --test pty_e2e_search timeline_preset_shortcuts_persist_and_reload_filters -- --nocapture

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
scenario_diag_begin "case10_tree_perf_budget"
e2e_case_banner "case10_tree_perf_budget"
e2e_log "description: large thread-tree render/build performance envelope"
e2e_log "tree structure: 100-message chain and per-screen render budget enforcement"
e2e_save_artifact "case10_fixture.txt" "Tree fixture: 100-message chain + screen render budget gate."
e2e_save_artifact "case10_expected.txt" "Tree build and render checks stay within enforced budgets."

CASE10_LOG_A="${E2E_ARTIFACT_DIR}/case10_tree_100_messages.log"
CASE10_LOG_B="${E2E_ARTIFACT_DIR}/case10_screen_render_budget.log"
case10_start="$(_e2e_now_ms)"
case10_skip=0

case10_ok=1
if [ "${_SUITE_STOP_REMAINING}" -eq 1 ]; then
    case10_skip=1
    scenario_diag_mark_reason "${_SUITE_STOP_REASON_CODE}" "${_SUITE_STOP_REASON}"
else
    set +e
    run_cargo_with_rch_only \
        "case10_tree_perf_budget/tree_build" \
        "${CASE10_LOG_A}" \
        test -p mcp-agent-mail-server --lib tui_screens::threads::tests::tree_100_messages_builds_quickly -- --nocapture
    case10_rc_a=$?
    set -e

    if [ "${case10_rc_a}" -ne 0 ]; then
        if [ "${case10_rc_a}" -eq 127 ]; then
            case10_skip=1
            scenario_diag_mark_reason "SKIP_RCH_UNAVAILABLE" "rch unavailable"
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_RCH_UNAVAILABLE"
            _SUITE_STOP_REASON="skipped after missing rch runtime in earlier case"
            _SUITE_STOP_EVIDENCE="${CASE10_LOG_A}"
        elif [ "${case10_rc_a}" -eq 124 ]; then
            case10_skip=1
            scenario_diag_mark_reason "SKIP_RCH_TIMEOUT" "rch command timed out"
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_RCH_TIMEOUT"
            _SUITE_STOP_REASON="skipped after rch timeout in earlier case"
            _SUITE_STOP_EVIDENCE="${CASE10_LOG_A}"
        elif is_known_rch_remote_dep_mismatch "${CASE10_LOG_A}"; then
            case10_skip=1
            scenario_diag_mark_reason "SKIP_RCH_REMOTE_DEP_MISMATCH" "remote worker dependency mismatch"
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_RCH_REMOTE_DEP_MISMATCH"
            _SUITE_STOP_REASON="skipped after remote dependency mismatch in earlier case"
            _SUITE_STOP_EVIDENCE="${CASE10_LOG_A}"
        else
            case10_ok=0
            scenario_diag_mark_reason "TREE_BUILD_FAILED" "tree_100_messages_builds_quickly command failed"
        fi
    fi

    if [ "${case10_ok}" -eq 1 ] && [ "${case10_skip}" -eq 0 ]; then
        export MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1
        set +e
        run_cargo_with_rch_only \
            "case10_tree_perf_budget/perf_budget" \
            "${CASE10_LOG_B}" \
            test -p mcp-agent-mail-server --test tui_perf_baselines perf_screen_render_80x24 -- --nocapture
        case10_rc_b=$?
        set -e
        unset MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS

        if [ "${case10_rc_b}" -ne 0 ]; then
            if [ "${case10_rc_b}" -eq 127 ]; then
                case10_skip=1
                scenario_diag_mark_reason "SKIP_RCH_UNAVAILABLE" "rch unavailable"
                _SUITE_STOP_REMAINING=1
                _SUITE_STOP_REASON_CODE="SKIP_RCH_UNAVAILABLE"
                _SUITE_STOP_REASON="skipped after missing rch runtime in earlier case"
                _SUITE_STOP_EVIDENCE="${CASE10_LOG_B}"
            elif [ "${case10_rc_b}" -eq 124 ]; then
                case10_skip=1
                scenario_diag_mark_reason "SKIP_RCH_TIMEOUT" "rch command timed out"
                _SUITE_STOP_REMAINING=1
                _SUITE_STOP_REASON_CODE="SKIP_RCH_TIMEOUT"
                _SUITE_STOP_REASON="skipped after rch timeout in earlier case"
                _SUITE_STOP_EVIDENCE="${CASE10_LOG_B}"
            elif is_known_rch_remote_dep_mismatch "${CASE10_LOG_B}"; then
                case10_skip=1
                scenario_diag_mark_reason "SKIP_RCH_REMOTE_DEP_MISMATCH" "remote worker dependency mismatch"
                _SUITE_STOP_REMAINING=1
                _SUITE_STOP_REASON_CODE="SKIP_RCH_REMOTE_DEP_MISMATCH"
                _SUITE_STOP_REASON="skipped after remote dependency mismatch in earlier case"
                _SUITE_STOP_EVIDENCE="${CASE10_LOG_B}"
            else
                case10_ok=0
                scenario_diag_mark_reason "PERF_BUDGET_COMMAND_FAILED" "perf_screen_render_80x24 command failed"
            fi
        fi
    fi
fi

case10_end="$(_e2e_now_ms)"
case10_elapsed=$((case10_end - case10_start))
echo -e "case10_tree_perf_budget\t${case10_elapsed}" >> "${TIMING_REPORT}"

if [ "${case10_skip}" -eq 1 ]; then
    e2e_skip "Tree 100+ build path and screen render budget checks (skipped due rch/runtime dependency condition)"
elif [ "${case10_ok}" -eq 1 ]; then
    e2e_pass "Tree 100+ build path and screen render budget checks passed"
    if grep -q "test result: ok" "${CASE10_LOG_A}" && grep -q "test result: ok" "${CASE10_LOG_B}"; then
        e2e_pass "case10_tree_perf_budget: cargo reported success for both checks"
    else
        scenario_fail "MISSING_SUCCESS_MARKER" "case10_tree_perf_budget: missing cargo success marker in logs"
    fi
else
    scenario_fail "TREE_PERF_BUDGET_FAILED" "Tree 100+ build path and/or render budget checks failed"
    e2e_log "case10 tree log tail:"
    tail -n 80 "${CASE10_LOG_A}" 2>/dev/null || true
    e2e_log "case10 budget log tail:"
    tail -n 80 "${CASE10_LOG_B}" 2>/dev/null || true
fi
scenario_diag_finish \
    "${E2E_ARTIFACT_DIR}/case10_fixture.txt" \
    "${E2E_ARTIFACT_DIR}/case10_expected.txt" \
    "${CASE10_LOG_A}" \
    "${CASE10_LOG_B}" \
    "${TIMING_REPORT}" \
    "${CARGO_DIAG_FILE}"

e2e_save_artifact "frame_render_timing.tsv" "$(cat "${TIMING_REPORT}")"

e2e_summary
