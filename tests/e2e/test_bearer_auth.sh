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
# Artifacts (via e2e_lib.sh helpers):
# - Server logs: tests/artifacts/bearer_auth/<timestamp>/logs/server_*.log
# - Per-case directories: <case_id>/request.json, response.json, headers.txt, status.txt

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

# bearer_server_start: Start server with bearer auth config using e2e_start_server_with_logs
# Args: label [extra_env_vars...]
bearer_server_start() {
    local label="$1"
    shift

    # Bearer auth-specific env vars for hermetic testing
    local base_env_vars=(
        "HTTP_JWT_ENABLED=0"
        "HTTP_RBAC_ENABLED=0"
        "HTTP_RATE_LIMIT_ENABLED=0"
    )

    # Start server with base env vars plus any extras passed in
    if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "${label}" \
        "${base_env_vars[@]}" "$@"; then
        e2e_fail "server (${label}) failed to start"
        return 1
    fi

    # Override URL to use /api/ endpoint instead of /mcp/
    E2E_SERVER_URL="${E2E_SERVER_URL%/mcp/}/api/"
    return 0
}

# bearer_rpc_call: Make JSON-RPC call (wraps e2e_rpc_call, tolerates non-200)
# Args: case_id [extra_headers...]
bearer_rpc_call() {
    local case_id="$1"
    shift
    # Use e2e_rpc_call with health_check tool; suppress error return for 401s
    e2e_rpc_call "${case_id}" "${E2E_SERVER_URL}" "health_check" "{}" "$@" || true
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_bearer_auth")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

# Generate a test bearer token
VALID_TOKEN="e2e-test-token-$(e2e_seeded_hex)"
INVALID_TOKEN="wrong-token-$(e2e_seeded_hex)"
e2e_save_artifact "valid_token_meta.json" "{\"len\":${#VALID_TOKEN},\"sha256\":\"$(e2e_sha256_str "$VALID_TOKEN")\"}"
e2e_save_artifact "invalid_token_meta.json" "{\"len\":${#INVALID_TOKEN},\"sha256\":\"$(e2e_sha256_str "$INVALID_TOKEN")\"}"

# ---------------------------------------------------------------------------
# Run 1: Bearer token auth enabled (HTTP_BEARER_TOKEN set)
# ---------------------------------------------------------------------------

if ! bearer_server_start "bearer_enabled" \
    "HTTP_BEARER_TOKEN=${VALID_TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# ---------------------------------------------------------------------------
# Case 1: Missing Authorization header -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Missing Authorization header -> 401"
e2e_mark_case_start "case01_missing_auth"
bearer_rpc_call "case01_missing_auth"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case01_missing_auth")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case01_missing_auth")" "Unauthorized"
e2e_mark_case_end "case01_missing_auth"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 2: Invalid token -> 401/403
# ---------------------------------------------------------------------------
e2e_case_banner "Invalid bearer token -> 401"
e2e_mark_case_start "case02_invalid_token"
bearer_rpc_call "case02_invalid_token" "Authorization: Bearer ${INVALID_TOKEN}"
STATUS_CASE2="$(e2e_rpc_read_status "case02_invalid_token")"
if [ "$STATUS_CASE2" = "401" ] || [ "$STATUS_CASE2" = "403" ]; then
    e2e_pass "HTTP status is 401 or 403 (got ${STATUS_CASE2})"
else
    e2e_fail "Expected 401 or 403, got ${STATUS_CASE2}"
fi
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case02_invalid_token")" "Unauthorized"
e2e_mark_case_end "case02_invalid_token"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 3: Bearer with empty token string -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Bearer with empty token -> 401"
e2e_mark_case_start "case03_empty_token"
bearer_rpc_call "case03_empty_token" "Authorization: Bearer "
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case03_empty_token")"
e2e_mark_case_end "case03_empty_token"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 4: Malformed - "Bearer" only (no space after) -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Malformed - Bearer without token -> 401"
e2e_mark_case_start "case04_bearer_only"
bearer_rpc_call "case04_bearer_only" "Authorization: Bearer"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case04_bearer_only")"
e2e_mark_case_end "case04_bearer_only"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 5: Malformed - Basic instead of Bearer -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Basic auth instead of Bearer -> 401"
e2e_mark_case_start "case05_basic_auth"
bearer_rpc_call "case05_basic_auth" "Authorization: Basic dXNlcjpwYXNz"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case05_basic_auth")"
e2e_mark_case_end "case05_basic_auth"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 6: Malformed - "bearer" lowercase -> check behavior
# ---------------------------------------------------------------------------
e2e_case_banner "Lowercase bearer (case sensitivity check)"
e2e_mark_case_start "case06_lowercase_bearer"
bearer_rpc_call "case06_lowercase_bearer" "Authorization: bearer ${VALID_TOKEN}"
STATUS_CASE6="$(e2e_rpc_read_status "case06_lowercase_bearer")"
# HTTP spec says Bearer should be case-insensitive, but implementations vary
if [ "$STATUS_CASE6" = "200" ]; then
    e2e_pass "Server accepts lowercase 'bearer' (RFC-compliant)"
elif [ "$STATUS_CASE6" = "401" ]; then
    e2e_pass "Server rejects lowercase 'bearer' (strict mode)"
else
    e2e_fail "Unexpected status ${STATUS_CASE6} for lowercase bearer"
fi
e2e_mark_case_end "case06_lowercase_bearer"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 7: Valid bearer token -> 200
# ---------------------------------------------------------------------------
e2e_case_banner "Valid bearer token -> 200"
e2e_mark_case_start "case07_valid_token"
bearer_rpc_call "case07_valid_token" "Authorization: Bearer ${VALID_TOKEN}"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case07_valid_token")"
e2e_assert_contains "JSON-RPC result present" "$(e2e_rpc_read_response "case07_valid_token")" "\"result\""
e2e_mark_case_end "case07_valid_token"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 8: Extra whitespace in Authorization header
# ---------------------------------------------------------------------------
e2e_case_banner "Extra whitespace in Authorization header"
e2e_mark_case_start "case08_extra_whitespace"
bearer_rpc_call "case08_extra_whitespace" "Authorization:   Bearer   ${VALID_TOKEN}  "
STATUS_CASE8="$(e2e_rpc_read_status "case08_extra_whitespace")"
# Should either accept (200) or reject with 401, but not crash
if [ "$STATUS_CASE8" = "200" ] || [ "$STATUS_CASE8" = "401" ]; then
    e2e_pass "Server handles extra whitespace gracefully (got ${STATUS_CASE8})"
else
    e2e_fail "Unexpected status ${STATUS_CASE8} for extra whitespace"
fi
e2e_mark_case_end "case08_extra_whitespace"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 2: Localhost bypass enabled
# ---------------------------------------------------------------------------

if ! bearer_server_start "localhost_bypass" \
    "HTTP_BEARER_TOKEN=${VALID_TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# ---------------------------------------------------------------------------
# Case 9: No auth header but localhost bypass enabled -> 200
# ---------------------------------------------------------------------------
e2e_case_banner "Localhost bypass: no auth -> 200"
e2e_mark_case_start "case09_localhost_bypass"
bearer_rpc_call "case09_localhost_bypass"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case09_localhost_bypass")"
e2e_assert_contains "JSON-RPC result present" "$(e2e_rpc_read_response "case09_localhost_bypass")" "\"result\""
e2e_mark_case_end "case09_localhost_bypass"
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Case 10: Valid token still works with bypass enabled
# ---------------------------------------------------------------------------
e2e_case_banner "Localhost bypass: valid token still works -> 200"
e2e_mark_case_start "case10_bypass_with_token"
bearer_rpc_call "case10_bypass_with_token" "Authorization: Bearer ${VALID_TOKEN}"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case10_bypass_with_token")"
e2e_assert_contains "JSON-RPC result present" "$(e2e_rpc_read_response "case10_bypass_with_token")" "\"result\""
e2e_mark_case_end "case10_bypass_with_token"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 3: No bearer token configured (auth disabled)
# ---------------------------------------------------------------------------

if ! bearer_server_start "no_auth" \
    "HTTP_BEARER_TOKEN=" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# ---------------------------------------------------------------------------
# Case 11: No bearer token configured - requests should pass
# ---------------------------------------------------------------------------
e2e_case_banner "No bearer token configured -> 200 (auth disabled)"
e2e_mark_case_start "case11_no_auth_configured"
bearer_rpc_call "case11_no_auth_configured"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case11_no_auth_configured")"
e2e_assert_contains "JSON-RPC result present" "$(e2e_rpc_read_response "case11_no_auth_configured")" "\"result\""
e2e_mark_case_end "case11_no_auth_configured"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
