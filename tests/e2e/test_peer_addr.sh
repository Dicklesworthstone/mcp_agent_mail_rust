#!/usr/bin/env bash
# test_peer_addr.sh - E2E test suite for peer_addr + localhost bypass behavior
#
# Verifies:
# - Local (loopback) peer addr bypasses HTTP bearer auth when
#   HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED is enabled.
# - Forwarded headers disable the localhost bypass.
#
# Artifacts:
# - Server logs: tests/artifacts/peer_addr/<timestamp>/logs/server_peer_addr.log
# - Per-case case directories: <case_id>/{request,response,headers,status,timing}.*

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

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "peer_addr" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0"; then
    e2e_fail "server failed to start"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# e2e_start_server_with_logs sets /mcp/ by default; peer_addr suite targets /api/.
E2E_SERVER_URL="${E2E_SERVER_URL%/mcp/}/api/"
PAYLOAD='{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"health_check","arguments":{}}}'

http_post() {
    local case_id="$1"
    shift

    if ! e2e_rpc_call_raw "${case_id}" "${E2E_SERVER_URL}" "${PAYLOAD}" "$@"; then
        :
    fi

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
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
STATUS1="$(e2e_rpc_read_status "case1_local_no_auth")"
BODY1="$(e2e_rpc_read_response "case1_local_no_auth")"

e2e_assert_eq "HTTP 200" "200" "${STATUS1}"
e2e_assert_contains "response contains JSON-RPC result" "${BODY1}" "\"result\""

# ---------------------------------------------------------------------------
# Case 2: Forwarded headers disable bypass (missing auth => 401)
# ---------------------------------------------------------------------------
e2e_case_banner "Forwarded header disables bypass (missing auth => 401)"

http_post "case2_forwarded_missing_auth" "X-Forwarded-For: 1.2.3.4"
STATUS2="$(e2e_rpc_read_status "case2_forwarded_missing_auth")"
BODY2="$(e2e_rpc_read_response "case2_forwarded_missing_auth")"

e2e_assert_eq "HTTP 401" "401" "${STATUS2}"
e2e_assert_contains "detail is Unauthorized" "${BODY2}" "Unauthorized"

# ---------------------------------------------------------------------------
# Case 3: Forwarded header + correct Authorization succeeds
# ---------------------------------------------------------------------------
e2e_case_banner "Forwarded header + correct Authorization succeeds"

http_post "case3_forwarded_with_auth" \
    "X-Forwarded-For: 1.2.3.4" \
    "Authorization: Bearer ${TOKEN}"
STATUS3="$(e2e_rpc_read_status "case3_forwarded_with_auth")"
BODY3="$(e2e_rpc_read_response "case3_forwarded_with_auth")"

e2e_assert_eq "HTTP 200" "200" "${STATUS3}"
e2e_assert_contains "response contains JSON-RPC result" "${BODY3}" "\"result\""

# ---------------------------------------------------------------------------
# Case 4: Forwarded header + wrong Authorization fails
# ---------------------------------------------------------------------------
e2e_case_banner "Forwarded header + wrong Authorization fails"

http_post "case4_forwarded_wrong_auth" \
    "X-Forwarded-For: 1.2.3.4" \
    "Authorization: Bearer wrong"
STATUS4="$(e2e_rpc_read_status "case4_forwarded_wrong_auth")"
BODY4="$(e2e_rpc_read_response "case4_forwarded_wrong_auth")"

e2e_assert_eq "HTTP 401" "401" "${STATUS4}"
e2e_assert_contains "detail is Unauthorized" "${BODY4}" "Unauthorized"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
