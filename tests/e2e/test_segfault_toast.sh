#!/usr/bin/env bash
# test_segfault_toast.sh - E2E wrapper for TUI git segfault retry toasts.

set -euo pipefail

AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP:-1}"
export E2E_SUITE="segfault_toast"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Git Segfault Toast E2E Test Suite"

run_cargo_remote_first() {
    if [ "${E2E_SEGFAULT_TOAST_REMOTE_CARGO:-0}" = "1" ]; then
        e2e_run_cargo "$@"
    else
        E2E_CARGO_FORCE_LOCAL=1 e2e_run_cargo "$@"
    fi
}

run_cargo_case() {
    local case_id="$1"
    shift
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "$case_dir"

    e2e_case_banner "$case_id"
    e2e_mark_case_start "$case_id"
    printf 'run_cargo_remote_first %q ' "$@" >"${case_dir}/replay.sh"
    printf '\n' >>"${case_dir}/replay.sh"
    chmod +x "${case_dir}/replay.sh"

    set +e
    run_cargo_remote_first "$@" >"${case_dir}/stdout.log" 2>"${case_dir}/stderr.log"
    local status=$?
    set -e

    printf '%s\n' "$status" >"${case_dir}/exit"
    e2e_save_artifact "${case_id}/stdout.log" "$(cat "${case_dir}/stdout.log")"
    e2e_save_artifact "${case_id}/stderr.log" "$(cat "${case_dir}/stderr.log")"
    e2e_save_artifact "${case_id}/exit" "$status"
    e2e_mark_case_end "$case_id" "$status"

    if [ "$status" -eq 0 ]; then
        e2e_pass "$case_id passed"
    else
        e2e_fail "$case_id failed with exit ${status}"
    fi
}

run_cargo_case "s1_server_integration_badge_and_toast" \
    test -p mcp-agent-mail-server --test segfault_toast -- --nocapture

run_cargo_case "s2_inline_unit_formatter_and_rate_limit" \
    test -p mcp-agent-mail-server segfault_toast -- --nocapture

e2e_summary
