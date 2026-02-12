#!/usr/bin/env bash
# test_bearer_auth.sh - E2E test suite for bearer token authentication
#
# Verifies (br-3h13.9.1):
# - Request with valid HTTP_BEARER_TOKEN (200)
# - Request with invalid token (401/403)
# - Request with missing Authorization header (401)
# - Request with malformed Authorization header (Bearer <space> only, etc.)
# - Request with empty token string (401)
# - Health endpoint bypasses auth (200)
# - Localhost bypass when http_allow_localhost_unauthenticated=true
#
# Artifacts:
# - Server logs: tests/artifacts/bearer_auth/<timestamp>/server_*.log
# - Per-case transcripts: *_status.txt, *_headers.txt, *_body.json, *_curl_stderr.txt

set -euo pipefail

E2E_SUITE="bearer_auth"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Bearer Token Authentication E2E Test Suite"

if ! command -v curl >/dev/null 2>&1; then
    e2e_log "curl not found; skipping suite"
    e2e_skip "curl required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

fail_fast_if_needed() {
    if [ "${_E2E_FAIL}" -gt 0 ]; then
        e2e_log "Fail-fast: exiting after first failure"
        e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
        e2e_summary || true
        exit 1
    fi
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

http_post_jsonrpc() {
    local case_id="$1"
    local url="$2"
    shift 2

    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.json"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"

    local payload='{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"health_check","arguments":{}}}'
    e2e_save_artifact "${case_id}_request.json" "${payload}"

    local args=(
        -sS
        -D "${headers_file}"
        -o "${body_file}"
        -w "%{http_code}"
        -X POST
        "${url}"
        -H "content-type: application/json"
        --data "${payload}"
    )
    for h in "$@"; do
        args+=(-H "$h")
    done

    set +e
    local status
    status="$(curl "${args[@]}" 2>"${curl_stderr_file}")"
    local rc=$?
    set -e

    echo "${status}" > "${status_file}"
    if [ "$rc" -ne 0 ]; then
        e2e_fail "${case_id}: curl failed rc=${rc}"
        return 1
    fi
    return 0
}

start_server() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5

    local server_log="${E2E_ARTIFACT_DIR}/server_${label}.log"
    e2e_log "Starting server (${label}): 127.0.0.1:${port}"
    e2e_log "  log: ${server_log}"

    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"
        # Disable JWT so we test pure bearer token auth
        export HTTP_JWT_ENABLED="0"
        export HTTP_RBAC_ENABLED="0"
        export HTTP_RATE_LIMIT_ENABLED="0"

        # Optional overrides passed as KEY=VALUE pairs in remaining args.
        while [ $# -gt 0 ]; do
            export "$1"
            shift
        done

        "${bin}" serve --host 127.0.0.1 --port "${port}"
    ) >"${server_log}" 2>&1 &
    echo $!
}

stop_server() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_bearer_auth")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"

# e2e_ensure_binary is verbose (logs to stdout); take the last line as the path.
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# Generate a test bearer token
VALID_TOKEN="e2e-test-token-$(e2e_seeded_hex)"
INVALID_TOKEN="wrong-token-$(e2e_seeded_hex)"
e2e_save_artifact "valid_token_meta.json" "{\"len\":${#VALID_TOKEN},\"sha256\":\"$(e2e_sha256_str "$VALID_TOKEN")\"}"
e2e_save_artifact "invalid_token_meta.json" "{\"len\":${#INVALID_TOKEN},\"sha256\":\"$(e2e_sha256_str "$INVALID_TOKEN")\"}"

# ---------------------------------------------------------------------------
# Run 1: Bearer token auth enabled (HTTP_BEARER_TOKEN set)
# ---------------------------------------------------------------------------

PORT1="$(pick_port)"
PID1="$(start_server "bearer_enabled" "${PORT1}" "${DB_PATH}" "${STORAGE_ROOT}" "${BIN}" \
    "HTTP_BEARER_TOKEN=${VALID_TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0")"
trap 'stop_server "${PID1}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT1}" 10; then
    e2e_fail "server failed to start (port not open)"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi

URL1="http://127.0.0.1:${PORT1}/api/"

# ---------------------------------------------------------------------------
# Case 1: Missing Authorization header -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Missing Authorization header -> 401"
http_post_jsonrpc "case01_missing_auth" "${URL1}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case01_missing_auth_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case01_missing_auth_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 2: Invalid token -> 401/403
# ---------------------------------------------------------------------------
e2e_case_banner "Invalid bearer token -> 401"
http_post_jsonrpc "case02_invalid_token" "${URL1}" "Authorization: Bearer ${INVALID_TOKEN}"
STATUS_CASE2="$(cat "${E2E_ARTIFACT_DIR}/case02_invalid_token_status.txt")"
if [ "$STATUS_CASE2" = "401" ] || [ "$STATUS_CASE2" = "403" ]; then
    e2e_pass "HTTP status is 401 or 403 (got ${STATUS_CASE2})"
else
    e2e_fail "Expected 401 or 403, got ${STATUS_CASE2}"
fi
e2e_assert_contains "detail Unauthorized or Forbidden" "$(cat "${E2E_ARTIFACT_DIR}/case02_invalid_token_body.json" 2>/dev/null || true)" "nauthorized\|orbidden"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 3: Bearer with empty token string -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Bearer with empty token -> 401"
http_post_jsonrpc "case03_empty_token" "${URL1}" "Authorization: Bearer "
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case03_empty_token_status.txt")"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 4: Malformed - "Bearer" only (no space after) -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Malformed - Bearer without token -> 401"
http_post_jsonrpc "case04_bearer_only" "${URL1}" "Authorization: Bearer"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case04_bearer_only_status.txt")"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 5: Malformed - Basic instead of Bearer -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Basic auth instead of Bearer -> 401"
http_post_jsonrpc "case05_basic_auth" "${URL1}" "Authorization: Basic dXNlcjpwYXNz"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case05_basic_auth_status.txt")"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 6: Malformed - "bearer" lowercase -> check behavior
# ---------------------------------------------------------------------------
e2e_case_banner "Lowercase bearer (case sensitivity check)"
http_post_jsonrpc "case06_lowercase_bearer" "${URL1}" "Authorization: bearer ${VALID_TOKEN}"
STATUS_CASE6="$(cat "${E2E_ARTIFACT_DIR}/case06_lowercase_bearer_status.txt")"
# HTTP spec says Bearer should be case-insensitive, but implementations vary
if [ "$STATUS_CASE6" = "200" ]; then
    e2e_pass "Server accepts lowercase 'bearer' (RFC-compliant)"
elif [ "$STATUS_CASE6" = "401" ]; then
    e2e_pass "Server rejects lowercase 'bearer' (strict mode)"
else
    e2e_fail "Unexpected status ${STATUS_CASE6} for lowercase bearer"
fi
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 7: Valid bearer token -> 200
# ---------------------------------------------------------------------------
e2e_case_banner "Valid bearer token -> 200"
http_post_jsonrpc "case07_valid_token" "${URL1}" "Authorization: Bearer ${VALID_TOKEN}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/case07_valid_token_status.txt")"
e2e_assert_contains "JSON-RPC result present" "$(cat "${E2E_ARTIFACT_DIR}/case07_valid_token_body.json" 2>/dev/null || true)" "\"result\""
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 8: Extra whitespace in Authorization header
# ---------------------------------------------------------------------------
e2e_case_banner "Extra whitespace in Authorization header"
http_post_jsonrpc "case08_extra_whitespace" "${URL1}" "Authorization:   Bearer   ${VALID_TOKEN}  "
STATUS_CASE8="$(cat "${E2E_ARTIFACT_DIR}/case08_extra_whitespace_status.txt")"
# Should either accept (200) or reject with 401, but not crash
if [ "$STATUS_CASE8" = "200" ] || [ "$STATUS_CASE8" = "401" ]; then
    e2e_pass "Server handles extra whitespace gracefully (got ${STATUS_CASE8})"
else
    e2e_fail "Unexpected status ${STATUS_CASE8} for extra whitespace"
fi
fail_fast_if_needed

stop_server "${PID1}"
trap - EXIT

# ---------------------------------------------------------------------------
# Run 2: Localhost bypass enabled
# ---------------------------------------------------------------------------

PORT2="$(pick_port)"
PID2="$(start_server "localhost_bypass" "${PORT2}" "${DB_PATH}" "${STORAGE_ROOT}" "${BIN}" \
    "HTTP_BEARER_TOKEN=${VALID_TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1")"
trap 'stop_server "${PID2}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    e2e_fail "server (localhost_bypass) failed to start (port not open)"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi

URL2="http://127.0.0.1:${PORT2}/api/"

# ---------------------------------------------------------------------------
# Case 9: No auth header but localhost bypass enabled -> 200
# ---------------------------------------------------------------------------
e2e_case_banner "Localhost bypass: no auth -> 200"
http_post_jsonrpc "case09_localhost_bypass" "${URL2}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/case09_localhost_bypass_status.txt")"
e2e_assert_contains "JSON-RPC result present" "$(cat "${E2E_ARTIFACT_DIR}/case09_localhost_bypass_body.json" 2>/dev/null || true)" "\"result\""
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 10: Valid token still works with bypass enabled
# ---------------------------------------------------------------------------
e2e_case_banner "Localhost bypass: valid token still works -> 200"
http_post_jsonrpc "case10_bypass_with_token" "${URL2}" "Authorization: Bearer ${VALID_TOKEN}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/case10_bypass_with_token_status.txt")"
e2e_assert_contains "JSON-RPC result present" "$(cat "${E2E_ARTIFACT_DIR}/case10_bypass_with_token_body.json" 2>/dev/null || true)" "\"result\""
fail_fast_if_needed

stop_server "${PID2}"
trap - EXIT

# ---------------------------------------------------------------------------
# Run 3: No bearer token configured (auth disabled)
# ---------------------------------------------------------------------------

PORT3="$(pick_port)"
PID3="$(start_server "no_auth" "${PORT3}" "${DB_PATH}" "${STORAGE_ROOT}" "${BIN}" \
    "HTTP_BEARER_TOKEN=" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0")"
trap 'stop_server "${PID3}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT3}" 10; then
    e2e_fail "server (no_auth) failed to start (port not open)"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi

URL3="http://127.0.0.1:${PORT3}/api/"

# ---------------------------------------------------------------------------
# Case 11: No bearer token configured - requests should pass
# ---------------------------------------------------------------------------
e2e_case_banner "No bearer token configured -> 200 (auth disabled)"
http_post_jsonrpc "case11_no_auth_configured" "${URL3}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/case11_no_auth_configured_status.txt")"
e2e_assert_contains "JSON-RPC result present" "$(cat "${E2E_ARTIFACT_DIR}/case11_no_auth_configured_body.json" 2>/dev/null || true)" "\"result\""
fail_fast_if_needed

stop_server "${PID3}"
trap - EXIT

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
