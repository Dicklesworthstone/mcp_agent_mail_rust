#!/usr/bin/env bash
# test_boot_check.sh - E2E coverage for archive boot-time integrity preflight.
# @tags: boot-check, startup

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

export E2E_SUITE="boot_check"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh disable=SC1091
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"
# shellcheck source=lib/structured_logging.sh disable=SC1091
source "${SCRIPT_DIR}/lib/structured_logging.sh"

e2e_init_artifacts
e2e_banner "Boot Check Startup E2E Test Suite"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

BOOT_EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${BOOT_EVENTS}"

for cmd in git python3 timeout; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        log_summary "boot_check" 5 0 0 "${E2E_ARTIFACT_DIR}" \
            "bash tests/e2e/test_boot_check.sh" >>"${BOOT_EVENTS}"
        e2e_summary
        exit 0
    fi
done

WORK="$(e2e_mktemp "e2e_boot_check")"
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"
SCENARIOS_TOTAL=5
SCENARIOS_PASSED=0
SCENARIOS_FAILED=0

pick_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

scenario_dir() {
    printf '%s/%s\n' "${E2E_ARTIFACT_DIR}" "$1"
}

boot_event() {
    local level="$1"
    local scenario="$2"
    local message="$3"
    local data="${4:-{}}"
    log_event "${level}" "boot_check" "${scenario}" "${message}" "${data}" >>"${BOOT_EVENTS}"
}

scenario_write_exit() {
    local scenario="$1"
    local exit_code="$2"
    mkdir -p "$(scenario_dir "${scenario}")"
    printf '%s\n' "${exit_code}" >"$(scenario_dir "${scenario}")/exit"
}

run_fixture_git() {
    local scenario="$1"
    local label="$2"
    local repo="$3"
    shift 3

    local out_dir
    out_dir="$(scenario_dir "${scenario}")/subprocesses/${label}"
    mkdir -p "${out_dir}"

    local rc=0
    set +e
    git -C "${repo}" "$@" >"${out_dir}/stdout" 2>"${out_dir}/stderr"
    rc=$?
    set -e
    printf '%s\n' "${rc}" >"${out_dir}/exit"

    if [ "${rc}" -ne 0 ]; then
        e2e_fail "${scenario}: git ${label} failed with exit ${rc}"
        boot_event fail "${scenario}" "fixture git command failed" \
            "{\"label\":\"$(_e2e_json_escape "${label}")\",\"exit\":${rc}}"
        return 1
    fi
}

init_clean_repo() {
    local scenario="$1"
    local repo="$2"
    mkdir -p "${repo}"
    run_fixture_git "${scenario}" "$(basename "${repo}")_init" "${repo}" init -q -b main || return 1
    run_fixture_git "${scenario}" "$(basename "${repo}")_config_name" "${repo}" config user.name "boot-check-e2e" || return 1
    run_fixture_git "${scenario}" "$(basename "${repo}")_config_email" "${repo}" config user.email "boot-check-e2e@example.invalid" || return 1
    printf 'boot check fixture\n' >"${repo}/README.md"
    run_fixture_git "${scenario}" "$(basename "${repo}")_add" "${repo}" add README.md || return 1
    run_fixture_git "${scenario}" "$(basename "${repo}")_commit" "${repo}" commit -q -m "initial" || return 1
}

init_orphan_repo() {
    local scenario="$1"
    local repo="$2"
    init_clean_repo "${scenario}" "${repo}" || return 1
    mkdir -p "${repo}/.git/refs"
    printf 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n' >"${repo}/.git/refs/stash"
}

init_corrupt_repo() {
    local repo="$1"
    mkdir -p "${repo}/.git"
    printf 'not valid git config = [\n' >"${repo}/.git/config"
}

extract_boot_events() {
    local scenario="$1"
    local case_dir
    case_dir="$(scenario_dir "${scenario}")"
    local text_events="${case_dir}/boot_events.txt"
    local json_events="${case_dir}/events.jsonl"
    : >"${text_events}"
    : >"${json_events}"

    grep -h "boot_check_" "${case_dir}/stdout" "${case_dir}/stderr" >"${text_events}" 2>/dev/null || true
    python3 - "${text_events}" "${json_events}" <<'PY'
import json
import re
import sys

names = [
    "boot_check_started",
    "boot_check_finding",
    "boot_check_completed",
    "boot_check_aborted",
    "boot_check_auto_repair_attempted",
    "boot_check_auto_repair_failed",
    "boot_check_auto_repair_gate_violated",
]
pattern = re.compile("|".join(re.escape(name) for name in names))
with open(sys.argv[1], encoding="utf-8", errors="replace") as source, open(
    sys.argv[2], "w", encoding="utf-8"
) as out:
    for line in source:
        for match in pattern.finditer(line):
            out.write(json.dumps({"event": match.group(0), "raw": line.rstrip()}) + "\n")
PY
}

event_count() {
    local scenario="$1"
    local event_name="$2"
    local events
    events="$(scenario_dir "${scenario}")/events.jsonl"
    if [ ! -f "${events}" ]; then
        printf '0\n'
        return
    fi
    python3 - "${events}" "${event_name}" <<'PY'
import json
import sys

count = 0
with open(sys.argv[1], encoding="utf-8") as handle:
    for line in handle:
        try:
            if json.loads(line).get("event") == sys.argv[2]:
                count += 1
        except json.JSONDecodeError:
            pass
print(count)
PY
}

e2e_assert_event_count_at_least() {
    local label="$1"
    local scenario="$2"
    local event_name="$3"
    local minimum="$4"
    local actual
    actual="$(event_count "${scenario}" "${event_name}")"
    if [ "${actual}" -ge "${minimum}" ]; then
        e2e_pass "${label} (count=${actual})"
    else
        e2e_fail "${label}"
        e2e_diff "${label}" ">=${minimum}" "${actual}"
    fi
}

run_server_case() {
    local scenario="$1"
    local storage_root="$2"
    local mode="$3"
    shift 3

    local case_dir db_path port pid rc
    case_dir="$(scenario_dir "${scenario}")"
    db_path="${case_dir}/storage.sqlite3"
    port="$(pick_port)"
    mkdir -p "${case_dir}" "${storage_root}"

    local -a env_parts=(
        "DATABASE_URL=sqlite:////${db_path}"
        "STORAGE_ROOT=${storage_root}"
        "HTTP_HOST=127.0.0.1"
        "HTTP_PORT=${port}"
        "AM_INTERFACE_MODE=mcp"
        "TUI_ENABLED=false"
        "AM_ATC_ENABLED=false"
        "HTTP_RBAC_ENABLED=0"
        "HTTP_RATE_LIMIT_ENABLED=0"
        "HTTP_JWT_ENABLED=0"
        "WORKTREES_ENABLED=false"
        "AM_BOOT_CHECK_MODE=${mode}"
        "RUST_LOG=mcp_agent_mail::boot_check=trace,mcp_agent_mail_server=info,mcp_agent_mail=info,warn"
    )
    while [ "$#" -gt 0 ]; do
        env_parts+=("$1")
        shift
    done

    set +e
    env "${env_parts[@]}" "${BIN}" serve --no-tui --no-reuse-running --host 127.0.0.1 --port "${port}" \
        >"${case_dir}/stdout" 2>"${case_dir}/stderr" &
    pid=$!
    set -e
    printf '%s\n' "${pid}" >"${case_dir}/pid"
    printf '%s\n' "${port}" >"${case_dir}/port"

    if e2e_wait_port 127.0.0.1 "${port}" 8; then
        printf 'started\n' >"${case_dir}/server_state"
        sleep 0.4
        kill "${pid}" 2>/dev/null || true
        set +e
        wait "${pid}" >/dev/null 2>&1
        rc=$?
        set -e
        printf '%s\n' "${rc}" >"${case_dir}/server_exit"
        extract_boot_events "${scenario}"
        return 0
    fi

    local deadline
    deadline=$(( $(date +%s) + 5 ))
    while kill -0 "${pid}" 2>/dev/null && [ "$(date +%s)" -lt "${deadline}" ]; do
        sleep 0.2
    done

    if kill -0 "${pid}" 2>/dev/null; then
        printf 'not_ready_still_running\n' >"${case_dir}/server_state"
        kill "${pid}" 2>/dev/null || true
        set +e
        wait "${pid}" >/dev/null 2>&1
        rc=$?
        set -e
        printf '%s\n' "${rc}" >"${case_dir}/server_exit"
        extract_boot_events "${scenario}"
        return 2
    fi

    set +e
    wait "${pid}" >/dev/null 2>&1
    rc=$?
    set -e
    printf 'exited\n' >"${case_dir}/server_state"
    printf '%s\n' "${rc}" >"${case_dir}/server_exit"
    extract_boot_events "${scenario}"
    return 1
}

scenario_clean_archive_warn_mode_starts_normally() {
    local scenario="$1"
    local storage="${WORK}/${scenario}/storage"
    mkdir -p "${storage}/projects"
    for idx in 1 2 3 4 5; do
        init_clean_repo "${scenario}" "${storage}/projects/healthy_${idx}" || return 1
    done

    if ! run_server_case "${scenario}" "${storage}" "warn"; then
        e2e_fail "${scenario}: clean archive server starts"
        return 1
    fi
    e2e_assert_eq "${scenario}: no boot findings logged" "0" "$(event_count "${scenario}" "boot_check_finding")"
    e2e_assert_eq "${scenario}: boot completed once" "1" "$(event_count "${scenario}" "boot_check_completed")"
}

scenario_seeded_findings_warn_mode_starts_with_warnings() {
    local scenario="$1"
    local storage="${WORK}/${scenario}/storage"
    mkdir -p "${storage}/projects"
    init_orphan_repo "${scenario}" "${storage}/projects/orphan_one" || return 1
    init_orphan_repo "${scenario}" "${storage}/projects/orphan_two" || return 1

    if ! run_server_case "${scenario}" "${storage}" "warn"; then
        e2e_fail "${scenario}: warn mode server starts with findings"
        return 1
    fi
    e2e_assert_eq "${scenario}: two boot findings logged" "2" "$(event_count "${scenario}" "boot_check_finding")"
    e2e_assert_eq "${scenario}: boot completed once" "1" "$(event_count "${scenario}" "boot_check_completed")"
}

scenario_seeded_findings_abort_mode_refuses_to_start() {
    local scenario="$1"
    local storage="${WORK}/${scenario}/storage"
    local rc
    mkdir -p "${storage}/projects"
    init_orphan_repo "${scenario}" "${storage}/projects/orphan_one" || return 1
    init_orphan_repo "${scenario}" "${storage}/projects/orphan_two" || return 1

    set +e
    run_server_case "${scenario}" "${storage}" "abort"
    rc=$?
    set -e

    if [ "${rc}" -eq 1 ]; then
        e2e_pass "${scenario}: abort mode refuses startup"
    else
        e2e_fail "${scenario}: abort mode should exit before serving"
        return 1
    fi
    e2e_assert_eq "${scenario}: abort event logged" "1" "$(event_count "${scenario}" "boot_check_aborted")"
    e2e_assert_eq "${scenario}: two findings logged before abort" "2" "$(event_count "${scenario}" "boot_check_finding")"
}

scenario_auto_repair_without_gate_logs_error_and_demotes() {
    local scenario="$1"
    local storage="${WORK}/${scenario}/storage"
    mkdir -p "${storage}/projects"
    init_orphan_repo "${scenario}" "${storage}/projects/orphan_one" || return 1

    if ! run_server_case "${scenario}" "${storage}" "auto_repair"; then
        e2e_fail "${scenario}: ungated auto_repair demotes and starts"
        return 1
    fi
    e2e_assert_event_count_at_least \
        "${scenario}: gate violation logged" \
        "${scenario}" \
        "boot_check_auto_repair_gate_violated" \
        1
    e2e_assert_eq "${scenario}: demoted warning still logs finding" "1" "$(event_count "${scenario}" "boot_check_finding")"
}

scenario_corrupt_repo_warn_mode_does_not_crash() {
    local scenario="$1"
    local storage="${WORK}/${scenario}/storage"
    mkdir -p "${storage}/projects"
    init_corrupt_repo "${storage}/projects/corrupt_config"

    if ! run_server_case "${scenario}" "${storage}" "warn"; then
        e2e_fail "${scenario}: corrupt repo warn mode starts"
        return 1
    fi
    e2e_assert_eq "${scenario}: corrupt repo logged as finding" "1" "$(event_count "${scenario}" "boot_check_finding")"
    if grep -Fq "panicked at" "$(scenario_dir "${scenario}")/stderr"; then
        e2e_fail "${scenario}: server stderr contains panic"
    else
        e2e_pass "${scenario}: server stderr contains no panic"
    fi
}

run_scenario() {
    local scenario="$1"
    local title="$2"
    local func="$3"
    local before_fail="${_E2E_FAIL}"
    local rc=0

    mkdir -p "$(scenario_dir "${scenario}")"
    e2e_case_banner "${title}"
    e2e_mark_case_start "${scenario}"
    boot_event info "${scenario}" "scenario started" "{}"

    if ! "${func}" "${scenario}"; then
        rc=1
    fi

    e2e_mark_case_end "${scenario}"

    if [ "${rc}" -ne 0 ] || [ "${_E2E_FAIL}" -gt "${before_fail}" ]; then
        scenario_write_exit "${scenario}" 1
        SCENARIOS_FAILED=$((SCENARIOS_FAILED + 1))
        boot_event fail "${scenario}" "scenario failed" "{\"exit\":1}"
    else
        scenario_write_exit "${scenario}" 0
        SCENARIOS_PASSED=$((SCENARIOS_PASSED + 1))
        boot_event pass "${scenario}" "scenario passed" "{\"exit\":0}"
    fi
}

run_scenario "s1_clean_archive_warn_mode_starts_normally" \
    "Clean archive warn mode starts normally" \
    scenario_clean_archive_warn_mode_starts_normally
run_scenario "s2_seeded_findings_warn_mode_starts_with_warnings" \
    "Seeded findings warn mode starts with warnings" \
    scenario_seeded_findings_warn_mode_starts_with_warnings
run_scenario "s3_seeded_findings_abort_mode_refuses_to_start" \
    "Seeded findings abort mode refuses startup" \
    scenario_seeded_findings_abort_mode_refuses_to_start
run_scenario "s4_auto_repair_without_gate_logs_error_and_demotes" \
    "Ungated auto repair logs error and demotes" \
    scenario_auto_repair_without_gate_logs_error_and_demotes
run_scenario "s5_corrupt_repo_warn_mode_does_not_crash" \
    "Corrupt repo warn mode does not crash" \
    scenario_corrupt_repo_warn_mode_does_not_crash

log_summary "boot_check" "${SCENARIOS_TOTAL}" "${SCENARIOS_PASSED}" "${SCENARIOS_FAILED}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_boot_check.sh" >>"${BOOT_EVENTS}"

e2e_summary
