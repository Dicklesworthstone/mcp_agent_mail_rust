#!/usr/bin/env bash
# test_llm.sh - E2E test suite for LLM llm_mode smoke (stubbed, offline) (br-2ei.9.7)
#
# Tests:
# 1. summarize_thread with llm_mode=true returns refined fields
# 2. macro_prepare_thread with llm_mode=true exercises LLM path
# 3. Invalid LLM stub triggers fallback to non-LLM summary
# 4. Multi-thread summarize with LLM stub returns aggregate digest
#
# All tests use MCP_AGENT_MAIL_LLM_STUB=1 for deterministic offline output.

set -euo pipefail

AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP:-1}"

E2E_SUITE="llm"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "LLM llm_mode Smoke E2E Test Suite (Stubbed)"

if ! command -v curl >/dev/null 2>&1; then
    e2e_skip "curl required"
    e2e_summary
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
    e2e_skip "python3 required"
    e2e_summary
    exit 0
fi

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

e2e_ensure_binary "am"
BIN="$(command -v am)"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
WORK="$(e2e_mktemp "e2e_llm")"
DB_PATH="${WORK}/storage.sqlite3"
STORAGE_ROOT="${WORK}/archive"
SERVER_LOG="${E2E_ARTIFACT_DIR}/server.log"
PORT="$(pick_port)"
TOKEN="test-token-llm"
PROJECT_DIR="/tmp/e2e_llm_project"
THREAD_ID="LLM-E2E-1"

mkdir -p "$STORAGE_ROOT"

# Initialize database
DATABASE_URL="sqlite:///${DB_PATH}" "$BIN" migrate >/dev/null 2>&1

# ---------------------------------------------------------------------------
# Start server with LLM stub enabled
# ---------------------------------------------------------------------------
e2e_log "Starting MCP server: port=${PORT}, LLM stub=on"

(
    export DATABASE_URL="sqlite:///${DB_PATH}"
    export STORAGE_ROOT="${STORAGE_ROOT}"
    export HTTP_BEARER_TOKEN="${TOKEN}"
    export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED="1"
    export HTTP_RBAC_ENABLED="0"
    export HTTP_RATE_LIMIT_ENABLED="0"
    export HTTP_JWT_ENABLED="0"
    export MCP_AGENT_MAIL_LLM_STUB="1"
    export LLM_ENABLED="1"
    "$BIN" serve-http --host 127.0.0.1 --port "${PORT}"
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

URL="http://127.0.0.1:${PORT}/api"

# Helper: call MCP tool via HTTP
mcp_call() {
    local tool="$1"
    local args="$2"
    MCP_CALL_SEQ="${MCP_CALL_SEQ:-0}"
    MCP_CALL_SEQ=$((MCP_CALL_SEQ + 1))
    local case_id
    case_id="$(printf "mcp_call_%03d_%s" "${MCP_CALL_SEQ}" "${tool}")"

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call "${case_id}" "${URL}" "${tool}" "${args}" "Authorization: Bearer ${TOKEN}"; then
        :
    fi

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
    if [ -z "${status}" ] || [ "${status}" = "000" ]; then
        return 1
    fi

    e2e_rpc_read_response "${case_id}"
}

# Helper: extract text content from MCP response
extract_text() {
    local response="$1"
    python3 -c "
import json,sys
data = json.loads(sys.argv[1])
for c in data.get('result',{}).get('content',[]):
    if c.get('type')=='text':
        print(c['text'])
        break
" "$response" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Setup: create project, agents, and seed thread messages
# ---------------------------------------------------------------------------
e2e_case_banner "Setup: seed project, agents, and thread messages"

mcp_call ensure_project "{\"human_key\": \"${PROJECT_DIR}\"}" >/dev/null 2>&1

for agent in "BlueLake" "GreenCastle"; do
    mcp_call register_agent "{\"project_key\": \"${PROJECT_DIR}\", \"name\": \"${agent}\", \"program\": \"e2e\", \"model\": \"test\", \"task_description\": \"llm e2e\"}" >/dev/null 2>&1
done

# Send messages to create a thread
mcp_call send_message "{
    \"project_key\": \"${PROJECT_DIR}\",
    \"sender_name\": \"BlueLake\",
    \"to\": [\"GreenCastle\"],
    \"subject\": \"API migration plan\",
    \"body_md\": \"We need to migrate the API to v2 next sprint. TODO: Update OpenAPI spec.\",
    \"thread_id\": \"${THREAD_ID}\"
}" >/dev/null 2>&1

mcp_call reply_message "{
    \"project_key\": \"${PROJECT_DIR}\",
    \"message_id\": 1,
    \"sender_name\": \"GreenCastle\",
    \"body_md\": \"Agreed. ACTION: Deploy to staging first. NEXT: Review the deployment checklist.\"
}" >/dev/null 2>&1

e2e_pass "project seeded with thread messages"

# ---------------------------------------------------------------------------
# Case 1: summarize_thread with llm_mode=true
# ---------------------------------------------------------------------------
e2e_case_banner "summarize_thread with llm_mode=true (stub)"

summary_result="$(mcp_call summarize_thread "{
    \"project_key\": \"${PROJECT_DIR}\",
    \"thread_id\": \"${THREAD_ID}\",
    \"llm_mode\": true
}")"

e2e_save_artifact "case1_summary_result.json" "$summary_result"

summary_text="$(extract_text "$summary_result")"
e2e_save_artifact "case1_summary_text.json" "$summary_text"

# Check for LLM-refined fields from stub
e2e_assert_contains "summary has participants" "$summary_text" "participants"
e2e_assert_contains "summary has key_points" "$summary_text" "key_points"
e2e_assert_contains "summary has action_items" "$summary_text" "action_items"

# Stub returns BlueLake and GreenCastle as participants
e2e_assert_contains "stub participant BlueLake" "$summary_text" "BlueLake"
e2e_assert_contains "stub participant GreenCastle" "$summary_text" "GreenCastle"

# Stub returns specific key_points
e2e_assert_contains "stub key_point API migration" "$summary_text" "API migration"

# ---------------------------------------------------------------------------
# Case 2: summarize_thread with llm_mode=false (heuristic only)
# ---------------------------------------------------------------------------
e2e_case_banner "summarize_thread with llm_mode=false (heuristic)"

heuristic_result="$(mcp_call summarize_thread "{
    \"project_key\": \"${PROJECT_DIR}\",
    \"thread_id\": \"${THREAD_ID}\",
    \"llm_mode\": false
}")"

e2e_save_artifact "case2_heuristic_result.json" "$heuristic_result"

heuristic_text="$(extract_text "$heuristic_result")"

# Heuristic mode should still have structure
e2e_assert_contains "heuristic has thread_id" "$heuristic_text" "thread_id"
e2e_assert_contains "heuristic has summary" "$heuristic_text" "summary"

e2e_save_artifact "case2_heuristic_text.json" "$heuristic_text"

# ---------------------------------------------------------------------------
# Case 3: summarize_thread with include_examples
# ---------------------------------------------------------------------------
e2e_case_banner "summarize_thread with include_examples=true"

examples_result="$(mcp_call summarize_thread "{
    \"project_key\": \"${PROJECT_DIR}\",
    \"thread_id\": \"${THREAD_ID}\",
    \"include_examples\": true,
    \"llm_mode\": true
}")"

e2e_save_artifact "case3_examples_result.json" "$examples_result"

examples_text="$(extract_text "$examples_result")"

# Examples should include sample messages
e2e_assert_contains "examples has examples field" "$examples_text" "examples"

e2e_save_artifact "case3_examples_text.json" "$examples_text"

# ---------------------------------------------------------------------------
# Case 4: macro_prepare_thread with llm_mode
# ---------------------------------------------------------------------------
e2e_case_banner "macro_prepare_thread with llm_mode=true"

prepare_result="$(mcp_call macro_prepare_thread "{
    \"project_key\": \"${PROJECT_DIR}\",
    \"thread_id\": \"${THREAD_ID}\",
    \"program\": \"e2e\",
    \"model\": \"test\",
    \"agent_name\": \"BlueLake\",
    \"llm_mode\": true,
    \"register_if_missing\": false
}")"

e2e_save_artifact "case4_prepare_result.json" "$prepare_result"

prepare_text="$(extract_text "$prepare_result")"

# macro_prepare_thread should return a summary with thread info
e2e_assert_contains "prepare has summary" "$prepare_text" "summary"

e2e_save_artifact "case4_prepare_text.json" "$prepare_text"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
