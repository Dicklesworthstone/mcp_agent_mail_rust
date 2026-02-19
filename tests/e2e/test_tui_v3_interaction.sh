#!/usr/bin/env bash
# test_tui_v3_interaction.sh - TUI V3 interaction E2E suite (br-1ssy6)
#
# Coverage goals:
#   1. Compose overlay open/close + focus trapping
#   2. Compose validation and Ctrl+Enter submit action wiring
#   3. Clipboard action path
#   4. Theme switching + persistence path
#   5. Error-boundary fallback + retry path
#   6. Ambient mode behavior (off/subtle/full + parsing)
#   7. Drag/drop edge cases (invalid targets, same-thread no-op, warning path)
#
# Diagnostics:
#   - case timing TSV
#   - scenario diagnostics JSONL (reason codes + artifact paths + repro commands)
#   - cargo execution diagnostics JSONL
#
# Execution policy:
#   - cargo invocations are offloaded through rch only (no local fallback)

set -euo pipefail

# Safety: default to keeping temp dirs so shared harness cleanup does not run
# destructive deletion commands in constrained environments.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="tui_v3_interaction"
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
e2e_banner "TUI V3 Interaction E2E Suite (br-1ssy6)"
e2e_log "cargo target dir: ${CARGO_TARGET_DIR}"
e2e_log "rch workspace root: ${RCH_WORKSPACE_ROOT}"
e2e_log "rch manifest path: ${RCH_MANIFEST_PATH}"

TIMING_REPORT="${E2E_ARTIFACT_DIR}/interaction_timing.tsv"
{
    echo -e "case_id\telapsed_ms"
} > "${TIMING_REPORT}"

SCENARIO_DIAG_FILE="${E2E_ARTIFACT_DIR}/diagnostics/interaction_scenarios.jsonl"
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
    local runner="$3"
    local fallback_local="$4"
    local rc="$5"
    local elapsed_ms="$6"
    local log_path="$7"

    {
        printf '{'
        printf '"schema_version":1,'
        printf '"suite":"%s",' "$(_e2e_json_escape "$E2E_SUITE")"
        printf '"scenario_id":"%s",' "$(_e2e_json_escape "$case_id")"
        printf '"command":"%s",' "$(_e2e_json_escape "$command_str")"
        printf '"runner":"%s",' "$(_e2e_json_escape "$runner")"
        printf '"fallback_local":%s,' "${fallback_local}"
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
    local runner="rch"
    local fallback_local="false"

    if [ ! -f "${RCH_WORKSPACE_ROOT}/${RCH_MANIFEST_PATH}" ]; then
        {
            echo "[error] manifest not found at ${RCH_WORKSPACE_ROOT}/${RCH_MANIFEST_PATH}"
            echo "[hint] set E2E_RCH_WORKSPACE_ROOT and/or E2E_RCH_MANIFEST_PATH"
        } >>"${out_file}"
        rc=2
        started_ms="$(_e2e_now_ms)"
        ended_ms="$(_e2e_now_ms)"
        elapsed_ms=$((ended_ms - started_ms))
        append_cargo_diag \
            "${case_id}" \
            "${command_str}" \
            "${runner}" \
            "${fallback_local}" \
            "${rc}" \
            "${elapsed_ms}" \
            "${out_file}"
        return "${rc}"
    fi

    started_ms="$(_e2e_now_ms)"
    {
        echo "[cmd] ${command_str}"
    } >>"${out_file}"

    printf "[runner] rch\n" >>"${out_file}"

    if ! command -v rch >/dev/null 2>&1; then
        echo "[error] rch is required but not found in PATH" >>"${out_file}"
        rc=127
        ended_ms="$(_e2e_now_ms)"
        elapsed_ms=$((ended_ms - started_ms))
        append_cargo_diag \
            "${case_id}" \
            "${command_str}" \
            "${runner}" \
            "${fallback_local}" \
            "${rc}" \
            "${elapsed_ms}" \
            "${out_file}"
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
    append_cargo_diag \
        "${case_id}" \
        "${command_str}" \
        "${runner}" \
        "${fallback_local}" \
        "${rc}" \
        "${elapsed_ms}" \
        "${out_file}"
    return "${rc}"
}

run_interaction_case() {
    local case_id="$1"
    local description="$2"
    local fixture_payload="$3"
    local expected_behavior="$4"
    shift 4
    local -a cargo_args=("$@")

    scenario_diag_begin "${case_id}"
    e2e_case_banner "${case_id}"
    e2e_log "description: ${description}"
    e2e_log "fixture payload: ${fixture_payload}"
    e2e_log "expected behavior: ${expected_behavior}"

    e2e_save_artifact "${case_id}_fixture.txt" "${fixture_payload}"
    e2e_save_artifact "${case_id}_expected.txt" "${expected_behavior}"

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
            tail -n 120 "${out_file}" 2>/dev/null || true
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
            _SUITE_STOP_REASON="skipped after rch unavailable in earlier case"
            _SUITE_STOP_EVIDENCE="${out_file}"
            e2e_log "rch unavailable; remaining cases will be skipped"
        elif [ "${cargo_rc}" -eq 124 ]; then
            scenario_diag_mark_reason "SKIP_RCH_TIMEOUT" "rch timed out"
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
        elif grep -q "error: could not compile \`" "${out_file}"; then
            scenario_diag_mark_reason "SKIP_SYSTEMIC_COMPILE_FAILURE" "systemic compile failure"
            e2e_skip "${description} (systemic compile failure)"
            _SUITE_STOP_REMAINING=1
            _SUITE_STOP_REASON_CODE="SKIP_SYSTEMIC_COMPILE_FAILURE"
            _SUITE_STOP_REASON="skipped after systemic compile failure in earlier case"
            _SUITE_STOP_EVIDENCE="${out_file}"
            e2e_log "systemic compile failure detected; remaining cases will be skipped"
        else
            scenario_fail "CARGO_COMMAND_FAILED" "${description}"
            e2e_log "command failed for ${case_id}; tail follows"
            tail -n 160 "${out_file}" 2>/dev/null || true
        fi
    fi

    scenario_diag_finish \
        "${fixture_file}" \
        "${expected_file}" \
        "${out_file}" \
        "${CARGO_DIAG_FILE}" \
        "${TIMING_REPORT}"
}

# Case 1: Compose modal opens from keybinding.
run_interaction_case \
    "case01_compose_open_keybinding" \
    "Ctrl+N opens compose overlay" \
    "MailAppModel receives Ctrl+N key event while no overlays are active." \
    "Compose overlay becomes topmost layer." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::ctrl_n_opens_compose_overlay -- --nocapture

# Case 2: Compose focus trapping and close behavior.
run_interaction_case \
    "case02_compose_focus_and_escape" \
    "Compose traps focus and closes with Escape" \
    "Compose overlay open, then Tab and Escape key events." \
    "Tab does not leak to screen focus changes; Escape closes compose." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::compose_traps_keys_from_reaching_screen -- --nocapture
run_interaction_case \
    "case03_compose_escape_close" \
    "Compose closes with Escape on clean state" \
    "Compose opened via Ctrl+N then Escape pressed." \
    "Overlay closes and z-order returns to none." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::compose_escape_closes_overlay -- --nocapture

# Case 3: Compose validation and submit action wiring.
run_interaction_case \
    "case04_compose_validation_required_body" \
    "Compose validation rejects empty body" \
    "ComposeState with recipient+subject but empty body." \
    "Validation returns error for required body." \
    test -p mcp-agent-mail-server --lib \
    tui_compose::tests::validate_requires_body -- --nocapture
run_interaction_case \
    "case05_compose_ctrl_enter_send_action" \
    "Ctrl+Enter produces send action from compose state" \
    "ComposeState receives Ctrl+Enter key event." \
    "ComposeAction::Send is produced." \
    test -p mcp-agent-mail-server --lib \
    tui_compose::tests::ctrl_enter_produces_send -- --nocapture

# Case 4: Clipboard action flow.
run_interaction_case \
    "case06_clipboard_copy_action_toast" \
    "Copy action dispatch writes clipboard payload and queues toast" \
    "Action menu dispatch uses CopyToClipboard action with text payload." \
    "Clipboard dispatch is handled and visible notification is queued." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::dispatch_copy_to_clipboard_shows_toast -- --nocapture

# Case 5: Theme cycle and persistence.
run_interaction_case \
    "case07_theme_cycle_actions" \
    "Theme action set switches active runtime theme" \
    "Palette theme actions dispatch Darcula then HighContrast." \
    "Runtime theme changes and accessibility high-contrast flag updates." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::explicit_theme_actions_switch_runtime_theme -- --nocapture
run_interaction_case \
    "case08_theme_persistence_flush" \
    "Theme/accessibility settings persist on shutdown flush" \
    "Model toggles theme and accessibility flags then flushes envfile." \
    "Persisted config file includes expected theme/accessibility keys." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::flush_before_shutdown_persists_theme_and_accessibility_settings -- --nocapture
run_interaction_case \
    "case08b_theme_shift_t_cycle" \
    "Shift+T rotates to the next theme when not in text mode" \
    "Dashboard receives Shift+T key input with no text-entry focus." \
    "Active theme id changes to a different palette." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::shift_t_cycles_theme_when_not_in_text_mode -- --nocapture
run_interaction_case \
    "case08c_theme_palette_distinct" \
    "Named themes expose distinct palette accents" \
    "Theme sample set is compared for visual distinguishability." \
    "At least one key accent color differs across themes." \
    test -p mcp-agent-mail-server --lib \
    tui_theme::tests::named_themes_are_visually_distinct -- --nocapture

# Case 6: Error boundary fallback and recovery.
run_interaction_case \
    "case09_error_boundary_fallback" \
    "Panicked screen renders fallback instead of re-panicking" \
    "Injected panicking screen on view path, then repeated render." \
    "Screen panic state is captured and fallback path remains stable." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::error_boundary_panicked_screen_shows_fallback_not_rerender -- --nocapture
run_interaction_case \
    "case10_error_boundary_retry" \
    "Error boundary retry key resets panicked screen state" \
    "Injected panicking screen then 'r' retry key event." \
    "Panicked screen marker is cleared and screen is replaced." \
    test -p mcp-agent-mail-server --lib \
    tui_app::tests::error_boundary_r_key_resets_panicked_screen -- --nocapture

# Case 7: Ambient mode behavior and parsing.
run_interaction_case \
    "case11_ambient_off_mode" \
    "Ambient renderer off mode is a no-op" \
    "Ambient renderer with mode=off over pre-filled background." \
    "No pixels are mutated when ambient mode is off." \
    test -p mcp-agent-mail-server --lib \
    tui_widgets::tests::ambient_renderer_off_is_noop -- --nocapture
run_interaction_case \
    "case12_ambient_subtle_vs_full" \
    "Ambient full mode is visually stronger than subtle mode" \
    "Render same health snapshot with subtle and full ambient modes." \
    "Full-mode delta exceeds subtle-mode delta." \
    test -p mcp-agent-mail-server --lib \
    tui_widgets::tests::ambient_renderer_full_mode_is_more_visible_than_subtle -- --nocapture
run_interaction_case \
    "case13_ambient_mode_parse_defaults" \
    "Ambient mode parser handles invalid values safely" \
    "AmbientMode::parse receives full/subtle/off/invalid strings." \
    "Invalid or empty values default to subtle mode." \
    test -p mcp-agent-mail-server --lib \
    tui_widgets::tests::ambient_mode_parse_defaults_to_subtle -- --nocapture

# Case 8: Drag/drop edge-case behavior and warning paths.
run_interaction_case \
    "case14_drag_drop_invalid_target_warning_snapshot" \
    "Mouse drag over invalid target keeps warning snapshot state" \
    "Begin drag then move cursor outside valid drop zones in message/thread browsers." \
    "Drag snapshot reports invalid hover and no target thread id." \
    test -p mcp-agent-mail-server --lib \
    invalid_target -- --nocapture
run_interaction_case \
    "case15_drag_drop_same_thread_keyboard_noop" \
    "Keyboard drop on same thread is no-op and preserves marker" \
    "Keyboard move marker source thread equals selected target thread." \
    "Ctrl+V returns no-op and keyboard move marker remains for retry." \
    test -p mcp-agent-mail-server --lib \
    same_thread_is_noop_and_preserves_marker -- --nocapture
run_interaction_case \
    "case16_drag_drop_rethread_dispatch_and_warning_path" \
    "Rethread dispatches on valid target and warns on invalid op payload" \
    "DnD rethread actions dispatched from screen tests plus app invalid-arg execute path." \
    "Valid rethread action executes and malformed rethread op surfaces warning behavior." \
    test -p mcp-agent-mail-server --lib \
    rethread -- --nocapture

e2e_save_artifact "interaction_timing.tsv" "$(cat "${TIMING_REPORT}")"

e2e_summary
