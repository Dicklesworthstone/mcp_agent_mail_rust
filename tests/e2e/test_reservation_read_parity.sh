#!/usr/bin/env bash
# test_reservation_read_parity.sh — MCP/robot/operator active-lease read parity.
# @tags: reservations, parity, robot, mcp

set -euo pipefail

E2E_SUITE="reservation_read_parity"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
source "${PROJECT_ROOT}/scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Reservation Read Surface Parity E2E"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    e2e_summary
    exit 0
fi

AM_BIN="$(e2e_ensure_binary am)"
WORK="$(e2e_mktemp reservation_read_parity)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
DB_PATH="${WORK}/mail.sqlite3"
STORAGE_ROOT="${WORK}/storage"
PROJECT_KEY="${WORK}/repo"
mkdir -p "${ART}" "${STORAGE_ROOT}" "${PROJECT_KEY}"

export AM_INTERFACE_MODE=cli
export DATABASE_URL="sqlite:///${DB_PATH}"
export STORAGE_ROOT
export HTTP_HOST=127.0.0.1
export HTTP_PORT="${AM_E2E_UNUSED_PORT:-8799}"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"reservation-read-parity","version":"1"}}}'

stdio_session() {
    local output="$1" expected_id="$2"
    shift 2
    local session_dir fifo server_pid writer_fd request response_seen
    session_dir="$(mktemp -d "${WORK}/stdio.XXXXXX")"
    fifo="${session_dir}/stdin"
    mkfifo "${fifo}"
    DATABASE_URL="${DATABASE_URL}" STORAGE_ROOT="${STORAGE_ROOT}" RUST_LOG=error \
        "${AM_BIN}" serve-stdio <"${fifo}" >"${output}" 2>"${output}.err" &
    server_pid=$!
    exec {writer_fd}>"${fifo}"
    printf '%s\n' "${INIT_REQ}" >&"${writer_fd}"
    for request in "$@"; do
        printf '%s\n' "${request}" >&"${writer_fd}"
    done
    exec {writer_fd}>&-

    response_seen=false
    for _ in {1..200}; do
        if jq -e --argjson id "${expected_id}" 'select(.id == $id)' "${output}" >/dev/null 2>&1; then
            response_seen=true
            break
        fi
        if ! kill -0 "${server_pid}" 2>/dev/null; then
            break
        fi
        sleep 0.05
    done
    if [[ "${response_seen}" != true ]]; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
        e2e_log "stdio response id ${expected_id} was not observed; stderr=$(head -c 320 "${output}.err")"
        return 1
    fi
    for _ in {1..100}; do
        if ! kill -0 "${server_pid}" 2>/dev/null; then
            break
        fi
        sleep 0.02
    done
    kill "${server_pid}" 2>/dev/null || true
    wait "${server_pid}" 2>/dev/null || true
}

tool_result() {
    local file="$1" request_id="$2"
    jq -r --argjson id "${request_id}" \
        'select(.id == $id) | .result.content[0].text // empty' "${file}" | head -1
}

amrun() {
    local output="$1"
    shift
    "${AM_BIN}" "$@" >"${output}" 2>"${output}.err"
}

json_assert() {
    local label="$1" file="$2" filter="$3"
    if jq -e "${filter}" "${file}" >/dev/null 2>&1; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        e2e_log "filter=${filter}; got=$(jq -c . "${file}" 2>/dev/null | head -c 320)"
    fi
}

e2e_case_banner "MCP grant"
stdio_session "${ART}/grant.jsonl" 12 \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_KEY}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"GoldFox\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/alpha/**\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"read parity\"}}}"

PROJECT_JSON="$(tool_result "${ART}/grant.jsonl" 10)"
GRANT_JSON="$(tool_result "${ART}/grant.jsonl" 12)"
printf '%s\n' "${GRANT_JSON}" >"${ART}/grant.json"
PROJECT_SLUG="$(jq -r '.slug // empty' <<<"${PROJECT_JSON}")"
EXPIRES_TS="$(jq -r '.granted[0].expires_ts // empty' "${ART}/grant.json")"
json_assert "MCP grants one exclusive lease" "${ART}/grant.json" \
    '.granted | any(.path_pattern == "src/alpha/**" and .exclusive == true)'

e2e_case_banner "All read surfaces agree while the lease is active"
amrun "${ART}/alias_path.json" reservations --project "${PROJECT_KEY}" --agent GoldFox --json
amrun "${ART}/robot_slug.json" robot --project "${PROJECT_SLUG}" --agent GoldFox reservations --all --format json
amrun "${ART}/conflict_focus.json" reservations --project "${PROJECT_KEY}" --agent GoldFox --conflicts --json
amrun "${ART}/active_path.txt" file_reservations active "${PROJECT_KEY}"
amrun "${ART}/active_slug.txt" file_reservations active "${PROJECT_SLUG}"
amrun "${ART}/list_path.txt" file_reservations list "${PROJECT_KEY}"
amrun "${ART}/list_slug.txt" file_reservations list "${PROJECT_SLUG}"

for view in alias_path robot_slug conflict_focus; do
    json_assert "${view}: authoritative holder/path visible" "${ART}/${view}.json" \
        '.all_active | any(.agent == "GoldFox" and .path == "src/alpha/**" and .exclusive == true)'
    json_assert "${view}: live expiry visible" "${ART}/${view}.json" \
        '.all_active | any(.path == "src/alpha/**" and .remaining_seconds > 0 and .remaining_seconds <= 3600)'
done
json_assert "conflict focus has a distinct empty projection" "${ART}/conflict_focus.json" \
    '(.conflicts == null or (.conflicts | length == 0)) and (.conflicting_active | length == 0)'
json_assert "conflict focus explains no-overlap vs no-active" "${ART}/conflict_focus.json" \
    '._alerts | any(.summary == "No overlapping active reservation pairs found" and .action == "`all_active` is the authoritative lease snapshot within the selected project/agent scope; use `am file_reservations conflicts <PROJECT> <PATHS>...` to check proposed edits.")'

for view in active_path active_slug list_path list_slug; do
    if grep -Fq "GoldFox" "${ART}/${view}.txt" && grep -Fq "src/alpha/**" "${ART}/${view}.txt"; then
        e2e_pass "${view}: authoritative holder/path visible"
    else
        e2e_fail "${view}: authoritative holder/path missing"
    fi
done
if [ -n "${EXPIRES_TS}" ] && grep -Fq "${EXPIRES_TS:0:19}" "${ART}/list_path.txt"; then
    e2e_pass "operator expiry matches MCP grant"
else
    e2e_fail "operator expiry does not match MCP grant"
fi

e2e_case_banner "MCP release disappears from every read surface"
stdio_session "${ART}/release.jsonl" 20 \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/alpha/**\"]}}}"
printf '%s\n' "$(tool_result "${ART}/release.jsonl" 20)" >"${ART}/release.json"
json_assert "MCP releases exactly one lease" "${ART}/release.json" '.released == 1'

amrun "${ART}/alias_after.json" reservations --project "${PROJECT_SLUG}" --agent GoldFox --json
amrun "${ART}/conflict_after.json" reservations --project "${PROJECT_KEY}" --agent GoldFox --conflicts --json
amrun "${ART}/active_after.txt" file_reservations active "${PROJECT_SLUG}"
amrun "${ART}/list_after.txt" file_reservations list "${PROJECT_KEY}"
for view in alias_after conflict_after; do
    json_assert "${view}: released lease absent" "${ART}/${view}.json" \
        '.all_active | all(.agent != "GoldFox" or .path != "src/alpha/**")'
done
for view in active_after list_after; do
    if grep -Fq "src/alpha/**" "${ART}/${view}.txt"; then
        e2e_fail "${view}: released lease still visible"
    else
        e2e_pass "${view}: released lease absent"
    fi
done

e2e_summary
