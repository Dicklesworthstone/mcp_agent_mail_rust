#!/usr/bin/env bash
# test_health_url_auth.sh - E2E test for System Health URL auth workflow (br-2k3qx.4.5)
#
# Verifies:
# - /mail with valid ?token= query param -> non-401 (D3)
# - /mail with wrong ?token= -> 401 with HTML remediation page (D3/D4)
# - /mail without any auth -> 401 with HTML remediation page (D4)
# - /mail with valid Bearer header -> non-401 (existing behavior)
# - /mail subpaths (/mail/dashboard) honor query token auth
# - HTML remediation page contains actionable guidance
# - Non-mail routes (/mcp/) still return JSON 401 (unchanged)
#
# Artifacts:
# - Server logs: tests/artifacts/health_url_auth/<timestamp>/logs/server_*.log
# - Per-case: request/response JSON + body + headers + status + timing + stderr

set -euo pipefail

E2E_SUITE="health_url_auth"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Health URL Auth Workflow E2E Test Suite"

if ! command -v curl >/dev/null 2>&1; then
    e2e_log "curl not found; skipping suite"
    e2e_skip "curl required"
    e2e_summary
    exit 0
fi

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_health_url_auth")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

VALID_TOKEN="health-e2e-token-$(e2e_seeded_hex)"

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "health_auth" \
    "HTTP_PATH=/api" \
    "HTTP_BEARER_TOKEN=${VALID_TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0"; then
    e2e_fail "server failed to start"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

BASE_URL="${E2E_SERVER_URL%/mcp/}"
MAIL_URL="${BASE_URL}/mail"

# Helper: curl a URL and save status/headers/body
# Args: case_id url [extra_curl_args...]
health_curl() {
    local case_id="$1"
    local url="$2"
    shift 2
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local body_file="${case_dir}/body.txt"
    local response_file="${case_dir}/response.json"
    local headers_file="${case_dir}/headers.txt"
    local stderr_file="${case_dir}/stderr.txt"
    local status_file="${case_dir}/status.txt"
    local timing_file="${case_dir}/timing.txt"
    local request_file="${case_dir}/request.json"
    local curl_args_file="${case_dir}/curl_args.txt"
    local response_body_alias="${case_dir}/response.txt"
    local curl_exit_file="${case_dir}/curl_exit.txt"
    local status="000"
    local curl_rc=0
    local curl_output=""
    local timing_s="0"
    local timing_ms="0"
    local arg
    local sep=""

    mkdir -p "${case_dir}"

    {
        echo "{"
        echo "  \"case_id\": \"$(_e2e_json_escape "${case_id}")\","
        echo "  \"method\": \"GET\","
        echo "  \"url\": \"$(_e2e_json_escape "${url}")\","
        echo "  \"extra_args\": ["
        for arg in "$@"; do
            printf '    %s"%s"\n' "${sep}" "$(_e2e_json_escape "${arg}")"
            sep=","
        done
        echo "  ]"
        echo "}"
    } > "${request_file}"

    {
        printf 'curl -sS -D %q -o %q -w "%%{http_code}\\n%%{time_total}"' "${headers_file}" "${body_file}"
        for arg in "$@"; do
            printf ' %q' "${arg}"
        done
        printf ' %q\n' "${url}"
    } > "${curl_args_file}"

    curl_output="$(curl -sS -D "${headers_file}" -o "${body_file}" -w "%{http_code}\n%{time_total}" \
        "$@" \
        "${url}" 2>"${stderr_file}")" || curl_rc=$?

    echo "${curl_rc}" > "${curl_exit_file}"
    if [ "${curl_rc}" -ne 0 ]; then
        status="000"
        timing_s="0"
    else
        status="$(printf '%s\n' "${curl_output}" | sed -n '1p')"
        timing_s="$(printf '%s\n' "${curl_output}" | sed -n '2p')"
    fi

    if ! [[ "${status}" =~ ^[0-9]{3}$ ]]; then
        status="000"
    fi

    timing_ms="$(awk -v sec="${timing_s}" 'BEGIN { if (sec == "") sec = 0; printf "%.0f\n", sec * 1000 }' 2>/dev/null || echo "0")"
    if ! [[ "${timing_ms}" =~ ^[0-9]+$ ]]; then
        timing_ms="0"
    fi

    echo "${status}" > "${status_file}"
    echo "${timing_ms}" > "${timing_file}"
    cp "${body_file}" "${response_body_alias}" 2>/dev/null || true

    {
        echo "{"
        echo "  \"case_id\": \"$(_e2e_json_escape "${case_id}")\","
        echo "  \"status\": \"$(_e2e_json_escape "${status}")\","
        echo "  \"timing_ms\": ${timing_ms},"
        echo "  \"curl_exit\": ${curl_rc},"
        echo "  \"paths\": {"
        echo "    \"body\": \"body.txt\","
        echo "    \"headers\": \"headers.txt\","
        echo "    \"status\": \"status.txt\","
        echo "    \"timing\": \"timing.txt\","
        echo "    \"stderr\": \"stderr.txt\""
        echo "  }"
        echo "}"
    } > "${response_file}"

    echo "${status}"
}

require_observed_http_status() {
    local case_id="$1"
    local status="$2"
    if ! [[ "${status}" =~ ^[0-9]{3}$ ]] || [ "${status}" = "000" ]; then
        e2e_fail "${case_id}: request failed before HTTP response (status=${status})"
    fi
}

# ---------------------------------------------------------------------------
# Case 1: /mail with valid ?token= -> non-401
# ---------------------------------------------------------------------------
e2e_case_banner "Mail with valid query token -> non-401"
e2e_mark_case_start "case01_valid_query_token"
STATUS=$(health_curl "case01_valid_query_token" "${MAIL_URL}?token=${VALID_TOKEN}")
require_observed_http_status "case01_valid_query_token" "${STATUS}"
if [ "$STATUS" != "401" ]; then
    e2e_pass "/mail with valid ?token= returned ${STATUS} (not 401)"
else
    e2e_fail "/mail with valid ?token= returned 401 (should have authenticated)"
fi
e2e_mark_case_end "case01_valid_query_token"

# ---------------------------------------------------------------------------
# Case 2: /mail with wrong ?token= -> 401
# ---------------------------------------------------------------------------
e2e_case_banner "Mail with wrong query token -> 401"
e2e_mark_case_start "case02_wrong_query_token"
STATUS=$(health_curl "case02_wrong_query_token" "${MAIL_URL}?token=wrong-token-value")
require_observed_http_status "case02_wrong_query_token" "${STATUS}"
e2e_assert_eq "HTTP 401 for wrong token" "401" "${STATUS}"
e2e_mark_case_end "case02_wrong_query_token"

# ---------------------------------------------------------------------------
# Case 3: /mail without any auth -> 401 with HTML
# ---------------------------------------------------------------------------
e2e_case_banner "Mail without auth -> 401 HTML remediation"
e2e_mark_case_start "case03_no_auth_html"
STATUS=$(health_curl "case03_no_auth_html" "${MAIL_URL}")
require_observed_http_status "case03_no_auth_html" "${STATUS}"
e2e_assert_eq "HTTP 401 for no auth" "401" "${STATUS}"

BODY=$(cat "${E2E_ARTIFACT_DIR}/case03_no_auth_html/body.txt" 2>/dev/null || true)
HEADERS=$(cat "${E2E_ARTIFACT_DIR}/case03_no_auth_html/headers.txt" 2>/dev/null || true)

# Check Content-Type is HTML (D4)
if echo "${HEADERS}" | grep -qi "content-type.*text/html"; then
    e2e_pass "401 response is HTML (not JSON)"
else
    e2e_fail "401 response should be text/html"
fi

# Check remediation content (D4)
e2e_assert_contains "mentions env var" "${BODY}" "AGENTMAIL_HTTP_BEARER_TOKEN"
e2e_assert_contains "has fix instructions" "${BODY}" "How to fix"
e2e_assert_contains "mentions localhost tip" "${BODY}" "localhost"
e2e_mark_case_end "case03_no_auth_html"

# ---------------------------------------------------------------------------
# Case 4: /mail with valid Bearer header -> non-401
# ---------------------------------------------------------------------------
e2e_case_banner "Mail with Bearer header -> non-401"
e2e_mark_case_start "case04_bearer_header"
STATUS=$(health_curl "case04_bearer_header" "${MAIL_URL}" \
    -H "Authorization: Bearer ${VALID_TOKEN}")
require_observed_http_status "case04_bearer_header" "${STATUS}"
if [ "$STATUS" != "401" ]; then
    e2e_pass "/mail with Bearer header returned ${STATUS} (not 401)"
else
    e2e_fail "/mail with Bearer header returned 401"
fi
e2e_mark_case_end "case04_bearer_header"

# ---------------------------------------------------------------------------
# Case 5: /mail subpath with valid query token -> non-401
# ---------------------------------------------------------------------------
e2e_case_banner "Mail subpath with query token -> non-401"
e2e_mark_case_start "case05_subpath_query_token"
STATUS=$(health_curl "case05_subpath_query_token" "${MAIL_URL}/dashboard?token=${VALID_TOKEN}")
require_observed_http_status "case05_subpath_query_token" "${STATUS}"
if [ "$STATUS" != "401" ]; then
    e2e_pass "/mail/dashboard with ?token= returned ${STATUS} (not 401)"
else
    e2e_fail "/mail/dashboard with ?token= returned 401"
fi
e2e_mark_case_end "case05_subpath_query_token"

# ---------------------------------------------------------------------------
# Case 6: Non-mail route without auth -> JSON 401 (not HTML)
# ---------------------------------------------------------------------------
e2e_case_banner "Non-mail route without auth -> JSON 401"
e2e_mark_case_start "case06_mcp_json_401"
API_URL="${BASE_URL}/api/"
STATUS=$(health_curl "case06_mcp_json_401" "${API_URL}")
require_observed_http_status "case06_mcp_json_401" "${STATUS}"
e2e_assert_eq "HTTP 401 for /api/ without auth" "401" "${STATUS}"

HEADERS=$(cat "${E2E_ARTIFACT_DIR}/case06_mcp_json_401/headers.txt" 2>/dev/null || true)
if echo "${HEADERS}" | grep -qi "content-type.*application/json"; then
    e2e_pass "/api/ 401 response is JSON"
else
    e2e_fail "/api/ 401 response should be application/json"
fi
e2e_mark_case_end "case06_mcp_json_401"

# ---------------------------------------------------------------------------
# Case 7: /health bypasses auth entirely -> non-401
# ---------------------------------------------------------------------------
e2e_case_banner "Health endpoint bypasses auth -> non-401"
e2e_mark_case_start "case07_health_bypass"
STATUS=$(health_curl "case07_health_bypass" "${BASE_URL}/health")
require_observed_http_status "case07_health_bypass" "${STATUS}"
if [ "$STATUS" != "401" ]; then
    e2e_pass "/health returned ${STATUS} (not 401, auth bypassed)"
else
    e2e_fail "/health returned 401 (should bypass auth)"
fi
e2e_mark_case_end "case07_health_bypass"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
