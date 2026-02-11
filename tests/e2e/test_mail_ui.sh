#!/usr/bin/env bash
# test_mail_ui.sh - E2E test suite for Mail SSR UI routes (/mail/*)
#
# Verifies:
# - Mail UI routes render without crashing against a local sqlite DB + storage root
# - Key pages contain expected markers: index, project view, inbox, thread, search, compose
# - Message body sanitization neutralizes common XSS payloads (no javascript: URLs / event handlers)
#
# Artifacts:
# - Server logs: tests/artifacts/mail_ui/<timestamp>/server.log
# - Per-route HTML/JSON responses + headers + curl stderr
# - Seed tool calls (JSON-RPC request/response) for debugging

set -euo pipefail

E2E_SUITE="mail_ui"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Mail UI E2E Test Suite"

if ! command -v curl >/dev/null 2>&1; then
    e2e_log "curl not found; skipping suite"
    e2e_skip "curl required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

WORK="$(e2e_mktemp "e2e_mail_ui")"
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

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"
SERVER_LOG="${E2E_ARTIFACT_DIR}/server.log"

e2e_log "Starting server:"
e2e_log "  bin:   ${BIN}"
e2e_log "  host:  127.0.0.1"
e2e_log "  port:  ${PORT}"
e2e_log "  db:    ${DB_PATH}"
e2e_log "  store: ${STORAGE_ROOT}"

(
    export DATABASE_URL="sqlite:////${DB_PATH}"
    export STORAGE_ROOT="${STORAGE_ROOT}"
    export HTTP_HOST="127.0.0.1"
    export HTTP_PORT="${PORT}"
    export HTTP_PATH="/api"
    export HTTP_BEARER_TOKEN="${TOKEN}"
    export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED="0"
    export HTTP_RBAC_ENABLED="0"
    export HTTP_RATE_LIMIT_ENABLED="0"
    "${BIN}" serve --host 127.0.0.1 --port "${PORT}"
) >"${SERVER_LOG}" 2>&1 &
SERVER_PID=$!

cleanup_server() {
    if kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" 2>/dev/null || true
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

API_URL="http://127.0.0.1:${PORT}/api/"

rpc_call() {
    local case_id="$1"
    local tool_name="$2"
    local args_json="$3"

    local headers_file="${E2E_ARTIFACT_DIR}/seed_${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/seed_${case_id}_body.json"
    local status_file="${E2E_ARTIFACT_DIR}/seed_${case_id}_status.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/seed_${case_id}_curl_stderr.txt"

    local payload
    payload="$(python3 -c "
import json, sys
tool = sys.argv[1]
args = json.loads(sys.argv[2])
print(json.dumps({
  'jsonrpc': '2.0',
  'method': 'tools/call',
  'id': 1,
  'params': { 'name': tool, 'arguments': args }
}))
" "$tool_name" "$args_json")"

    e2e_save_artifact "seed_${case_id}_request.json" "$payload"

    set +e
    local status
    status="$(curl -sS -D "${headers_file}" -o "${body_file}" -w "%{http_code}" \
        -X POST "${API_URL}" \
        -H "content-type: application/json" \
        -H "authorization: Bearer ${TOKEN}" \
        --data "${payload}" \
        2>"${curl_stderr_file}")"
    local rc=$?
    set -e

    echo "${status}" > "${status_file}"
    if [ "$rc" -ne 0 ]; then
        e2e_fail "seed_${case_id}: curl failed rc=${rc}"
        return 1
    fi
    if [ "${status}" != "200" ]; then
        e2e_fail "seed_${case_id}: unexpected HTTP status ${status}"
        return 1
    fi
    return 0
}

extract_tool_json() {
    local resp_file="$1"
    python3 -c "
import json, sys
data = json.load(open(sys.argv[1], 'r', encoding='utf-8'))
res = data.get('result') or {}
content = res.get('content') or []
if content and isinstance(content[0], dict) and content[0].get('type') == 'text':
    print(content[0].get('text') or '')
else:
    # Fallback: dump result directly (best-effort)
    print(json.dumps(res))
" "$resp_file"
}

http_get() {
    local case_id="$1"
    local url="$2"

    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.txt"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"

    set +e
    local status
    status="$(curl -sS -D "${headers_file}" -o "${body_file}" -w "%{http_code}" \
        -H "authorization: Bearer ${TOKEN}" \
        "${url}" 2>"${curl_stderr_file}")"
    local rc=$?
    set -e

    echo "${status}" > "${status_file}"
    if [ "$rc" -ne 0 ]; then
        e2e_fail "${case_id}: curl failed rc=${rc}"
        return 1
    fi
    if [ "${status}" != "200" ]; then
        e2e_fail "${case_id}: unexpected HTTP status ${status}"
        return 1
    fi
    return 0
}

http_get_expect_status() {
    local case_id="$1"
    local url="$2"
    local expected_status="$3"

    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.txt"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"

    set +e
    local status
    status="$(curl -sS -D "${headers_file}" -o "${body_file}" -w "%{http_code}" \
        -H "authorization: Bearer ${TOKEN}" \
        "${url}" 2>"${curl_stderr_file}")"
    local rc=$?
    set -e

    echo "${status}" > "${status_file}"
    if [ "$rc" -ne 0 ]; then
        e2e_fail "${case_id}: curl failed rc=${rc}"
        return 1
    fi
    if [ "${status}" != "${expected_status}" ]; then
        e2e_fail "${case_id}: unexpected HTTP status ${status} (expected ${expected_status})"
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# Seed data via JSON-RPC tools
# ---------------------------------------------------------------------------

e2e_case_banner "Seed: ensure_project + register_agent + send_message"

PROJECT_DIR="$(e2e_mktemp "e2e_mail_ui_project")"

rpc_call "ensure_project" "ensure_project" "{\"human_key\": \"${PROJECT_DIR}\"}" || true
PROJECT_JSON="$(extract_tool_json "${E2E_ARTIFACT_DIR}/seed_ensure_project_body.json")"
PROJECT_SLUG="$(python3 -c "import json, sys; print(json.loads(sys.argv[1])['slug'])" "$PROJECT_JSON")"

rpc_call "register_agent_sender" "register_agent" "{\"project_key\": \"${PROJECT_DIR}\", \"program\": \"e2e\", \"model\": \"test\", \"name\": \"RedFox\", \"task_description\": \"e2e seed\"}" || true
rpc_call "register_agent_recipient" "register_agent" "{\"project_key\": \"${PROJECT_DIR}\", \"program\": \"e2e\", \"model\": \"test\", \"name\": \"BlueBear\", \"task_description\": \"e2e seed\"}" || true

XSS_MD=$'Hello <script>alert(1)</script>\\n\\n[click](javascript:alert(2))\\n\\n<img src=\"x\" onerror=\"alert(3)\">\\n'
rpc_call "send_message" "send_message" "$(python3 -c "import json,sys; print(json.dumps({\"project_key\": sys.argv[1], \"sender_name\": \"RedFox\", \"to\": [\"BlueBear\"], \"subject\": \"[br-123] XSS probe\", \"body_md\": sys.argv[2], \"thread_id\": \"br-123\"}))" "${PROJECT_DIR}" "${XSS_MD}")" || true

e2e_pass "seeded project=${PROJECT_SLUG}"

# ---------------------------------------------------------------------------
# Fetch pages (/mail/*)
# ---------------------------------------------------------------------------

BASE_URL="http://127.0.0.1:${PORT}"

e2e_case_banner "GET /mail (index)"
http_get "mail_index" "${BASE_URL}/mail" || true
MAIL_INDEX_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_index_body.txt")"
e2e_assert_contains "index includes Projects title" "${MAIL_INDEX_BODY}" "Projects"
e2e_assert_contains "index includes project slug" "${MAIL_INDEX_BODY}" "${PROJECT_SLUG}"

e2e_case_banner "GET /mail/${PROJECT_SLUG} (project view)"
http_get "mail_project" "${BASE_URL}/mail/${PROJECT_SLUG}" || true
MAIL_PROJECT_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_project_body.txt")"
e2e_assert_contains "project page includes slug" "${MAIL_PROJECT_BODY}" "${PROJECT_SLUG}"

e2e_case_banner "GET /mail/${PROJECT_SLUG}/inbox/BlueBear (inbox)"
http_get "mail_inbox" "${BASE_URL}/mail/${PROJECT_SLUG}/inbox/BlueBear?limit=50&page=1" || true
MAIL_INBOX_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_inbox_body.txt")"
e2e_assert_contains "inbox contains subject" "${MAIL_INBOX_BODY}" "[br-123] XSS probe"

e2e_case_banner "GET /mail/${PROJECT_SLUG}/thread/br-123 (thread)"
http_get "mail_thread" "${BASE_URL}/mail/${PROJECT_SLUG}/thread/br-123" || true
MAIL_THREAD_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_thread_body.txt")"
e2e_assert_contains "thread contains subject" "${MAIL_THREAD_BODY}" "[br-123] XSS probe"
e2e_assert_contains "thread contains sender" "${MAIL_THREAD_BODY}" "RedFox"
e2e_assert_contains "thread has markdown text" "${MAIL_THREAD_BODY}" "click"
e2e_assert_contains "thread has img tag" "${MAIL_THREAD_BODY}" "<img"
e2e_assert_not_contains "thread strips script tag" "${MAIL_THREAD_BODY}" "<script>alert(1)"
e2e_assert_contains "thread preserves script text as plain text" "${MAIL_THREAD_BODY}" "alert(1)"
e2e_assert_contains "thread neutralizes javascript url" "${MAIL_THREAD_BODY}" "click"
e2e_assert_contains "thread strips onerror attr" "${MAIL_THREAD_BODY}" "img"
e2e_assert_not_contains "thread neutralizes javascript href (double quotes)" "${MAIL_THREAD_BODY}" "href=\"javascript:"
e2e_assert_not_contains "thread neutralizes javascript href (single quotes)" "${MAIL_THREAD_BODY}" "href='javascript:"
e2e_assert_not_contains "thread does not include onerror attribute" "${MAIL_THREAD_BODY}" "onerror="

e2e_case_banner "GET /mail/${PROJECT_SLUG}/search?q=br-123 (search results)"
http_get "mail_search" "${BASE_URL}/mail/${PROJECT_SLUG}/search?q=br-123&limit=10" || true
MAIL_SEARCH_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_search_body.txt")"
e2e_assert_contains "search includes subject" "${MAIL_SEARCH_BODY}" "[br-123] XSS probe"

e2e_case_banner "GET /mail/${PROJECT_SLUG}/overseer/compose (compose)"
http_get "mail_compose" "${BASE_URL}/mail/${PROJECT_SLUG}/overseer/compose" || true
MAIL_COMPOSE_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_compose_body.txt")"
e2e_assert_contains "compose includes agents" "${MAIL_COMPOSE_BODY}" "BlueBear"

e2e_case_banner "GET /mail/unified-inbox (unified)"
http_get "mail_unified" "${BASE_URL}/mail/unified-inbox?limit=50" || true
MAIL_UNIFIED_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_unified_body.txt")"
e2e_assert_contains "unified inbox includes subject" "${MAIL_UNIFIED_BODY}" "[br-123] XSS probe"

e2e_case_banner "GET /mail/api/unified-inbox (json api)"
http_get "mail_api_unified" "${BASE_URL}/mail/api/unified-inbox" || true
MAIL_API_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_api_unified_body.txt")"
e2e_assert_contains "api returns JSON array" "${MAIL_API_BODY}" "\"messages\""

e2e_case_banner "GET /mail/archive/browser/{project}/file invalid path (json 400)"
http_get_expect_status "mail_archive_file_invalid_path" "${BASE_URL}/mail/archive/browser/${PROJECT_SLUG}/file?path=../etc/passwd" "400" || true
MAIL_ARCHIVE_FILE_INVALID_HEADERS="$(cat "${E2E_ARTIFACT_DIR}/mail_archive_file_invalid_path_headers.txt")"
MAIL_ARCHIVE_FILE_INVALID_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_archive_file_invalid_path_body.txt")"
e2e_assert_contains "archive file invalid path has JSON content-type" "${MAIL_ARCHIVE_FILE_INVALID_HEADERS}" "application/json"
e2e_assert_contains "archive file invalid path body has detail" "${MAIL_ARCHIVE_FILE_INVALID_BODY}" "\"detail\""
e2e_assert_contains "archive file invalid path message" "${MAIL_ARCHIVE_FILE_INVALID_BODY}" "Invalid file path"

e2e_case_banner "GET /mail/archive/browser/{project}/file missing file (json 404)"
http_get_expect_status "mail_archive_file_not_found" "${BASE_URL}/mail/archive/browser/${PROJECT_SLUG}/file?path=messages/missing.md" "404" || true
MAIL_ARCHIVE_FILE_NOT_FOUND_HEADERS="$(cat "${E2E_ARTIFACT_DIR}/mail_archive_file_not_found_headers.txt")"
MAIL_ARCHIVE_FILE_NOT_FOUND_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_archive_file_not_found_body.txt")"
e2e_assert_contains "archive file missing has JSON content-type" "${MAIL_ARCHIVE_FILE_NOT_FOUND_HEADERS}" "application/json"
e2e_assert_contains "archive file missing has detail" "${MAIL_ARCHIVE_FILE_NOT_FOUND_BODY}" "\"detail\""
e2e_assert_contains "archive file missing message" "${MAIL_ARCHIVE_FILE_NOT_FOUND_BODY}" "File not found"

SNAP_TS="$(
python3 - <<'PY'
from datetime import datetime, timezone
print(datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M"))
PY
)"
e2e_case_banner "GET /mail/archive/time-travel/snapshot (json 200)"
http_get "mail_archive_snapshot" "${BASE_URL}/mail/archive/time-travel/snapshot?project=${PROJECT_SLUG}&agent=BlueBear&timestamp=${SNAP_TS}" || true
MAIL_ARCHIVE_SNAPSHOT_HEADERS="$(cat "${E2E_ARTIFACT_DIR}/mail_archive_snapshot_headers.txt")"
MAIL_ARCHIVE_SNAPSHOT_BODY="$(cat "${E2E_ARTIFACT_DIR}/mail_archive_snapshot_body.txt")"
e2e_assert_contains "archive snapshot has JSON content-type" "${MAIL_ARCHIVE_SNAPSHOT_HEADERS}" "application/json"
e2e_assert_contains "archive snapshot has messages key" "${MAIL_ARCHIVE_SNAPSHOT_BODY}" "\"messages\""
e2e_assert_contains "archive snapshot has snapshot_time key" "${MAIL_ARCHIVE_SNAPSHOT_BODY}" "\"snapshot_time\""
e2e_assert_contains "archive snapshot has commit_sha key" "${MAIL_ARCHIVE_SNAPSHOT_BODY}" "\"commit_sha\""
e2e_assert_contains "archive snapshot has requested_time key" "${MAIL_ARCHIVE_SNAPSHOT_BODY}" "\"requested_time\""

e2e_summary
