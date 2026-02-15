#!/usr/bin/env bash
# test_doc_parity.sh - E2E: Documentation & Error Message Parity
#
# Validates that user-facing text (error messages, descriptions, validation
# messages) matches the Python reference through the actual MCP stdio protocol.
#
# This is the "final gate" ensuring an agent interfacing through MCP cannot
# tell whether it is talking to the Python or Rust implementation.

E2E_SUITE="doc_parity"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Documentation & Error Message Parity E2E Test Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_doc_parity")"
DOC_DB="${WORK}/doc_parity.sqlite3"
PROJECT_PATH="/tmp/e2e_doc_parity_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-doc-parity","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"; shift
    local requests=("$@")
    local output_file="${WORK}/session_response_$$.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.3
        done
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=25
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if ! kill -0 "$srv_pid" 2>/dev/null; then break; fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    [ -f "$output_file" ] && cat "$output_file"
}

# Extract text content from a successful tool result
extract_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $req_id and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                print(content[0].get('text', ''))
                sys.exit(0)
    except Exception:
        pass
" 2>/dev/null
}

# Check if a response is an error (isError:true or JSON-RPC error)
is_error_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $req_id:
            if 'error' in d:
                print('true')
                sys.exit(0)
            if 'result' in d and d['result'].get('isError', False):
                print('true')
                sys.exit(0)
    except Exception:
        pass
print('false')
" 2>/dev/null
}

# Extract error text content (plain text from isError response)
extract_error_text() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $req_id:
            if 'error' in d:
                print(d['error'].get('message', ''))
                sys.exit(0)
            if 'result' in d and d['result'].get('isError', False):
                content = d['result'].get('content', [])
                if content:
                    print(content[0].get('text', ''))
                    sys.exit(0)
    except Exception:
        pass
" 2>/dev/null
}

# Extract tool description from tools/list
extract_tool_description() {
    local response="$1"
    local tool_name="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if 'result' in d and 'tools' in d['result']:
            for tool in d['result']['tools']:
                if tool.get('name') == '$tool_name':
                    print(tool.get('description', ''))
                    sys.exit(0)
    except Exception:
        pass
" 2>/dev/null
}

# Count tools from tools/list
extract_tool_count() {
    local response="$1"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if 'result' in d and 'tools' in d['result']:
            print(len(d['result']['tools']))
            sys.exit(0)
    except Exception:
        pass
print('0')
" 2>/dev/null
}

# Count resources from resources/list
extract_resource_count() {
    local response="$1"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if 'result' in d and 'resources' in d['result']:
            print(len(d['result']['resources']))
            sys.exit(0)
    except Exception:
        pass
print('0')
" 2>/dev/null
}

# Extract resource text content
extract_resource_text() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $req_id and 'result' in d and 'contents' in d['result']:
            for c in d['result']['contents']:
                if 'text' in c:
                    print(c['text'])
                    sys.exit(0)
    except Exception:
        pass
" 2>/dev/null
}

# Parse a JSON field from text
parse_json_field() {
    local text="$1"
    local field="$2"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    val = d
    for key in '$field'.split('.'):
        if isinstance(val, dict):
            val = val.get(key, '')
        elif isinstance(val, list) and key.isdigit():
            val = val[int(key)]
        else:
            val = ''
            break
    print(val if val is not None else '')
except Exception:
    print('')
" 2>/dev/null
}

# ===========================================================================
# Setup: Create project + register agents
# ===========================================================================
e2e_case_banner "Setup: project + two agents"

SETUP_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"BlueLake\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"RedPeak\"}}}" \
)"
e2e_save_artifact "setup.txt" "$SETUP_RESP"

BL_ERR="$(is_error_result "$SETUP_RESP" 11)"
RP_ERR="$(is_error_result "$SETUP_RESP" 12)"
if [ "$BL_ERR" = "false" ] && [ "$RP_ERR" = "false" ]; then
    e2e_pass "setup: project + 2 agents registered"
else
    e2e_fail "setup: agent registration failed" "BL=$BL_ERR RP=$RP_ERR" "both false"
fi

# ===========================================================================
# 1. Tool Description Parity (6 assertions)
# ===========================================================================
e2e_case_banner "Tool Description Parity"

TOOLS_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":20,"method":"tools/list","params":{}}' \
)"
e2e_save_artifact "tools_list.txt" "$TOOLS_RESP"

# 1a. Tool count
TOOL_COUNT="$(extract_tool_count "$TOOLS_RESP")"
if [ "$TOOL_COUNT" -ge 34 ]; then
    e2e_pass "tool_count: at least 34 tools registered ($TOOL_COUNT)"
else
    e2e_fail "tool_count: expected >=34 tools" "$TOOL_COUNT" ">=34"
fi

# 1b. send_message description
SM_DESC="$(extract_tool_description "$TOOLS_RESP" "send_message")"
if echo "$SM_DESC" | grep -qF "Send a Markdown message to one or more recipients and persist canonical and mailbox copies to Git."; then
    e2e_pass "send_message description prefix"
else
    e2e_fail "send_message description prefix" "${SM_DESC:0:120}" "Send a Markdown message..."
fi

# 1c. health_check description
HC_DESC="$(extract_tool_description "$TOOLS_RESP" "health_check")"
if echo "$HC_DESC" | grep -qF "Return basic readiness information"; then
    e2e_pass "health_check description prefix"
else
    e2e_fail "health_check description prefix" "${HC_DESC:0:120}" "Return basic readiness..."
fi

# 1d. ensure_project description
EP_DESC="$(extract_tool_description "$TOOLS_RESP" "ensure_project")"
if echo "$EP_DESC" | grep -qF "Idempotently create or ensure a project exists for the given human key."; then
    e2e_pass "ensure_project description prefix"
else
    e2e_fail "ensure_project description prefix" "${EP_DESC:0:120}" "Idempotently create or ensure..."
fi

# 1e. register_agent description
RA_DESC="$(extract_tool_description "$TOOLS_RESP" "register_agent")"
if echo "$RA_DESC" | grep -qF "Create or update an agent identity within a project"; then
    e2e_pass "register_agent description prefix"
else
    e2e_fail "register_agent description prefix" "${RA_DESC:0:120}" "Create or update an agent..."
fi

# 1f. fetch_inbox description
FI_DESC="$(extract_tool_description "$TOOLS_RESP" "fetch_inbox")"
if echo "$FI_DESC" | grep -qF "Retrieve recent messages for an agent without mutating"; then
    e2e_pass "fetch_inbox description prefix"
else
    e2e_fail "fetch_inbox description prefix" "${FI_DESC:0:120}" "Retrieve recent messages..."
fi

# ===========================================================================
# 2. Error Message Parity (10 assertions)
# ===========================================================================
e2e_case_banner "Error Message Parity"

# 2a. Empty project_key → error with "absolute directory path"
ERR1_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":""}}}' \
)"
e2e_save_artifact "err_empty_project.txt" "$ERR1_RESP"
ERR1_IS="$(is_error_result "$ERR1_RESP" 30)"
ERR1_TEXT="$(extract_error_text "$ERR1_RESP" 30)"
if [ "$ERR1_IS" = "true" ] && echo "$ERR1_TEXT" | grep -qF "absolute directory path"; then
    e2e_pass "empty project_key: error with correct message"
else
    e2e_fail "empty project_key error" "is_err=$ERR1_IS text=${ERR1_TEXT:0:80}" "error with 'absolute directory path'"
fi

# 2b. Placeholder project_key → error with "absolute directory path"
ERR2_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"YOUR_PROJECT"}}}' \
)"
e2e_save_artifact "err_placeholder_project.txt" "$ERR2_RESP"
ERR2_IS="$(is_error_result "$ERR2_RESP" 31)"
ERR2_TEXT="$(extract_error_text "$ERR2_RESP" 31)"
if [ "$ERR2_IS" = "true" ] && echo "$ERR2_TEXT" | grep -qF "absolute directory path"; then
    e2e_pass "placeholder project_key: error with path hint"
else
    e2e_fail "placeholder project_key" "is_err=$ERR2_IS text=${ERR2_TEXT:0:80}" "error with 'absolute directory path'"
fi

# 2c. Program name as agent → "program name" hint
ERR3_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"codex-cli\",\"model\":\"gpt-5\",\"name\":\"codex-cli\"}}}" \
)"
e2e_save_artifact "err_program_as_agent.txt" "$ERR3_RESP"
ERR3_IS="$(is_error_result "$ERR3_RESP" 32)"
ERR3_TEXT="$(extract_error_text "$ERR3_RESP" 32)"
if [ "$ERR3_IS" = "true" ] && echo "$ERR3_TEXT" | grep -qF "program name"; then
    e2e_pass "program-as-agent: hint mentions 'program name'"
else
    e2e_fail "program-as-agent" "is_err=$ERR3_IS text=${ERR3_TEXT:0:80}" "error with 'program name'"
fi

# 2d. Model name as agent → "model name" hint
ERR4_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":33,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"codex-cli\",\"model\":\"gpt-5\",\"name\":\"gpt-5\"}}}" \
)"
e2e_save_artifact "err_model_as_agent.txt" "$ERR4_RESP"
ERR4_IS="$(is_error_result "$ERR4_RESP" 33)"
ERR4_TEXT="$(extract_error_text "$ERR4_RESP" 33)"
if [ "$ERR4_IS" = "true" ] && echo "$ERR4_TEXT" | grep -qF "model name"; then
    e2e_pass "model-as-agent: hint mentions 'model name'"
else
    e2e_fail "model-as-agent" "is_err=$ERR4_IS text=${ERR4_TEXT:0:80}" "error with 'model name'"
fi

# 2e. send_message with empty recipients → error mentioning "recipient"
ERR5_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":34,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"BlueLake\",\"to\":[],\"subject\":\"test\",\"body_md\":\"test\"}}}" \
)"
e2e_save_artifact "err_empty_recipients.txt" "$ERR5_RESP"
ERR5_IS="$(is_error_result "$ERR5_RESP" 34)"
ERR5_TEXT="$(extract_error_text "$ERR5_RESP" 34)"
if [ "$ERR5_IS" = "true" ] && echo "$ERR5_TEXT" | grep -qiF "recipient"; then
    e2e_pass "empty recipients: error mentions 'recipient'"
else
    e2e_fail "empty recipients" "is_err=$ERR5_IS text=${ERR5_TEXT:0:80}" "error with 'recipient'"
fi

# 2f. Invalid importance → error mentioning "importance"
ERR6_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":35,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"test\",\"body_md\":\"test\",\"importance\":\"invalid_level\"}}}" \
)"
e2e_save_artifact "err_invalid_importance.txt" "$ERR6_RESP"
ERR6_IS="$(is_error_result "$ERR6_RESP" 35)"
ERR6_TEXT="$(extract_error_text "$ERR6_RESP" 35)"
if [ "$ERR6_IS" = "true" ] && echo "$ERR6_TEXT" | grep -qiF "importance"; then
    e2e_pass "invalid importance: error mentions 'importance'"
else
    e2e_fail "invalid importance" "is_err=$ERR6_IS text=${ERR6_TEXT:0:80}" "error with 'importance'"
fi

# 2g. Unregistered sender → error
ERR7_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":36,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldRiver\",\"to\":[\"RedPeak\"],\"subject\":\"test\",\"body_md\":\"test\"}}}" \
)"
e2e_save_artifact "err_unregistered_sender.txt" "$ERR7_RESP"
ERR7_IS="$(is_error_result "$ERR7_RESP" 36)"
ERR7_TEXT="$(extract_error_text "$ERR7_RESP" 36)"
if [ "$ERR7_IS" = "true" ]; then
    e2e_pass "unregistered sender: error returned"
else
    e2e_fail "unregistered sender" "is_err=$ERR7_IS text=${ERR7_TEXT:0:120}" "error expected"
fi

# 2h. reply_message to nonexistent message → error
ERR8_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":37,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":999999,\"sender_name\":\"BlueLake\",\"body_md\":\"reply\"}}}" \
)"
e2e_save_artifact "err_reply_not_found.txt" "$ERR8_RESP"
ERR8_IS="$(is_error_result "$ERR8_RESP" 37)"
ERR8_TEXT="$(extract_error_text "$ERR8_RESP" 37)"
if [ "$ERR8_IS" = "true" ] && echo "$ERR8_TEXT" | grep -qiF "not found"; then
    e2e_pass "reply to missing message: 'not found' error"
else
    e2e_fail "reply not found" "is_err=$ERR8_IS text=${ERR8_TEXT:0:80}" "error with 'not found'"
fi

# 2i. Invalid thread_id (path traversal) → error
ERR9_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":38,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"test\",\"body_md\":\"test\",\"thread_id\":\"../escape\"}}}" \
)"
e2e_save_artifact "err_invalid_thread.txt" "$ERR9_RESP"
ERR9_IS="$(is_error_result "$ERR9_RESP" 38)"
if [ "$ERR9_IS" = "true" ]; then
    e2e_pass "invalid thread_id: error returned"
else
    e2e_fail "invalid thread_id" "is_err=$ERR9_IS" "error expected"
fi

# 2j. Invalid agent name → error with adjective+noun hint
ERR10_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":39,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"invalid name 123\"}}}" \
)"
e2e_save_artifact "err_invalid_agent_name.txt" "$ERR10_RESP"
ERR10_IS="$(is_error_result "$ERR10_RESP" 39)"
ERR10_TEXT="$(extract_error_text "$ERR10_RESP" 39)"
if [ "$ERR10_IS" = "true" ] && echo "$ERR10_TEXT" | grep -qF "adjective+noun"; then
    e2e_pass "invalid agent name: mentions adjective+noun"
else
    e2e_fail "invalid agent name" "is_err=$ERR10_IS text=${ERR10_TEXT:0:80}" "error with 'adjective+noun'"
fi

# ===========================================================================
# 3. Resource Description Parity (4 assertions)
# ===========================================================================
e2e_case_banner "Resource Description Parity"

RESOURCES_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":40,"method":"resources/list","params":{}}' \
)"
e2e_save_artifact "resources_list.txt" "$RESOURCES_RESP"

RESOURCE_COUNT="$(extract_resource_count "$RESOURCES_RESP")"
if [ "$RESOURCE_COUNT" -ge 7 ]; then
    e2e_pass "resource_count: at least 7 static resources ($RESOURCE_COUNT)"
else
    e2e_fail "resource_count" "$RESOURCE_COUNT" ">=7"
fi

# 3a. tooling/directory has correct cluster count
TOOLDIR_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":41,"method":"resources/read","params":{"uri":"resource://tooling/directory"}}' \
)"
e2e_save_artifact "tooling_directory.txt" "$TOOLDIR_RESP"
TOOLDIR_TEXT="$(extract_resource_text "$TOOLDIR_RESP" 41)"
CLUSTER_COUNT="$(echo "$TOOLDIR_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(len(d.get('clusters', [])))
except Exception:
    print(0)
" 2>/dev/null)"
if [ "$CLUSTER_COUNT" -ge 7 ]; then
    e2e_pass "tooling/directory: at least 7 clusters ($CLUSTER_COUNT)"
else
    e2e_fail "tooling/directory clusters" "$CLUSTER_COUNT" ">=7"
fi

# 3b. tooling/schemas has generated_at + tools
SCHEMAS_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":42,"method":"resources/read","params":{"uri":"resource://tooling/schemas"}}' \
)"
e2e_save_artifact "tooling_schemas.txt" "$SCHEMAS_RESP"
SCHEMAS_TEXT="$(extract_resource_text "$SCHEMAS_RESP" 42)"
HAS_STRUCT="$(echo "$SCHEMAS_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print('true' if 'generated_at' in d and 'tools' in d else 'false')
except Exception:
    print('false')
" 2>/dev/null)"
if [ "$HAS_STRUCT" = "true" ]; then
    e2e_pass "tooling/schemas: has generated_at + tools"
else
    e2e_fail "tooling/schemas structure" "$HAS_STRUCT" "true"
fi

# ===========================================================================
# 4. Validation Message Parity (5 assertions)
# ===========================================================================
e2e_case_banner "Validation Message Parity"

# 4a. Empty program → error mentioning "program"
VAL1_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"\",\"model\":\"test-model\"}}}" \
)"
e2e_save_artifact "val_empty_program.txt" "$VAL1_RESP"
VAL1_IS="$(is_error_result "$VAL1_RESP" 50)"
VAL1_TEXT="$(extract_error_text "$VAL1_RESP" 50)"
if [ "$VAL1_IS" = "true" ] && echo "$VAL1_TEXT" | grep -qiF "program"; then
    e2e_pass "empty program: error mentions 'program'"
else
    e2e_fail "empty program" "is_err=$VAL1_IS text=${VAL1_TEXT:0:80}" "error with 'program'"
fi

# 4b. Empty model → error mentioning "model"
VAL2_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"test\",\"model\":\"\"}}}" \
)"
e2e_save_artifact "val_empty_model.txt" "$VAL2_RESP"
VAL2_IS="$(is_error_result "$VAL2_RESP" 51)"
VAL2_TEXT="$(extract_error_text "$VAL2_RESP" 51)"
if [ "$VAL2_IS" = "true" ] && echo "$VAL2_TEXT" | grep -qiF "model"; then
    e2e_pass "empty model: error mentions 'model'"
else
    e2e_fail "empty model" "is_err=$VAL2_IS text=${VAL2_TEXT:0:80}" "error with 'model'"
fi

# 4c. fetch_inbox with negative limit → error with "limit"
VAL3_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":52,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"limit\":-1}}}" \
)"
e2e_save_artifact "val_invalid_limit.txt" "$VAL3_RESP"
VAL3_IS="$(is_error_result "$VAL3_RESP" 52)"
VAL3_TEXT="$(extract_error_text "$VAL3_RESP" 52)"
if [ "$VAL3_IS" = "true" ] && echo "$VAL3_TEXT" | grep -qiF "limit"; then
    e2e_pass "negative limit: error mentions 'limit'"
else
    e2e_fail "negative limit" "is_err=$VAL3_IS text=${VAL3_TEXT:0:80}" "error with 'limit'"
fi

# 4d. file_reservation_paths with empty paths → error with "paths"
VAL4_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":53,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"paths\":[]}}}" \
)"
e2e_save_artifact "val_empty_paths.txt" "$VAL4_RESP"
VAL4_IS="$(is_error_result "$VAL4_RESP" 53)"
VAL4_TEXT="$(extract_error_text "$VAL4_RESP" 53)"
if [ "$VAL4_IS" = "true" ] && echo "$VAL4_TEXT" | grep -qiF "path"; then
    e2e_pass "empty paths: error mentions 'path'"
else
    e2e_fail "empty paths" "is_err=$VAL4_IS text=${VAL4_TEXT:0:80}" "error with 'path'"
fi

# 4e. set_contact_policy with invalid policy → coerces to "auto"
VAL5_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":54,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"policy\":\"bogus_policy\"}}}" \
)"
e2e_save_artifact "val_invalid_policy.txt" "$VAL5_RESP"
VAL5_IS="$(is_error_result "$VAL5_RESP" 54)"
VAL5_TEXT="$(extract_result "$VAL5_RESP" 54)"
if [ "$VAL5_IS" = "false" ] && echo "$VAL5_TEXT" | grep -qF '"policy":"auto"'; then
    e2e_pass "invalid policy: silently coerced to 'auto'"
else
    e2e_fail "invalid policy coercion" "is_err=$VAL5_IS text=${VAL5_TEXT:0:80}" "non-error with policy=auto"
fi

# ===========================================================================
# 5. Contact Policy Error Parity (2 assertions)
# ===========================================================================
e2e_case_banner "Contact Policy Error Parity"

# Set BlueLake to block_all
send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"policy\":\"block_all\"}}}" \
    > /dev/null

# Try to send to blocked agent
CERR_RESP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedPeak\",\"to\":[\"BlueLake\"],\"subject\":\"test\",\"body_md\":\"test\"}}}" \
)"
e2e_save_artifact "err_contact_blocked.txt" "$CERR_RESP"
CERR_IS="$(is_error_result "$CERR_RESP" 61)"
CERR_TEXT="$(extract_error_text "$CERR_RESP" 61)"
if [ "$CERR_IS" = "true" ] && echo "$CERR_TEXT" | grep -qiF "not accepting"; then
    e2e_pass "send to blocked: error mentions 'not accepting'"
else
    e2e_fail "send to blocked" "is_err=$CERR_IS text=${CERR_TEXT:0:80}" "error with 'not accepting'"
fi

# Verify error mentions the recipient
if echo "$CERR_TEXT" | grep -qiF "recipient"; then
    e2e_pass "contact error: mentions recipient"
else
    e2e_fail "contact error remedy" "${CERR_TEXT:0:120}" "should mention 'recipient'"
fi

# ===========================================================================
# 6. Reply Subject Prefix Parity (2 assertions)
# ===========================================================================
e2e_case_banner "Reply Subject Prefix Parity"

# Reset policy and send a message
REPLY_SETUP="$(send_jsonrpc_session "$DOC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"policy\":\"open\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":71,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Original Subject\",\"body_md\":\"Hello\"}}}" \
)"
e2e_save_artifact "reply_setup.txt" "$REPLY_SETUP"

SEND_RESULT="$(extract_result "$REPLY_SETUP" 71)"
MSG_ID="$(echo "$SEND_RESULT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(d['deliveries'][0]['payload']['id'])
except Exception:
    print('0')
" 2>/dev/null)"

if [ "$MSG_ID" != "0" ] && [ -n "$MSG_ID" ]; then
    # Reply
    REPLY_RESP="$(send_jsonrpc_session "$DOC_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":72,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":${MSG_ID},\"sender_name\":\"RedPeak\",\"body_md\":\"Reply body\"}}}" \
    )"
    e2e_save_artifact "reply_response.txt" "$REPLY_RESP"
    REPLY_RESULT="$(extract_result "$REPLY_RESP" 72)"
    REPLY_SUBJECT="$(echo "$REPLY_RESULT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(d['deliveries'][0]['payload']['subject'])
except Exception:
    print('')
" 2>/dev/null)"

    if [ "$REPLY_SUBJECT" = "Re: Original Subject" ]; then
        e2e_pass "reply adds Re: prefix"
    else
        e2e_fail "reply subject prefix" "$REPLY_SUBJECT" "Re: Original Subject"
    fi

    # Reply to the reply — should NOT double Re:
    REPLY_ID="$(echo "$REPLY_RESULT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(d['deliveries'][0]['payload']['id'])
except Exception:
    print('0')
" 2>/dev/null)"

    if [ "$REPLY_ID" != "0" ] && [ -n "$REPLY_ID" ]; then
        REPLY2_RESP="$(send_jsonrpc_session "$DOC_DB" \
            "$INIT_REQ" \
            "{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":${REPLY_ID},\"sender_name\":\"BlueLake\",\"body_md\":\"Second reply\"}}}" \
        )"
        e2e_save_artifact "reply2_response.txt" "$REPLY2_RESP"
        REPLY2_RESULT="$(extract_result "$REPLY2_RESP" 73)"
        REPLY2_SUBJECT="$(echo "$REPLY2_RESULT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(d['deliveries'][0]['payload']['subject'])
except Exception:
    print('')
" 2>/dev/null)"

        if [ "$REPLY2_SUBJECT" = "Re: Original Subject" ]; then
            e2e_pass "reply to reply: no double Re: prefix"
        else
            e2e_fail "double reply subject" "$REPLY2_SUBJECT" "Re: Original Subject"
        fi
    else
        e2e_fail "could not get reply ID" "$REPLY_ID" "non-zero"
    fi
else
    e2e_fail "could not get msg ID for reply test" "$MSG_ID" "non-zero"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
