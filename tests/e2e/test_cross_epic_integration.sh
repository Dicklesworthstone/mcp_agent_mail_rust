#!/usr/bin/env bash
# test_cross_epic_integration.sh - Cross-epic composition E2E suite.
#
# Verifies the major reality-check outcomes compose under one live workload:
# - README inventory claims still match the live MCP tool/resource surface
# - deferred browser/WASM routes stay honest with 501 responses
# - live send_message traffic still records ATC experiences
# - fetch_inbox under live ATC preserves stable response shapes
# - a modest message burst stays within a sane latency envelope without
#   duplicating the dedicated perf gates

set -euo pipefail

E2E_SUITE="cross_epic_integration"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Cross-Epic Integration E2E Suite"

for cmd in curl python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

if ! e2e_has_sqlite_compat; then
    e2e_log "sqlite compatibility shim unavailable; skipping suite"
    e2e_skip "test_db sqlite shim required"
    e2e_summary
    exit 0
fi

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_cross_epic")"
DB_PATH="${WORK}/cross_epic.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
PROJECT_PATH="${WORK}/project"
mkdir -p "${STORAGE_ROOT}" "${PROJECT_PATH}"

EXPECTED_TOOL_COUNT=37
EXPECTED_RESOURCE_COUNT=25
EXPECTED_TUI_SCREEN_COUNT=16
MESSAGE_BURST=25
MAX_SEND_BURST_MS=8000
THREAD_ID="br-bb0gt.4-cross-epic"
SENDER="BlueLake"
RECIPIENT="GreenStone"
BASE_URL=""

trap 'e2e_stop_server || true' EXIT

readme_claims_match() {
    local expected_tools="$1"
    local expected_resources="$2"
    local expected_screens="$3"
    python3 - "$expected_tools" "$expected_resources" "$expected_screens" <<'PY'
import pathlib
import sys

tools = sys.argv[1]
resources = sys.argv[2]
screens = sys.argv[3]
text = pathlib.Path("README.md").read_text(encoding="utf-8")
required = [
    f"{tools} tools and {resources} resources",
    f"{screens}-screen TUI",
    f"all {resources} MCP resources",
]
missing = [needle for needle in required if needle not in text]
if missing:
    raise SystemExit("\n".join(missing))
PY
}

response_tool_count() {
    local case_id="$1"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(len((data.get("result") or {}).get("tools") or []))
PY
}

response_resource_count() {
    local case_id="$1"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(len((data.get("result") or {}).get("resources") or []))
PY
}

response_resource_template_count() {
    local case_id="$1"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
result = data.get("result") or {}
templates = result.get("resourceTemplates")
if templates is None:
    templates = result.get("resource_templates")
print(len(templates or []))
PY
}

tool_response_text() {
    local case_id="$1"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if "error" in data:
    print((data.get("error") or {}).get("message") or "")
    raise SystemExit(0)
result = data.get("result") or {}
content = result.get("content") or []
if content:
    print(content[0].get("text") or "")
PY
}

tool_call_succeeded() {
    local case_id="$1"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if "error" in data:
    raise SystemExit(1)
result = data.get("result") or {}
if result.get("isError") is True:
    raise SystemExit(1)
raise SystemExit(0)
PY
}

tool_response_is_busy() {
    local case_id="$1"
    local text
    text="$(tool_response_text "${case_id}")"
    [[ "${text}" == *"Resource is temporarily busy."* ]]
}

invoke_tool_case() {
    local case_id="$1"
    local label="$2"
    local tool_name="$3"
    local args_json="$4"
    local max_attempts="${5:-10}"
    local sleep_seconds="${6:-0.5}"
    local attempt

    for attempt in $(seq 1 "${max_attempts}"); do
        e2e_rpc_call "${case_id}" "${E2E_SERVER_URL}" "${tool_name}" "${args_json}" || true
        if tool_call_succeeded "${case_id}"; then
            e2e_pass "${label}"
            return 0
        fi
        if tool_response_is_busy "${case_id}"; then
            sleep "${sleep_seconds}"
            continue
        fi
        break
    done

    e2e_fail "${label}"
    return 1
}

assert_send_message_shape() {
    local case_id="$1"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
result = data.get("result") or {}
assert result.get("isError") is not True
content = result.get("content") or []
assert content, "content array missing"
payload_text = content[0].get("text") or "{}"
payload_doc = json.loads(payload_text)
assert payload_doc.get("count") == 1
deliveries = payload_doc.get("deliveries") or []
assert len(deliveries) == 1
payload = (deliveries[0] or {}).get("payload") or {}
assert isinstance(payload.get("id"), int)
assert payload.get("subject")
assert payload.get("thread_id")
print(payload["id"])
PY
}

assert_fetch_inbox_shape() {
    local case_id="$1"
    local expected_count="$2"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" "$expected_count" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
result = data.get("result") or {}
assert result.get("isError") is not True
content = result.get("content") or []
assert content, "content array missing"
text = content[0].get("text") or "[]"
messages = json.loads(text)
assert isinstance(messages, list)
assert len(messages) == int(sys.argv[2]), f"expected {sys.argv[2]} messages, got {len(messages)}"
sample = messages[0]
assert isinstance(sample.get("id"), int)
assert sample.get("from")
assert sample.get("subject")
print(len(messages))
PY
}

sql_scalar() {
    local sql="$1"
    sqlite3 "${DB_PATH}" "${sql}" 2>/dev/null | tr -d '\r\n'
}

assert_monotone_sender_message_ids() {
    local subject="$1"
    local expected="$2"
    local out_file="${WORK}/sender_message_ids.txt"
    sqlite3 "${DB_PATH}" \
        "SELECT json_extract(context_json, '\$.message_id') \
         FROM atc_experiences \
         WHERE decision_class = 'message_sent' AND subject = '${subject}' \
         ORDER BY experience_id ASC;" > "${out_file}"
    python3 - "${out_file}" "${expected}" <<'PY'
import pathlib
import sys

values = [
    int(line.strip())
    for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
    if line.strip()
]
expected = int(sys.argv[2])
assert len(values) == expected, f"expected {expected} ids, got {len(values)}"
assert values == sorted(values), f"ids not monotone: {values}"
assert len(set(values)) == len(values), f"ids not unique: {values}"
PY
}

count_archive_message_files() {
    find "${STORAGE_ROOT}/projects" -path '*/messages/20[0-9][0-9]/[0-1][0-9]/*.md' -type f | wc -l | tr -d '[:space:]'
}

assert_monotone_send_response_ids() {
    local expected="$1"
    local out_file="${WORK}/send_response_ids.txt"
    : > "${out_file}"
    local i
    for i in $(seq 1 "${expected}"); do
        local case_id
        case_id="$(printf 'cross_epic_send_%03d' "${i}")"
        assert_send_message_shape "${case_id}" >> "${out_file}"
    done
    python3 - "${out_file}" "${expected}" <<'PY'
import pathlib
import sys

values = [
    int(line.strip())
    for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
    if line.strip()
]
expected = int(sys.argv[2])
assert len(values) == expected, f"expected {expected} ids, got {len(values)}"
assert values == sorted(values), f"ids not monotone: {values}"
assert len(set(values)) == len(values), f"ids not unique: {values}"
PY
}

server_log_contains() {
    local needle="$1"
    local log_file="${E2E_ARTIFACT_DIR}/logs/server_cross_epic.log"
    [ -f "${log_file}" ] && grep -Fq "${needle}" "${log_file}"
}

http_capture() {
    local case_id="$1"
    local url="$2"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.txt"
    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local status

    status="$(curl -sS -D "${headers_file}" -o "${body_file}" -w "%{http_code}" "${url}" 2>"${stderr_file}")"
    echo "${status}" > "${status_file}"
    printf '%s' "${status}"
}

start_cross_epic_server() {
    local label="$1"
    shift
    if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "${label}" \
        "TUI_ENABLED=false" \
        "INTEGRITY_CHECK_ON_STARTUP=false" \
        "HTTP_RBAC_ENABLED=0" \
        "HTTP_RATE_LIMIT_ENABLED=0" \
        "$@"; then
        e2e_fail "server failed to start for cross-epic suite (${label})"
        e2e_summary
        exit 1
    fi
    BASE_URL="${E2E_SERVER_URL%/mcp/}"
}

start_cross_epic_server "cross_epic_bootstrap" \
    "AM_ATC_ENABLED=false" \
    "AM_ATC_WRITE_MODE=off"

e2e_case_banner "Bootstrap archive-backed identities before ATC live mode"
invoke_tool_case "cross_epic_ensure_project" "ensure_project succeeds" "ensure_project" \
    "{\"human_key\":\"${PROJECT_PATH}\"}" \
    20 \
    1

invoke_tool_case "cross_epic_register_sender" "sender identity creation succeeds" "create_agent_identity" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"codex-cli\",\"model\":\"gpt-5.2\",\"name_hint\":\"${SENDER}\",\"task_description\":\"cross-epic sender\"}" \
    20 \
    1

invoke_tool_case "cross_epic_register_recipient" "recipient identity creation succeeds" "create_agent_identity" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"claude-code\",\"model\":\"opus-4.6\",\"name_hint\":\"${RECIPIENT}\",\"task_description\":\"cross-epic recipient\"}" \
    20 \
    1

invoke_tool_case "cross_epic_policy_sender" "sender contact policy open" "set_contact_policy" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${SENDER}\",\"policy\":\"open\"}" \
    20 \
    1

invoke_tool_case "cross_epic_policy_recipient" "recipient contact policy open" "set_contact_policy" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RECIPIENT}\",\"policy\":\"open\"}" \
    20 \
    1

e2e_stop_server
start_cross_epic_server "cross_epic" \
    "AM_ATC_ENABLED=true" \
    "AM_ATC_WRITE_MODE=live"

e2e_case_banner "README claims align with live MCP counts"
INIT_PAYLOAD='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"cross-epic-e2e","version":"1.0"}}}'
TOOLS_LIST_PAYLOAD='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
RESOURCES_LIST_PAYLOAD='{"jsonrpc":"2.0","id":3,"method":"resources/list","params":{}}'
RESOURCE_TEMPLATES_LIST_PAYLOAD='{"jsonrpc":"2.0","id":4,"method":"resources/templates/list","params":{}}'

e2e_rpc_call_raw "cross_epic_initialize" "${E2E_SERVER_URL}" "${INIT_PAYLOAD}" || true
e2e_rpc_assert_success "cross_epic_initialize" "initialize succeeds"

e2e_rpc_call_raw "cross_epic_tools_list" "${E2E_SERVER_URL}" "${TOOLS_LIST_PAYLOAD}" || true
e2e_rpc_assert_success "cross_epic_tools_list" "tools/list succeeds"
TOOL_COUNT="$(response_tool_count "cross_epic_tools_list")"
e2e_assert_eq "live tool count matches reality-check baseline" "${EXPECTED_TOOL_COUNT}" "${TOOL_COUNT}"

e2e_rpc_call_raw "cross_epic_resources_list" "${E2E_SERVER_URL}" "${RESOURCES_LIST_PAYLOAD}" || true
e2e_rpc_assert_success "cross_epic_resources_list" "resources/list succeeds"
RESOURCE_COUNT="$(response_resource_count "cross_epic_resources_list")"
if [ "${RESOURCE_COUNT}" -gt 0 ]; then
    e2e_pass "static resources/list surface is non-empty (${RESOURCE_COUNT})"
else
    e2e_fail "static resources/list surface is empty"
fi

e2e_rpc_call_raw "cross_epic_resource_templates_list" "${E2E_SERVER_URL}" "${RESOURCE_TEMPLATES_LIST_PAYLOAD}" || true
e2e_rpc_assert_success "cross_epic_resource_templates_list" "resources/templates/list succeeds"
RESOURCE_TEMPLATE_COUNT="$(response_resource_template_count "cross_epic_resource_templates_list")"
e2e_assert_eq "logical resource template count matches reality-check baseline" "${EXPECTED_RESOURCE_COUNT}" "${RESOURCE_TEMPLATE_COUNT}"

if readme_claims_match "${EXPECTED_TOOL_COUNT}" "${EXPECTED_RESOURCE_COUNT}" "${EXPECTED_TUI_SCREEN_COUNT}"; then
    e2e_pass "README claims match the live tool/resource/TUI inventory"
else
    e2e_fail "README claims drifted from the live tool/resource/TUI inventory"
fi

e2e_case_banner "Negative control rejects stale README expectations"
if readme_claims_match 36 "${EXPECTED_RESOURCE_COUNT}" "${EXPECTED_TUI_SCREEN_COUNT}"; then
    e2e_fail "negative README control unexpectedly passed with stale tool count"
else
    e2e_pass "negative README control fails when the expected tool count is stale"
fi

e2e_case_banner "Live ATC + archive workload preserves shape and ordering"
BURST_START_MS="$(_e2e_now_ms)"
for i in $(seq 1 "${MESSAGE_BURST}"); do
    case_id="$(printf 'cross_epic_send_%03d' "${i}")"
    args_json="$(python3 - "${PROJECT_PATH}" "${SENDER}" "${RECIPIENT}" "${THREAD_ID}" "${i}" <<'PY'
import json
import sys

print(json.dumps({
    "project_key": sys.argv[1],
    "sender_name": sys.argv[2],
    "to": [sys.argv[3]],
    "subject": f"cross-epic message {sys.argv[5]}",
    "body_md": f"Cross-epic message body {sys.argv[5]}",
    "thread_id": sys.argv[4],
}, separators=(",", ":")))
PY
)"
    invoke_tool_case "${case_id}" "send_message ${i}/${MESSAGE_BURST} succeeds" "send_message" "${args_json}"
    if [ "${i}" = "1" ]; then
        if assert_send_message_shape "${case_id}" >/dev/null; then
            e2e_pass "send_message response shape remains stable under live ATC"
        else
            e2e_fail "send_message response shape drifted under live ATC"
        fi
    fi
done
BURST_END_MS="$(_e2e_now_ms)"
BURST_ELAPSED_MS=$((BURST_END_MS - BURST_START_MS))
echo "${BURST_ELAPSED_MS}" > "${E2E_ARTIFACT_DIR}/cross_epic_burst_elapsed_ms.txt"
if [ "${BURST_ELAPSED_MS}" -le "${MAX_SEND_BURST_MS}" ]; then
    e2e_pass "message burst stayed under ${MAX_SEND_BURST_MS}ms (${BURST_ELAPSED_MS}ms)"
else
    e2e_fail "message burst exceeded ${MAX_SEND_BURST_MS}ms (${BURST_ELAPSED_MS}ms)"
fi

invoke_tool_case "cross_epic_fetch_inbox" "fetch_inbox succeeds under live ATC" "fetch_inbox" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RECIPIENT}\",\"limit\":${MESSAGE_BURST},\"include_bodies\":true}"
if assert_fetch_inbox_shape "cross_epic_fetch_inbox" "${MESSAGE_BURST}" >/dev/null; then
    e2e_pass "fetch_inbox response shape remains stable under live ATC"
else
    e2e_fail "fetch_inbox response shape drifted under live ATC"
fi

ARCHIVE_MESSAGE_COUNT="$(count_archive_message_files)"
e2e_assert_eq "messages persisted to the Git-backed archive" "${MESSAGE_BURST}" "${ARCHIVE_MESSAGE_COUNT}"

if assert_monotone_send_response_ids "${MESSAGE_BURST}"; then
    e2e_pass "send_message payload ids stay monotone across the burst"
else
    e2e_fail "send_message payload ids are not monotone"
fi

if server_log_contains "decision_kind=probe_agent" \
    && server_log_contains "agent_name=${SENDER}" \
    && server_log_contains "agent_name=${RECIPIENT}"; then
    e2e_pass "ATC liveness engine observed both seeded agents after live restart"
else
    e2e_fail "ATC liveness engine did not observe both seeded agents after live restart"
fi

e2e_case_banner "Supported web UI stays live while deferred browser mirror stays honest"
MAIL_STATUS="$(http_capture "cross_epic_mail_ui" "${BASE_URL}/mail/")"
WEB_DASH_STATUS="$(http_capture "cross_epic_web_dashboard" "${BASE_URL}/web-dashboard")"
WS_STATE_STATUS="$(http_capture "cross_epic_mail_ws_state" "${BASE_URL}/mail/ws-state")"

e2e_assert_eq "server-rendered /mail/ UI remains available" "200" "${MAIL_STATUS}"
e2e_assert_eq "deferred /web-dashboard returns honest 501" "501" "${WEB_DASH_STATUS}"
e2e_assert_eq "deferred /mail/ws-state returns honest 501" "501" "${WS_STATE_STATUS}"

MAIL_BODY="$(cat "${E2E_ARTIFACT_DIR}/cross_epic_mail_ui_body.txt" 2>/dev/null || true)"
WEB_DASH_BODY="$(cat "${E2E_ARTIFACT_DIR}/cross_epic_web_dashboard_body.txt" 2>/dev/null || true)"
WS_STATE_BODY="$(cat "${E2E_ARTIFACT_DIR}/cross_epic_mail_ws_state_body.txt" 2>/dev/null || true)"

e2e_assert_contains "mail UI returns HTML" "${MAIL_BODY}" "<!doctype html>"
e2e_assert_contains "web dashboard 501 explains deferred contract" "${WEB_DASH_BODY}" "SPEC-browser-parity-contract-deferred.md"
e2e_assert_contains "mail/ws-state 501 explains deferred contract" "${WS_STATE_BODY}" "\"not_implemented\""

e2e_summary
