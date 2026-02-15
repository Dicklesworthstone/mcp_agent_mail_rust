#!/usr/bin/env bash
# e2e_tui_wasm.sh - WASM/browser-mode E2E suite for TUI state-sync contract.
#
# Run via:
#   ./scripts/e2e_test.sh tui_wasm
#
# Coverage:
# - WASM crate native unit-test execution via rch
# - wasm32 target build verification via rch
# - Browser fallback transport contract (/mail/ws-state + /mail/ws-input)
# - Degraded + recovery paths (invalid ingress payload + subsequent recovery)
# - WebSocket upgrade denial behavior on polling endpoint

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_wasm}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI WASM (Browser-Mode) E2E Test Suite"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in python3 curl timeout script; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

SCENARIO_DIAG_FILE="${E2E_ARTIFACT_DIR}/diagnostics/wasm_scenarios.jsonl"
mkdir -p "$(dirname "${SCENARIO_DIAG_FILE}")"
: > "${SCENARIO_DIAG_FILE}"

_SCENARIO_DIAG_ID=""
_SCENARIO_DIAG_START_MS=0
_SCENARIO_DIAG_FAIL_BASE=0
_SCENARIO_DIAG_SKIP_BASE=0
_SCENARIO_DIAG_REASON_CODE="OK"
_SCENARIO_DIAG_REASON="completed"

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
    _SCENARIO_DIAG_SKIP_BASE="${_E2E_SKIP}"
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
    local artifact_path="$1"
    local elapsed_ms fail_delta skip_delta status reason_code reason repro_cmd

    elapsed_ms=$(( $(_e2e_now_ms) - _SCENARIO_DIAG_START_MS ))
    fail_delta=$(( _E2E_FAIL - _SCENARIO_DIAG_FAIL_BASE ))
    skip_delta=$(( _E2E_SKIP - _SCENARIO_DIAG_SKIP_BASE ))

    status="pass"
    reason_code="${_SCENARIO_DIAG_REASON_CODE}"
    reason="${_SCENARIO_DIAG_REASON}"

    if [ "${fail_delta}" -gt 0 ]; then
        status="fail"
        if [ "${reason_code}" = "OK" ]; then
            reason_code="ASSERTION_FAILURE"
            reason="${fail_delta} assertion(s) failed"
        fi
    elif [ "${skip_delta}" -gt 0 ]; then
        status="skip"
        if [ "${reason_code}" = "OK" ]; then
            reason_code="SKIPPED"
            reason="${skip_delta} skip(s) recorded"
        fi
    fi

    repro_cmd="$(e2e_repro_command | tr -d '\n')"
    {
        printf '{'
        printf '"schema_version":1,'
        printf '"suite":"%s",' "$(_e2e_json_escape "$E2E_SUITE")"
        printf '"scenario_id":"%s",' "$(_e2e_json_escape "$_SCENARIO_DIAG_ID")"
        printf '"status":"%s",' "$(_e2e_json_escape "$status")"
        printf '"elapsed_ms":%s,' "$elapsed_ms"
        printf '"reason_code":"%s",' "$(_e2e_json_escape "$reason_code")"
        printf '"reason":"%s",' "$(_e2e_json_escape "$reason")"
        printf '"artifact_path":"%s",' "$(_e2e_json_escape "$(diag_rel_path "$artifact_path")")"
        printf '"repro_command":"%s"' "$(_e2e_json_escape "$repro_cmd")"
        printf '}\n'
    } >> "${SCENARIO_DIAG_FILE}"
}

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

run_cargo_with_rch_only() {
    local out_file="$1"
    shift
    local -a cargo_args=("$@")

    {
        echo "[cmd] cargo ${cargo_args[*]}"
        echo "[runner] rch"
    } >> "${out_file}"

    if ! command -v rch >/dev/null 2>&1; then
        echo "[error] rch is required but was not found in PATH" >> "${out_file}"
        return 127
    fi

    timeout "${E2E_RCH_TIMEOUT_SECONDS:-420}" \
        rch exec -- cargo "${cargo_args[@]}" >> "${out_file}" 2>&1
}

resolve_binary() {
    if [ -n "${MCP_AGENT_MAIL_BIN:-}" ] && [ -x "${MCP_AGENT_MAIL_BIN}" ]; then
        printf '%s\n' "${MCP_AGENT_MAIL_BIN}"
        return
    fi

    if [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "${CARGO_TARGET_DIR}/debug/mcp-agent-mail" ]; then
        printf '%s\n' "${CARGO_TARGET_DIR}/debug/mcp-agent-mail"
        return
    fi

    if [ -x "/data/tmp/cargo-target/debug/mcp-agent-mail" ]; then
        printf '%s\n' "/data/tmp/cargo-target/debug/mcp-agent-mail"
        return
    fi

    if command -v mcp-agent-mail >/dev/null 2>&1; then
        command -v mcp-agent-mail
        return
    fi

    e2e_ensure_binary "mcp-agent-mail" | tail -n 1
}

SERVER_PID=""
stop_server() {
    local pid="$1"
    if [ -n "${pid}" ] && kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

cleanup_server() {
    if [ -n "${SERVER_PID}" ]; then
        stop_server "${SERVER_PID}"
        SERVER_PID=""
    fi
}

trap cleanup_server EXIT

start_tui_server() {
    local label="$1"
    local bin="$2"
    local port="$3"
    local db_path="$4"
    local storage_root="$5"
    local log_path="${E2E_ARTIFACT_DIR}/server_${label}.typescript"

    (
        script -q -f -c "env \
DATABASE_URL=sqlite:////${db_path} \
STORAGE_ROOT=${storage_root} \
HTTP_HOST=127.0.0.1 \
HTTP_PORT=${port} \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_JWT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
LINES=40 \
COLUMNS=120 \
timeout ${E2E_SERVER_TIMEOUT_SECONDS:-180}s ${bin} serve --host 127.0.0.1 --port ${port}" \
            "${log_path}"
    ) >/dev/null 2>&1 &

    echo $!
}

HTTP_LAST_CASE_DIR=""
HTTP_LAST_STATUS=""
HTTP_LAST_RESPONSE_FILE=""

http_call_json() {
    local case_id="$1"
    local method="$2"
    local url="$3"
    local payload="${4:-}"
    shift 4 || true

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "${case_dir}"

    local request_file="${case_dir}/request.json"
    local response_file="${case_dir}/response.json"
    local headers_file="${case_dir}/headers.txt"
    local timing_file="${case_dir}/timing.txt"
    local status_file="${case_dir}/status.txt"
    local curl_stderr_file="${case_dir}/curl_stderr.txt"
    local repro_file="${case_dir}/repro_command.sh"

    printf '{"method":"%s","url":"%s","body":"%s"}\n' \
        "${method}" \
        "${url}" \
        "$(_e2e_json_escape "${payload}")" \
        > "${request_file}"
    printf '%s\n' "$(e2e_repro_command)" > "${repro_file}"

    local -a curl_args=(
        -sS
        -X "${method}"
        "${url}"
        -H "Accept: application/json"
        -D "${headers_file}"
        -o "${response_file}"
        -w '%{http_code} %{time_total}'
    )

    if [ -n "${payload}" ]; then
        curl_args+=( -H "Content-Type: application/json" --data "${payload}" )
    fi

    while [ "$#" -gt 0 ]; do
        curl_args+=( -H "$1" )
        shift
    done

    local timing_line curl_rc status_code
    set +e
    timing_line="$(curl "${curl_args[@]}" 2>"${curl_stderr_file}")"
    curl_rc=$?
    set -e

    printf '%s\n' "${timing_line}" > "${timing_file}"
    status_code="${timing_line%% *}"
    printf '%s\n' "${status_code}" > "${status_file}"

    HTTP_LAST_CASE_DIR="${case_dir}"
    HTTP_LAST_STATUS="${status_code}"
    HTTP_LAST_RESPONSE_FILE="${response_file}"

    if [ "${curl_rc}" -ne 0 ]; then
        e2e_fail "${case_id}: curl failed (rc=${curl_rc})"
        return 1
    fi

    return 0
}

BIN="$(resolve_binary)"
e2e_log "using mcp-agent-mail binary: ${BIN}"

# ═══════════════════════════════════════════════════════════════════════
# Case 1: WASM crate native unit tests (via rch)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "wasm_native_unit_tests"
scenario_diag_begin "wasm_native_unit_tests"
CASE1_LOG="${E2E_ARTIFACT_DIR}/case_01_wasm_native_unit_tests.log"

set +e
run_cargo_with_rch_only "${CASE1_LOG}" test -p mcp-agent-mail-wasm --lib -- --nocapture
CASE1_RC=$?
set -e
CASE1_OUT="$(cat "${CASE1_LOG}" 2>/dev/null || true)"

if [ "${CASE1_RC}" -eq 0 ]; then
    e2e_pass "wasm native unit tests exit 0 via rch"
    e2e_assert_file_exists "wasm unit-test log exists" "${CASE1_LOG}"
elif [ "${CASE1_RC}" -eq 127 ]; then
    scenario_diag_mark_reason "RCH_UNAVAILABLE" "rch not available in PATH"
    e2e_skip "wasm native unit tests skipped: rch unavailable"
elif printf '%s' "${CASE1_OUT}" | grep -Fq "failed to select a version for the requirement \`ftui = \"^0.2.0\"\`"; then
    scenario_diag_mark_reason "RCH_REMOTE_DEP_MISMATCH" "remote worker dependency mismatch (ftui 0.2.0)"
    e2e_skip "wasm native unit tests skipped: remote rch dependency mismatch (ftui 0.2.0)"
else
    scenario_diag_mark_reason "WASM_UNIT_TEST_FAILED" "cargo test failed"
    e2e_fail "wasm native unit tests failed (rc=${CASE1_RC})"
fi
scenario_diag_finish "${CASE1_LOG}"

# ═══════════════════════════════════════════════════════════════════════
# Case 2: wasm32 target build (via rch)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "wasm_target_build"
scenario_diag_begin "wasm_target_build"
CASE2_LOG="${E2E_ARTIFACT_DIR}/case_02_wasm_target_build.log"

set +e
run_cargo_with_rch_only "${CASE2_LOG}" build -p mcp-agent-mail-wasm --target wasm32-unknown-unknown --release
CASE2_RC=$?
set -e

CASE2_OUT="$(cat "${CASE2_LOG}" 2>/dev/null || true)"
if [ "${CASE2_RC}" -eq 0 ]; then
    e2e_pass "wasm32 target build exits 0 via rch"
elif [ "${CASE2_RC}" -eq 127 ]; then
    scenario_diag_mark_reason "RCH_UNAVAILABLE" "rch not available in PATH"
    e2e_skip "wasm32 build skipped: rch unavailable"
elif printf '%s' "${CASE2_OUT}" | grep -Fq "failed to select a version for the requirement \`ftui = \"^0.2.0\"\`"; then
    scenario_diag_mark_reason "RCH_REMOTE_DEP_MISMATCH" "remote worker dependency mismatch (ftui 0.2.0)"
    e2e_skip "wasm32 build skipped: remote rch dependency mismatch (ftui 0.2.0)"
elif printf '%s' "${CASE2_OUT}" | grep -Eq 'wasm32-unknown-unknown.*(not installed|not found)|can.t find crate for .*core.*|target may not be installed'; then
    scenario_diag_mark_reason "WASM_TARGET_UNAVAILABLE" "remote worker missing wasm32 target"
    e2e_skip "wasm32 build skipped: target unavailable on remote worker"
else
    scenario_diag_mark_reason "WASM_TARGET_BUILD_FAILED" "cargo build failed"
    e2e_fail "wasm32 target build failed (rc=${CASE2_RC})"
fi
scenario_diag_finish "${CASE2_LOG}"

# ═══════════════════════════════════════════════════════════════════════
# Start live TUI server for browser-mode contract cases
# ═══════════════════════════════════════════════════════════════════════
WORK="$(e2e_mktemp "e2e_tui_wasm")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage"
mkdir -p "${STORAGE_ROOT}"
PORT="$(pick_port)"
BASE_URL="http://127.0.0.1:${PORT}"

SERVER_PID="$(start_tui_server "wasm_contract" "${BIN}" "${PORT}" "${DB_PATH}" "${STORAGE_ROOT}")"
e2e_log "started server pid=${SERVER_PID} on ${BASE_URL}"

if ! e2e_wait_port "127.0.0.1" "${PORT}" 20; then
    e2e_fail "server failed to bind ${BASE_URL} within timeout"
    scenario_diag_begin "server_startup"
    scenario_diag_mark_reason "SERVER_START_TIMEOUT" "port did not open within 20s"
    scenario_diag_finish "${E2E_ARTIFACT_DIR}/server_wasm_contract.typescript"
    cleanup_server
    e2e_summary
    exit 1
fi

# ═══════════════════════════════════════════════════════════════════════
# Case 3: /mail/ws-state snapshot contract
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "ws_state_snapshot_contract"
scenario_diag_begin "ws_state_snapshot_contract"
CASE3_ID="case_03_ws_state_snapshot"
if http_call_json "${CASE3_ID}" "GET" "${BASE_URL}/mail/ws-state?limit=50" ""; then
    if [ "${HTTP_LAST_STATUS}" = "200" ]; then
        e2e_pass "ws-state snapshot HTTP status 200"
    else
        scenario_diag_mark_reason "WS_STATE_HTTP_STATUS" "unexpected status ${HTTP_LAST_STATUS}"
        e2e_fail "ws-state snapshot status ${HTTP_LAST_STATUS}"
    fi

    CASE3_SEQ="$(python3 - "${HTTP_LAST_RESPONSE_FILE}" "${HTTP_LAST_CASE_DIR}/snapshot_meta.json" <<'PY'
import json
import sys

resp_path = sys.argv[1]
meta_path = sys.argv[2]
with open(resp_path, 'r', encoding='utf-8') as f:
    payload = json.load(f)

required = [
    'schema_version',
    'transport',
    'mode',
    'generated_at_us',
    'request_counters',
    'event_ring_stats',
    'db_stats',
    'sparkline_ms',
    'events',
]
for key in required:
    if key not in payload:
        raise AssertionError(f'missing field: {key}')

if payload['schema_version'] != 'am_ws_state_poll.v1':
    raise AssertionError(f"unexpected schema_version: {payload['schema_version']}")
if payload['transport'] != 'http-poll':
    raise AssertionError(f"unexpected transport: {payload['transport']}")
if payload['mode'] != 'snapshot':
    raise AssertionError(f"unexpected mode for initial poll: {payload['mode']}")

next_seq = int(payload.get('next_seq', 0))
if next_seq < 0:
    raise AssertionError(f'invalid next_seq: {next_seq}')

meta = {
    'schema_version': payload['schema_version'],
    'transport': payload['transport'],
    'mode': payload['mode'],
    'next_seq': next_seq,
    'event_count': int(payload.get('event_count', 0)),
}
with open(meta_path, 'w', encoding='utf-8') as f:
    json.dump(meta, f, indent=2)

print(next_seq)
PY
)"

    if [ -n "${CASE3_SEQ}" ]; then
        e2e_pass "ws-state snapshot payload shape validated"
    else
        scenario_diag_mark_reason "WS_STATE_SNAPSHOT_PARSE_FAILED" "snapshot parser returned empty sequence"
        e2e_fail "ws-state snapshot parser returned empty sequence"
        CASE3_SEQ="0"
    fi
else
    scenario_diag_mark_reason "WS_STATE_SNAPSHOT_CURL_FAILED" "curl request failed"
fi
scenario_diag_finish "${HTTP_LAST_CASE_DIR}"

# ═══════════════════════════════════════════════════════════════════════
# Case 4: /mail/ws-state delta contract with since=seq
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "ws_state_delta_contract"
scenario_diag_begin "ws_state_delta_contract"
CASE4_ID="case_04_ws_state_delta"
if http_call_json "${CASE4_ID}" "GET" "${BASE_URL}/mail/ws-state?since=${CASE3_SEQ:-0}&limit=25" ""; then
    if [ "${HTTP_LAST_STATUS}" = "200" ]; then
        e2e_pass "ws-state delta HTTP status 200"
    else
        scenario_diag_mark_reason "WS_STATE_DELTA_HTTP_STATUS" "unexpected status ${HTTP_LAST_STATUS}"
        e2e_fail "ws-state delta status ${HTTP_LAST_STATUS}"
    fi

    if python3 - "${HTTP_LAST_RESPONSE_FILE}" "${CASE3_SEQ:-0}" "${HTTP_LAST_CASE_DIR}/delta_meta.json" <<'PY'
import json
import sys

resp_path = sys.argv[1]
since = int(sys.argv[2])
meta_path = sys.argv[3]

with open(resp_path, 'r', encoding='utf-8') as f:
    payload = json.load(f)

if payload.get('mode') != 'delta':
    raise AssertionError(f"expected delta mode, got {payload.get('mode')}")
if payload.get('transport') != 'http-poll':
    raise AssertionError(f"unexpected transport: {payload.get('transport')}")
if int(payload.get('since_seq', -1)) != since:
    raise AssertionError(f"since_seq mismatch: expected {since}, got {payload.get('since_seq')}")

to_seq = int(payload.get('to_seq', since))
if to_seq < since:
    raise AssertionError(f"to_seq regression: {to_seq} < {since}")

meta = {
    'mode': payload.get('mode'),
    'since_seq': since,
    'to_seq': to_seq,
    'event_count': int(payload.get('event_count', 0)),
}
with open(meta_path, 'w', encoding='utf-8') as f:
    json.dump(meta, f, indent=2)
PY
    then
        e2e_pass "ws-state delta semantics validated"
    else
        scenario_diag_mark_reason "WS_STATE_DELTA_PARSE_FAILED" "delta payload assertions failed"
        e2e_fail "ws-state delta payload assertions failed"
    fi
else
    scenario_diag_mark_reason "WS_STATE_DELTA_CURL_FAILED" "curl request failed"
fi
scenario_diag_finish "${HTTP_LAST_CASE_DIR}"

# ═══════════════════════════════════════════════════════════════════════
# Case 5: /mail/ws-input accepts key+resize (with ignored unsupported input)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "ws_input_accepts_batch"
scenario_diag_begin "ws_input_accepts_batch"
CASE5_ID="case_05_ws_input_batch"
PAYLOAD_BATCH='{"events":[{"type":"Input","data":{"kind":"Key","key":"j","modifiers":0}},{"type":"Input","data":{"kind":"Mouse","x":1,"y":2,"button":1}},{"type":"Resize","data":{"cols":100,"rows":30}}]}'
if http_call_json "${CASE5_ID}" "POST" "${BASE_URL}/mail/ws-input" "${PAYLOAD_BATCH}"; then
    if [ "${HTTP_LAST_STATUS}" = "202" ]; then
        e2e_pass "ws-input batch HTTP status 202"
    else
        scenario_diag_mark_reason "WS_INPUT_BATCH_HTTP_STATUS" "unexpected status ${HTTP_LAST_STATUS}"
        e2e_fail "ws-input batch status ${HTTP_LAST_STATUS}"
    fi

    if python3 - "${HTTP_LAST_RESPONSE_FILE}" <<'PY'
import json
import sys

with open(sys.argv[1], 'r', encoding='utf-8') as f:
    payload = json.load(f)

if payload.get('status') != 'accepted':
    raise AssertionError(f"unexpected status field: {payload.get('status')}")
if int(payload.get('accepted', -1)) != 2:
    raise AssertionError(f"expected accepted=2, got {payload.get('accepted')}")
if int(payload.get('ignored', -1)) != 1:
    raise AssertionError(f"expected ignored=1, got {payload.get('ignored')}")
if int(payload.get('queue_depth', -1)) < 0:
    raise AssertionError(f"unexpected queue_depth: {payload.get('queue_depth')}")
PY
    then
        e2e_pass "ws-input batch payload validated"
    else
        scenario_diag_mark_reason "WS_INPUT_BATCH_PARSE_FAILED" "response assertions failed"
        e2e_fail "ws-input batch response assertions failed"
    fi
else
    scenario_diag_mark_reason "WS_INPUT_BATCH_CURL_FAILED" "curl request failed"
fi
scenario_diag_finish "${HTTP_LAST_CASE_DIR}"

# ═══════════════════════════════════════════════════════════════════════
# Case 6: invalid /mail/ws-input payload returns 400
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "ws_input_invalid_payload"
scenario_diag_begin "ws_input_invalid_payload"
CASE6_ID="case_06_ws_input_invalid"
PAYLOAD_INVALID='{"type":"Input"'
if http_call_json "${CASE6_ID}" "POST" "${BASE_URL}/mail/ws-input" "${PAYLOAD_INVALID}"; then
    if [ "${HTTP_LAST_STATUS}" = "400" ]; then
        e2e_pass "ws-input invalid payload HTTP status 400"
    else
        scenario_diag_mark_reason "WS_INPUT_INVALID_HTTP_STATUS" "unexpected status ${HTTP_LAST_STATUS}"
        e2e_fail "ws-input invalid payload status ${HTTP_LAST_STATUS}"
    fi

    RESP6="$(cat "${HTTP_LAST_RESPONSE_FILE}" 2>/dev/null || true)"
    if printf '%s' "${RESP6}" | grep -Fq 'Invalid /mail/ws-input payload'; then
        e2e_pass "ws-input invalid payload includes parser detail"
    else
        scenario_diag_mark_reason "WS_INPUT_INVALID_DETAIL_MISSING" "expected parser detail missing"
        e2e_fail "ws-input invalid payload detail missing"
    fi
else
    scenario_diag_mark_reason "WS_INPUT_INVALID_CURL_FAILED" "curl request failed"
fi
scenario_diag_finish "${HTTP_LAST_CASE_DIR}"

# ═══════════════════════════════════════════════════════════════════════
# Case 7: recovery after invalid payload (valid payload accepted)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "ws_input_recovery_after_invalid"
scenario_diag_begin "ws_input_recovery_after_invalid"
CASE7_ID="case_07_ws_input_recovery"
PAYLOAD_RECOVERY='{"type":"Input","data":{"kind":"Key","key":"k","modifiers":1}}'
if http_call_json "${CASE7_ID}" "POST" "${BASE_URL}/mail/ws-input" "${PAYLOAD_RECOVERY}"; then
    if [ "${HTTP_LAST_STATUS}" = "202" ]; then
        e2e_pass "ws-input recovery HTTP status 202"
    else
        scenario_diag_mark_reason "WS_INPUT_RECOVERY_HTTP_STATUS" "unexpected status ${HTTP_LAST_STATUS}"
        e2e_fail "ws-input recovery status ${HTTP_LAST_STATUS}"
    fi

    RESP7="$(cat "${HTTP_LAST_RESPONSE_FILE}" 2>/dev/null || true)"
    if printf '%s' "${RESP7}" | grep -Fq '"accepted":1'; then
        e2e_pass "ws-input recovery accepted=1"
    else
        scenario_diag_mark_reason "WS_INPUT_RECOVERY_ACCEPTED_MISSING" "accepted count mismatch"
        e2e_fail "ws-input recovery accepted count mismatch"
    fi
else
    scenario_diag_mark_reason "WS_INPUT_RECOVERY_CURL_FAILED" "curl request failed"
fi
scenario_diag_finish "${HTTP_LAST_CASE_DIR}"

# ═══════════════════════════════════════════════════════════════════════
# Case 8: /mail/ws-state rejects websocket upgrade with 501 guidance
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "ws_state_upgrade_denial"
scenario_diag_begin "ws_state_upgrade_denial"
CASE8_ID="case_08_ws_state_upgrade_denial"
if http_call_json \
    "${CASE8_ID}" \
    "GET" \
    "${BASE_URL}/mail/ws-state" \
    "" \
    "Connection: Upgrade" \
    "Upgrade: websocket" \
    "Sec-WebSocket-Version: 13" \
    "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ=="; then
    if [ "${HTTP_LAST_STATUS}" = "501" ]; then
        e2e_pass "ws-state upgrade denial HTTP status 501"
    else
        scenario_diag_mark_reason "WS_STATE_UPGRADE_HTTP_STATUS" "unexpected status ${HTTP_LAST_STATUS}"
        e2e_fail "ws-state upgrade denial status ${HTTP_LAST_STATUS}"
    fi

    RESP8="$(cat "${HTTP_LAST_RESPONSE_FILE}" 2>/dev/null || true)"
    if printf '%s' "${RESP8}" | grep -Fq 'use HTTP polling'; then
        e2e_pass "ws-state upgrade denial guidance mentions HTTP polling"
    else
        scenario_diag_mark_reason "WS_STATE_UPGRADE_GUIDANCE_MISSING" "missing HTTP polling guidance"
        e2e_fail "ws-state upgrade denial guidance missing"
    fi
else
    scenario_diag_mark_reason "WS_STATE_UPGRADE_CURL_FAILED" "curl request failed"
fi
scenario_diag_finish "${HTTP_LAST_CASE_DIR}"

cleanup_server
trap - EXIT

e2e_summary
