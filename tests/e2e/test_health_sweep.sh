#!/usr/bin/env bash
# test_health_sweep.sh - E2E suite for the System Health git-ref sweep.
# @tags: slow, tui, health-sweep
#
# Verifies the real PTY-backed server, real SQLite project registry, real git
# fixture repos, and the supported /mail/ws-state polling surface. No mocked
# sweep state.

set -euo pipefail

E2E_SUITE="health_sweep"
AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP:-1}"
AM_E2E_SERVER_TIMEOUT_S="${AM_E2E_SERVER_TIMEOUT_S:-50}"
E2E_PTY_COLUMNS="${E2E_PTY_COLUMNS:-140}"
E2E_PTY_ROWS="${E2E_PTY_ROWS:-42}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="${PROJECT_ROOT}/target"
fi
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"
# shellcheck source=lib/structured_logging.sh
source "${SCRIPT_DIR}/lib/structured_logging.sh"

e2e_init_artifacts
e2e_banner "Health Sweep E2E Test Suite"

HEALTH_EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${HEALTH_EVENTS}"

for cmd in curl git jq python3 script timeout; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        log_summary "health_sweep" 6 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_health_sweep.sh" >>"${HEALTH_EVENTS}"
        e2e_summary
        exit 0
    fi
done

WORK="$(e2e_mktemp "e2e_health_sweep")"

TOKEN=""
BASE_URL=""
WS_STATE_URL=""
API_URL=""
SCENARIO_WORK=""
SCENARIO_XDG=""
SCENARIO_HOME=""
SCENARIO_DB_PATH=""
SCENARIO_STORAGE_ROOT=""
SCENARIOS_TOTAL=6
SCENARIOS_PASSED=0
SCENARIOS_FAILED=0

trap 'e2e_stop_server || true; _e2e_cleanup || true' EXIT

hs_event() {
    local level="$1"
    local scenario="$2"
    local message="$3"
    local data="${4:-{}}"
    log_event "${level}" "health_sweep" "${scenario}" "${message}" "${data}" >>"${HEALTH_EVENTS}"
}

scenario_dir() {
    printf '%s/%s\n' "${E2E_ARTIFACT_DIR}" "$1"
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
        hs_event fail "${scenario}" "fixture git command failed" \
            "{\"label\":\"$(_e2e_json_escape "${label}")\",\"exit\":${rc},\"stderr\":\"$(_e2e_json_escape "${out_dir}/stderr")\"}"
        return 1
    fi
    return 0
}

init_clean_repo() {
    local scenario="$1"
    local repo="$2"
    mkdir -p "${repo}"
    run_fixture_git "${scenario}" "$(basename "${repo}")_init" "${repo}" init -q -b main || return 1
    run_fixture_git "${scenario}" "$(basename "${repo}")_config_name" "${repo}" config user.name "health-sweep-e2e" || return 1
    run_fixture_git "${scenario}" "$(basename "${repo}")_config_email" "${repo}" config user.email "health-sweep-e2e@example.invalid" || return 1
    printf 'health sweep fixture\n' >"${repo}/README.md"
    run_fixture_git "${scenario}" "$(basename "${repo}")_add" "${repo}" add README.md || return 1
    run_fixture_git "${scenario}" "$(basename "${repo}")_commit" "${repo}" commit -q -m "initial" || return 1
}

init_orphan_stash_repo() {
    local scenario="$1"
    local repo="$2"
    init_clean_repo "${scenario}" "${repo}" || return 1
    printf 'cafebabecafebabecafebabecafebabecafebabe\n' >"${repo}/.git/refs/stash"
}

init_dangling_branch_repo() {
    local scenario="$1"
    local repo="$2"
    init_clean_repo "${scenario}" "${repo}" || return 1
    mkdir -p "${repo}/.git/refs/heads"
    printf 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n' >"${repo}/.git/refs/heads/crash-recovery"
}

set_server_urls() {
    BASE_URL="${E2E_SERVER_URL%/mcp/}"
    WS_STATE_URL="${BASE_URL}/mail/ws-state"
    API_URL="${BASE_URL}/api/"
}

am_cli_bin() {
    if [ -x "${CARGO_TARGET_DIR}/debug/am" ]; then
        printf '%s\n' "${CARGO_TARGET_DIR}/debug/am"
        return 0
    fi
    if [ -x "${PROJECT_ROOT}/target/debug/am" ]; then
        printf '%s\n' "${PROJECT_ROOT}/target/debug/am"
        return 0
    fi
    command -v am 2>/dev/null || true
}

prepare_health_server_env() {
    local scenario="$1"

    SCENARIO_WORK="${WORK}/${scenario}"
    SCENARIO_XDG="${SCENARIO_WORK}/xdg_data"
    SCENARIO_HOME="${SCENARIO_WORK}/home"
    SCENARIO_DB_PATH="${SCENARIO_WORK}/mail.sqlite3"
    SCENARIO_STORAGE_ROOT="${SCENARIO_WORK}/storage"
    TOKEN="health-sweep-${scenario}-$(e2e_seeded_hex)"
    mkdir -p "${SCENARIO_WORK}" "${SCENARIO_XDG}" "${SCENARIO_HOME}" "${SCENARIO_STORAGE_ROOT}"
}

ensure_health_db_schema() {
    local scenario="$1"
    local label="$2"
    local am_bin
    am_bin="$(am_cli_bin)"
    if [ -z "${am_bin}" ] || [ ! -x "${am_bin}" ]; then
        e2e_fail "${scenario}: am CLI binary not found for DB seeding"
        return 1
    fi

    local out_dir
    out_dir="$(scenario_dir "${scenario}")/subprocesses/${label}_migrate"
    mkdir -p "${out_dir}" "$(dirname "${SCENARIO_DB_PATH}")"
    if [ ! -f "${SCENARIO_DB_PATH}" ]; then
        : >"${SCENARIO_DB_PATH}"
    fi

    local rc=0
    set +e
    AM_INTERFACE_MODE=cli \
        DATABASE_URL="sqlite://${SCENARIO_DB_PATH}" \
        STORAGE_ROOT="${SCENARIO_STORAGE_ROOT}" \
        XDG_DATA_HOME="${SCENARIO_XDG}" \
        HOME="${SCENARIO_HOME}" \
        WORKTREES_ENABLED=false \
        timeout 20 "${am_bin}" migrate >"${out_dir}/stdout" 2>"${out_dir}/stderr"
    rc=$?
    set -e
    printf '%s\n' "${rc}" >"${out_dir}/exit"

    if [ "${rc}" -ne 0 ]; then
        e2e_fail "${scenario}: DB schema initialization failed with exit ${rc}"
        return 1
    fi
}

seed_project_for_repo() {
    local scenario="$1"
    local label="$2"
    local repo="$3"

    ensure_health_db_schema "${scenario}" "seed_${label}" || return 1

    local out_dir
    out_dir="$(scenario_dir "${scenario}")/subprocesses/seed_${label}_project"
    mkdir -p "${out_dir}"

    local rc=0
    set +e
    python3 - "${SCENARIO_DB_PATH}" "${SCENARIO_STORAGE_ROOT}" "${repo}" \
        >"${out_dir}/stdout" 2>"${out_dir}/stderr" <<'PY'
import pathlib
import sqlite3
import sys
import time

db_path = sys.argv[1]
storage_root = pathlib.Path(sys.argv[2])
human_key = sys.argv[3]

def slugify(value: str) -> str:
    out = []
    prev_dash = False
    for ch in value.strip():
        if "A" <= ch <= "Z":
            ch = ch.lower()
        if ch.isascii() and ch.isalnum():
            out.append(ch)
            prev_dash = False
        elif not prev_dash:
            out.append("-")
            prev_dash = True
    slug = "".join(out).strip("-")
    return slug or "project"

slug = slugify(human_key)
now_us = int(time.time() * 1_000_000)
storage_root.joinpath("projects", slug).mkdir(parents=True, exist_ok=True)

conn = sqlite3.connect(db_path)
try:
    conn.execute(
        """
        INSERT INTO projects (slug, human_key, created_at)
        VALUES (?, ?, ?)
        ON CONFLICT(slug) DO UPDATE SET human_key = excluded.human_key
        """,
        (slug, human_key, now_us),
    )
    conn.commit()
finally:
    conn.close()

print(slug)
PY
    rc=$?
    set -e
    printf '%s\n' "${rc}" >"${out_dir}/exit"

    if [ "${rc}" -ne 0 ]; then
        e2e_fail "${scenario}: project seed failed for ${repo}"
        return 1
    fi

    cat "${out_dir}/stdout"
}

start_health_server() {
    local scenario="$1"
    shift

    if [ -z "${SCENARIO_WORK}" ] || [ "${SCENARIO_WORK}" != "${WORK}/${scenario}" ]; then
        prepare_health_server_env "${scenario}"
    fi

    if ! e2e_start_server_with_pty "${SCENARIO_DB_PATH}" "${SCENARIO_STORAGE_ROOT}" "${scenario}" \
        "HTTP_PATH=/api" \
        "HTTP_BEARER_TOKEN=${TOKEN}" \
        "TUI_ENABLED=true" \
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0" \
        "HTTP_RBAC_ENABLED=0" \
        "HTTP_RATE_LIMIT_ENABLED=0" \
        "CONSOLE_POLL_INTERVAL_MS=100" \
        "AM_HEALTH_SWEEP_INTERVAL_SEC=1" \
        "AM_HEALTH_SWEEP_BATCH=50" \
        "WORKTREES_ENABLED=false" \
        "XDG_DATA_HOME=${SCENARIO_XDG}" \
        "HOME=${SCENARIO_HOME}" \
        "$@"; then
        return 1
    fi
    set_server_urls
}

http_capture() {
    local case_id="$1"
    local method="$2"
    local url="$3"
    shift 3

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local body_file="${case_dir}/body.txt"
    local headers_file="${case_dir}/headers.txt"
    local stderr_file="${case_dir}/stderr.txt"
    local status_file="${case_dir}/status.txt"
    local timing_file="${case_dir}/timing.txt"
    local curl_exit_file="${case_dir}/curl_exit.txt"
    mkdir -p "${case_dir}"

    local curl_output=""
    local curl_rc=0
    local status="000"
    local timing_s="0"
    set +e
    curl_output="$(curl -sS -X "${method}" -D "${headers_file}" -o "${body_file}" \
        -w "%{http_code}\n%{time_total}" "$@" "${url}" 2>"${stderr_file}")"
    curl_rc=$?
    set -e

    printf '%s\n' "${curl_rc}" >"${curl_exit_file}"
    if [ "${curl_rc}" -eq 0 ]; then
        status="$(printf '%s\n' "${curl_output}" | sed -n '1p')"
        timing_s="$(printf '%s\n' "${curl_output}" | sed -n '2p')"
    fi
    if ! [[ "${status}" =~ ^[0-9]{3}$ ]]; then
        status="000"
    fi

    local timing_ms
    timing_ms="$(awk -v sec="${timing_s}" 'BEGIN { if (sec == "") sec = 0; printf "%.0f\n", sec * 1000 }' 2>/dev/null || echo "0")"
    printf '%s\n' "${status}" >"${status_file}"
    printf '%s\n' "${timing_ms}" >"${timing_file}"
    printf '%s\n' "${status}"
}

ws_state_url() {
    printf '%s?limit=5&include=system_health&token=%s\n' "${WS_STATE_URL}" "${TOKEN}"
}

ws_state_curl() {
    local case_id="$1"
    http_capture "${case_id}" GET "$(ws_state_url)"
}

wait_for_health_sweep_json() {
    local scenario="$1"
    local jq_filter="$2"
    local timeout_s="${3:-15}"
    local attempt=1
    local attempts=$((timeout_s * 2))
    local last_summary="(no payload)"

    while [ "${attempt}" -le "${attempts}" ]; do
        local case_id status body_file out_file summary
        case_id="$(printf '%s_wait_sweep_%02d' "${scenario}" "${attempt}")"
        status="$(ws_state_curl "${case_id}")"
        if [ "${status}" = "200" ]; then
            body_file="${E2E_ARTIFACT_DIR}/${case_id}/body.txt"
            out_file="$(scenario_dir "${scenario}")/git_ref_integrity.json"
            mkdir -p "$(scenario_dir "${scenario}")"
            if jq -e '.system_health.git_ref_integrity' "${body_file}" >"${out_file}"; then
                summary="$(jq -c '{enabled,total_projects,projects_scanned,total_findings,banner,am_git_binary_set,banner_suppressed_by_am_git_binary}' "${out_file}")"
                last_summary="${summary}"
                if jq -e "${jq_filter}" "${out_file}" >/dev/null; then
                    cp "${body_file}" "$(scenario_dir "${scenario}")/ws_state.json" 2>/dev/null || true
                    hs_event info "${scenario}" "observed health sweep payload" \
                        "{\"attempt\":${attempt},\"summary\":${summary}}"
                    printf '%s\n' "${out_file}"
                    return 0
                fi
            fi
        fi
        sleep 0.5
        attempt=$((attempt + 1))
    done

    e2e_fail "${scenario}: timed out waiting for health sweep payload matching ${jq_filter}; last=${last_summary}"
    return 1
}

scenario_default_interval_runs_periodically() {
    local scenario="$1"
    local repo="${WORK}/${scenario}/repo-clean"
    init_clean_repo "${scenario}" "${repo}" || return 1
    prepare_health_server_env "${scenario}"
    seed_project_for_repo "${scenario}" "clean" "${repo}" >/dev/null || return 1
    start_health_server "${scenario}" || return 1

    local json_file
    json_file="$(wait_for_health_sweep_json "${scenario}" '.enabled == true and .total_projects == 1 and .projects_scanned == 1' 25)" || return 1
    e2e_assert_eq "${scenario}: sweep is enabled" "true" "$(jq -r '.enabled' "${json_file}")"
    e2e_assert_eq "${scenario}: clean repo has one project" "1" "$(jq -r '.total_projects' "${json_file}")"
    e2e_assert_eq "${scenario}: clean repo has no findings" "0" "$(jq -r '.total_findings' "${json_file}")"
    e2e_assert_eq "${scenario}: clean repo level is OK" "OK" "$(jq -r '.level' "${json_file}")"
}

scenario_disabled_env_no_sweeps() {
    local scenario="$1"
    local repo="${WORK}/${scenario}/repo-clean"
    init_clean_repo "${scenario}" "${repo}" || return 1
    prepare_health_server_env "${scenario}"
    seed_project_for_repo "${scenario}" "clean" "${repo}" >/dev/null || return 1
    start_health_server "${scenario}" "AM_HEALTH_SWEEP_ENABLED=false" || return 1

    local json_file
    json_file="$(wait_for_health_sweep_json "${scenario}" '.enabled == false' 15)" || return 1
    e2e_assert_eq "${scenario}: payload shows disabled state" "false" "$(jq -r '.enabled' "${json_file}")"
    e2e_assert_eq "${scenario}: disabled sweep scans no projects" "0" "$(jq -r '.projects_scanned' "${json_file}")"
}

scenario_panel_visible_after_findings() {
    local scenario="$1"
    local repo="${WORK}/${scenario}/repo-orphan-stash"
    init_orphan_stash_repo "${scenario}" "${repo}" || return 1
    prepare_health_server_env "${scenario}"
    seed_project_for_repo "${scenario}" "stash" "${repo}" >/dev/null || return 1
    start_health_server "${scenario}" || return 1

    local json_file
    json_file="$(wait_for_health_sweep_json "${scenario}" '.total_findings == 1 and .projects[0].finding_count == 1' 25)" || return 1
    e2e_assert_eq "${scenario}: payload exposes finding count" "1" "$(jq -r '.total_findings' "${json_file}")"
    e2e_assert_eq "${scenario}: project summary exposes finding count" "1" "$(jq -r '.projects[0].finding_count' "${json_file}")"
    e2e_assert_eq "${scenario}: project summary warns" "WARN" "$(jq -r '.projects[0].classification' "${json_file}")"
}

scenario_banner_visible_after_findings() {
    local scenario="$1"
    local repo="${WORK}/${scenario}/repo-dangling-branch"
    init_dangling_branch_repo "${scenario}" "${repo}" || return 1
    prepare_health_server_env "${scenario}"
    seed_project_for_repo "${scenario}" "branch" "${repo}" >/dev/null || return 1
    start_health_server "${scenario}" || return 1

    local json_file banner
    json_file="$(wait_for_health_sweep_json "${scenario}" '.total_findings == 1 and .banner != null' 25)" || return 1
    banner="$(jq -r '.banner // ""' "${json_file}")"
    e2e_assert_contains "${scenario}: banner reports orphan refs" "${banner}" "registered projects have 1 orphan refs across 1 projects"
    e2e_assert_contains "${scenario}: banner points to doctor dry-run" "${banner}" "am doctor fix-orphan-refs --all --dry-run"
}

scenario_dismissal_takes_effect_next_cycle() {
    local scenario="$1"
    local repo="${WORK}/${scenario}/repo-orphan-stash"
    init_orphan_stash_repo "${scenario}" "${repo}" || return 1
    prepare_health_server_env "${scenario}"
    local slug
    slug="$(seed_project_for_repo "${scenario}" "stash" "${repo}")" || return 1
    start_health_server "${scenario}" || return 1

    wait_for_health_sweep_json "${scenario}" '.total_findings == 1 and .projects[0].finding_count == 1' 25 >/dev/null || return 1

    mkdir -p "${SCENARIO_XDG}/mcp-agent-mail"
    cat >"${SCENARIO_XDG}/mcp-agent-mail/sweep_dismissals.toml" <<EOTOML
[[dismissed]]
project_slug = "${slug}"
ref_kind = "orphan_stash"
dismissed_at = "2026-05-11T00:00:00Z"
reason = "health sweep e2e dismissal"
EOTOML
    hs_event info "${scenario}" "wrote dismissal" "{\"project_slug\":\"$(_e2e_json_escape "${slug}")\",\"ref_kind\":\"orphan_stash\"}"

    local json_file
    json_file="$(wait_for_health_sweep_json "${scenario}" '.total_projects == 1 and .total_findings == 0 and .banner == null' 25)" || return 1
    e2e_assert_eq "${scenario}: dismissed finding clears visible count" "0" "$(jq -r '.total_findings' "${json_file}")"
    e2e_assert_eq "${scenario}: dismissed finding suppresses banner" "true" "$(jq -r '.banner == null' "${json_file}")"
}

scenario_am_git_binary_suppresses_banner_only() {
    local scenario="$1"
    local repo="${WORK}/${scenario}/repo-orphan-stash"
    init_orphan_stash_repo "${scenario}" "${repo}" || return 1
    prepare_health_server_env "${scenario}"
    seed_project_for_repo "${scenario}" "stash" "${repo}" >/dev/null || return 1
    start_health_server "${scenario}" "AM_GIT_BINARY=/usr/bin/git" || return 1

    local json_file
    json_file="$(wait_for_health_sweep_json "${scenario}" '.total_findings == 1 and .am_git_binary_set == true and .banner_suppressed_by_am_git_binary == true' 25)" || return 1
    e2e_assert_eq "${scenario}: finding still visible" "1" "$(jq -r '.total_findings' "${json_file}")"
    e2e_assert_eq "${scenario}: banner suppression is explicit" "true" "$(jq -r '.banner_suppressed_by_am_git_binary' "${json_file}")"
    e2e_assert_eq "${scenario}: doctor banner is suppressed" "true" "$(jq -r '.banner == null' "${json_file}")"
}

run_scenario() {
    local scenario="$1"
    local title="$2"
    local func="$3"
    local before_fail="${_E2E_FAIL}"
    local rc=0

    mkdir -p "$(scenario_dir "${scenario}")"
    : >"$(scenario_dir "${scenario}")/stdout"
    : >"$(scenario_dir "${scenario}")/stderr"

    e2e_case_banner "${title}"
    e2e_mark_case_start "${scenario}"
    hs_event info "${scenario}" "scenario started" "{}"

    if ! "${func}" "${scenario}"; then
        rc=1
    fi

    e2e_mark_case_end "${scenario}"
    e2e_stop_server || true

    if [ "${rc}" -ne 0 ] || [ "${_E2E_FAIL}" -gt "${before_fail}" ]; then
        scenario_write_exit "${scenario}" 1
        SCENARIOS_FAILED=$((SCENARIOS_FAILED + 1))
        hs_event fail "${scenario}" "scenario failed" "{\"exit\":1}"
    else
        scenario_write_exit "${scenario}" 0
        SCENARIOS_PASSED=$((SCENARIOS_PASSED + 1))
        hs_event pass "${scenario}" "scenario passed" "{\"exit\":0}"
    fi
}

run_scenario "s1_default_interval_runs_periodically" \
    "Default interval runs periodic git-ref sweeps" \
    scenario_default_interval_runs_periodically
run_scenario "s2_disabled_env_no_sweeps" \
    "Disabled env prevents git-ref sweep events" \
    scenario_disabled_env_no_sweeps
run_scenario "s3_panel_visible_after_findings" \
    "Panel shows orphan-stash findings" \
    scenario_panel_visible_after_findings
run_scenario "s4_banner_in_startup_banner" \
    "Banner state points operators to doctor dry-run" \
    scenario_banner_visible_after_findings
run_scenario "s5_dismissal_takes_effect_next_cycle" \
    "Dismissal TOML filters findings on the next cycle" \
    scenario_dismissal_takes_effect_next_cycle
run_scenario "s6_am_git_binary_suppresses_banner_only" \
    "AM_GIT_BINARY suppresses banner while preserving findings" \
    scenario_am_git_binary_suppresses_banner_only

log_summary "health_sweep" "${SCENARIOS_TOTAL}" "${SCENARIOS_PASSED}" "${SCENARIOS_FAILED}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_health_sweep.sh" >>"${HEALTH_EVENTS}"

e2e_summary
