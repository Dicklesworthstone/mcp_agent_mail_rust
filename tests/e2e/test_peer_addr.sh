#!/usr/bin/env bash
# test_peer_addr.sh - E2E test suite for peer_addr + localhost bypass behavior
#
# Verifies:
# - Local (loopback) peer addr bypasses HTTP bearer auth when
#   HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED is enabled.
# - Forwarded headers disable the localhost bypass.
#
# Artifacts:
# - Server logs: tests/artifacts/peer_addr/<timestamp>/server.log
# - Per-case HTTP transcripts: status/headers/body + curl stderr

E2E_SUITE="peer_addr"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Peer Addr / Localhost Bypass E2E Test Suite"

if ! command -v curl >/dev/null 2>&1; then
    e2e_log "curl not found; skipping suite"
    e2e_skip "curl required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

# ---------------------------------------------------------------------------
# Setup: temp workspace + server
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_peer_addr")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
TOKEN="e2e-token"

PORT="$(
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"

# e2e_ensure_binary is verbose (logs to stdout); take the last line as the path.
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"
SERVER_LOG="${E2E_ARTIFACT_DIR}/server.log"

e2e_log "Starting server:"
e2e_log "  bin:   ${BIN}"
e2e_log "  host:  127.0.0.1"
e2e_log "  port:  ${PORT}"

(
    export DATABASE_URL="sqlite:////${DB_PATH}"
    export STORAGE_ROOT="${STORAGE_ROOT}"
    export HTTP_BEARER_TOKEN="${TOKEN}"
    export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED="1"
    export HTTP_RBAC_ENABLED="0"
    export HTTP_RATE_LIMIT_ENABLED="0"
    "${BIN}" serve --host 127.0.0.1 --port "${PORT}"
) >"${SERVER_LOG}" 2>&1 &
SERVER_PID=$!

cleanup_server() {
    if kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" 2>/dev/null || true
        # Give it a moment to exit cleanly.
        sleep 0.2
        kill -9 "${SERVER_PID}" 2>/dev/null || true
    fi
}
trap cleanup_server EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT}" 10; then
    e2e_fail "server failed to start (port not open)"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi

URL="http://127.0.0.1:${PORT}/api/"
PAYLOAD='{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"health_check","arguments":{}}}'

http_post() {
    local case_id="$1"
    shift

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local request_file="${E2E_ARTIFACT_DIR}/${case_id}_request.json"
    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.json"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local timing_file="${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call_raw "${case_id}" "${URL}" "${PAYLOAD}" "$@"; then
        :
    fi

    cp "${case_dir}/request.json" "${request_file}" 2>/dev/null || e2e_save_artifact "${case_id}_request.json" "${PAYLOAD}"
    cp "${case_dir}/headers.txt" "${headers_file}" 2>/dev/null || true
    cp "${case_dir}/response.json" "${body_file}" 2>/dev/null || true
    cp "${case_dir}/status.txt" "${status_file}" 2>/dev/null || true
    cp "${case_dir}/timing.txt" "${timing_file}" 2>/dev/null || true
    cp "${case_dir}/curl_stderr.txt" "${curl_stderr_file}" 2>/dev/null || true

    local status
    status="$(cat "${status_file}" 2>/dev/null || echo "")"
    if [ -z "${status}" ] || [ "${status}" = "000" ]; then
        e2e_fail "${case_id}: curl failed (status=${status:-missing})"
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# Case 1: Local bypass allows missing Authorization (no forwarded headers)
# ---------------------------------------------------------------------------
e2e_case_banner "Local bypass allows missing Authorization"

http_post "case1_local_no_auth"
STATUS1="$(cat "${E2E_ARTIFACT_DIR}/case1_local_no_auth_status.txt")"
BODY1="$(cat "${E2E_ARTIFACT_DIR}/case1_local_no_auth_body.json" 2>/dev/null || true)"

e2e_assert_eq "HTTP 200" "200" "${STATUS1}"
e2e_assert_contains "response contains JSON-RPC result" "${BODY1}" "\"result\""

# ---------------------------------------------------------------------------
# Case 2: Forwarded headers disable bypass (missing auth => 401)
# ---------------------------------------------------------------------------
e2e_case_banner "Forwarded header disables bypass (missing auth => 401)"

http_post "case2_forwarded_missing_auth" "X-Forwarded-For: 1.2.3.4"
STATUS2="$(cat "${E2E_ARTIFACT_DIR}/case2_forwarded_missing_auth_status.txt")"
BODY2="$(cat "${E2E_ARTIFACT_DIR}/case2_forwarded_missing_auth_body.json" 2>/dev/null || true)"

e2e_assert_eq "HTTP 401" "401" "${STATUS2}"
e2e_assert_contains "detail is Unauthorized" "${BODY2}" "Unauthorized"

# ---------------------------------------------------------------------------
# Case 3: Forwarded header + correct Authorization succeeds
# ---------------------------------------------------------------------------
e2e_case_banner "Forwarded header + correct Authorization succeeds"

http_post "case3_forwarded_with_auth" \
    "X-Forwarded-For: 1.2.3.4" \
    "Authorization: Bearer ${TOKEN}"
STATUS3="$(cat "${E2E_ARTIFACT_DIR}/case3_forwarded_with_auth_status.txt")"
BODY3="$(cat "${E2E_ARTIFACT_DIR}/case3_forwarded_with_auth_body.json" 2>/dev/null || true)"

e2e_assert_eq "HTTP 200" "200" "${STATUS3}"
e2e_assert_contains "response contains JSON-RPC result" "${BODY3}" "\"result\""

# ---------------------------------------------------------------------------
# Case 4: Forwarded header + wrong Authorization fails
# ---------------------------------------------------------------------------
e2e_case_banner "Forwarded header + wrong Authorization fails"

http_post "case4_forwarded_wrong_auth" \
    "X-Forwarded-For: 1.2.3.4" \
    "Authorization: Bearer wrong"
STATUS4="$(cat "${E2E_ARTIFACT_DIR}/case4_forwarded_wrong_auth_status.txt")"
BODY4="$(cat "${E2E_ARTIFACT_DIR}/case4_forwarded_wrong_auth_body.json" 2>/dev/null || true)"

e2e_assert_eq "HTTP 401" "401" "${STATUS4}"
e2e_assert_contains "detail is Unauthorized" "${BODY4}" "Unauthorized"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
