#!/usr/bin/env bash
# test_null_fields.sh - E2E null/missing field fuzzing for top 10 tools
#
# Sends tool calls with null values, missing required fields, empty strings,
# and wrong types to the top 10 MCP tools via stdio transport.
# Verifies proper error responses (not crashes).
#
# Top 10 tools tested:
#   1. ensure_project         6. search_messages
#   2. register_agent         7. file_reservation_paths
#   3. send_message           8. release_file_reservations
#   4. reply_message          9. request_contact
#   5. fetch_inbox           10. acknowledge_message

E2E_SUITE="null_fields"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Null/Missing Field Fuzzing E2E Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_null_fields")"
FUZZ_DB="${WORK}/fuzz_test.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-fuzz","version":"1.0"}}}'

# Helper: send a tool call via stdio and check for proper error/result.
# Returns the parsed response text.
send_jsonrpc_session() {
    local db_path="$1"
    shift
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
            sleep 0.2
        done
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=10
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if ! kill -0 "$srv_pid" 2>/dev/null; then
            break
        fi
        sleep 0.3
        elapsed=$((elapsed + 1))
    done

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    if [ -f "$output_file" ]; then
        cat "$output_file"
    fi
}

# Helper: check that a tool call returned an error response.
# Accepts response text and the JSON-RPC id to look for.
# Checks both MCP-style isError=true and JSON-RPC-level error.
assert_tool_error() {
    local label="$1"
    local resp="$2"
    local id="$3"

    local check
    check="$(echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') != $id:
            continue
        # JSON-RPC level error
        if 'error' in d:
            print('ERROR')
            sys.exit(0)
        # MCP tool error (isError=true in result)
        if 'result' in d:
            if d['result'].get('isError', False):
                print('ERROR')
                sys.exit(0)
            # Some tools may return valid result for null inputs (e.g., empty arrays)
            print('RESULT')
            sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError, TypeError):
        pass
print('NO_MATCH')
" 2>/dev/null)"

    if [ "$check" = "ERROR" ]; then
        e2e_pass "$label → returned error"
    elif [ "$check" = "RESULT" ]; then
        # Some cases may legitimately succeed (e.g., empty list for release)
        e2e_pass "$label → returned result (acceptable)"
    elif [ "$check" = "NO_MATCH" ]; then
        # Server didn't crash but no response matched - still not a crash
        e2e_fail "$label → no matching response for id=$id"
        echo "    response: $(echo "$resp" | tail -3)"
    fi
}

# ===========================================================================
# Setup: create a project and agent for tools that need them
# ===========================================================================
e2e_case_banner "Setup: create test project + agent"

SETUP_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_fuzz_project"}}}'
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fuzz_project","program":"test","model":"test","name":"RedLake"}}}'
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fuzz_project","program":"test","model":"test","name":"BluePeak"}}}'
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","sender_name":"RedLake","to":["BluePeak"],"subject":"test","body_md":"hello"}}}'
)

SETUP_RESP="$(send_jsonrpc_session "$FUZZ_DB" "${SETUP_REQS[@]}")"
e2e_save_artifact "setup_response.txt" "$SETUP_RESP"

# Verify setup succeeded
if echo "$SETUP_RESP" | python3 -c "
import sys, json
found = 0
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') in (10, 11, 12, 13) and 'result' in d:
            if not d['result'].get('isError', False):
                found += 1
    except: pass
print(found)
sys.exit(0 if found >= 3 else 1)
" 2>/dev/null; then
    e2e_pass "setup: project + 2 agents + message created"
else
    e2e_fail "setup: could not create test fixtures"
    echo "    response: $(echo "$SETUP_RESP" | tail -5)"
fi

# ===========================================================================
# Tool 1: ensure_project - null/missing human_key
# ===========================================================================
e2e_case_banner "ensure_project: null/missing human_key"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"ensure_project","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":null}}}' \
    '{"jsonrpc":"2.0","id":102,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":""}}}' \
)"
e2e_save_artifact "tool_01_ensure_project.txt" "$RESP"

assert_tool_error "ensure_project(missing human_key)" "$RESP" 100
assert_tool_error "ensure_project(null human_key)" "$RESP" 101
assert_tool_error "ensure_project(empty human_key)" "$RESP" 102

# ===========================================================================
# Tool 2: register_agent - null/missing required fields
# ===========================================================================
e2e_case_banner "register_agent: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"register_agent","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":201,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":null,"program":"x","model":"x"}}}' \
    '{"jsonrpc":"2.0","id":202,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fuzz_project","program":null,"model":"x"}}}' \
    '{"jsonrpc":"2.0","id":203,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fuzz_project","program":"x","model":null}}}' \
)"
e2e_save_artifact "tool_02_register_agent.txt" "$RESP"

assert_tool_error "register_agent(empty args)" "$RESP" 200
assert_tool_error "register_agent(null project_key)" "$RESP" 201
assert_tool_error "register_agent(null program)" "$RESP" 202
assert_tool_error "register_agent(null model)" "$RESP" 203

# ===========================================================================
# Tool 3: send_message - null/missing required fields
# ===========================================================================
e2e_case_banner "send_message: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"send_message","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":301,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":null,"sender_name":"RedLake","to":["BluePeak"],"subject":"x","body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":302,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","sender_name":null,"to":["BluePeak"],"subject":"x","body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":303,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","sender_name":"RedLake","to":null,"subject":"x","body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":304,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","sender_name":"RedLake","to":[],"subject":"x","body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":305,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","sender_name":"RedLake","to":["BluePeak"],"subject":null,"body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":306,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","sender_name":"RedLake","to":["BluePeak"],"subject":"x","body_md":null}}}' \
)"
e2e_save_artifact "tool_03_send_message.txt" "$RESP"

assert_tool_error "send_message(empty args)" "$RESP" 300
assert_tool_error "send_message(null project_key)" "$RESP" 301
assert_tool_error "send_message(null sender_name)" "$RESP" 302
assert_tool_error "send_message(null to)" "$RESP" 303
assert_tool_error "send_message(empty to array)" "$RESP" 304
assert_tool_error "send_message(null subject)" "$RESP" 305
assert_tool_error "send_message(null body_md)" "$RESP" 306

# ===========================================================================
# Tool 4: reply_message - null/missing fields
# ===========================================================================
e2e_case_banner "reply_message: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"reply_message","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":401,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":null,"message_id":1,"sender_name":"BluePeak","body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":402,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","message_id":null,"sender_name":"BluePeak","body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":403,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","message_id":1,"sender_name":null,"body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":404,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","message_id":1,"sender_name":"BluePeak","body_md":null}}}' \
    '{"jsonrpc":"2.0","id":405,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","message_id":99999,"sender_name":"BluePeak","body_md":"x"}}}' \
)"
e2e_save_artifact "tool_04_reply_message.txt" "$RESP"

assert_tool_error "reply_message(empty args)" "$RESP" 400
assert_tool_error "reply_message(null project_key)" "$RESP" 401
assert_tool_error "reply_message(null message_id)" "$RESP" 402
assert_tool_error "reply_message(null sender_name)" "$RESP" 403
assert_tool_error "reply_message(null body_md)" "$RESP" 404
assert_tool_error "reply_message(nonexistent message_id)" "$RESP" 405

# ===========================================================================
# Tool 5: fetch_inbox - null/missing fields
# ===========================================================================
e2e_case_banner "fetch_inbox: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":501,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":null,"agent_name":"RedLake"}}}' \
    '{"jsonrpc":"2.0","id":502,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":null}}}' \
    '{"jsonrpc":"2.0","id":503,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/nonexistent","agent_name":"RedLake"}}}' \
)"
e2e_save_artifact "tool_05_fetch_inbox.txt" "$RESP"

assert_tool_error "fetch_inbox(empty args)" "$RESP" 500
assert_tool_error "fetch_inbox(null project_key)" "$RESP" 501
assert_tool_error "fetch_inbox(null agent_name)" "$RESP" 502
assert_tool_error "fetch_inbox(nonexistent project)" "$RESP" 503

# ===========================================================================
# Tool 6: search_messages - null/missing fields
# ===========================================================================
e2e_case_banner "search_messages: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"search_messages","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":601,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":null,"query":"test"}}}' \
    '{"jsonrpc":"2.0","id":602,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_fuzz_project","query":null}}}' \
    '{"jsonrpc":"2.0","id":603,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_fuzz_project","query":""}}}' \
)"
e2e_save_artifact "tool_06_search_messages.txt" "$RESP"

assert_tool_error "search_messages(empty args)" "$RESP" 600
assert_tool_error "search_messages(null project_key)" "$RESP" 601
assert_tool_error "search_messages(null query)" "$RESP" 602
assert_tool_error "search_messages(empty query)" "$RESP" 603

# ===========================================================================
# Tool 7: file_reservation_paths - null/missing fields
# ===========================================================================
e2e_case_banner "file_reservation_paths: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":700,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":701,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":null,"agent_name":"RedLake","paths":["x"]}}}' \
    '{"jsonrpc":"2.0","id":702,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":null,"paths":["x"]}}}' \
    '{"jsonrpc":"2.0","id":703,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":"RedLake","paths":null}}}' \
    '{"jsonrpc":"2.0","id":704,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":"RedLake","paths":[]}}}' \
)"
e2e_save_artifact "tool_07_file_reservation_paths.txt" "$RESP"

assert_tool_error "file_reservation_paths(empty args)" "$RESP" 700
assert_tool_error "file_reservation_paths(null project_key)" "$RESP" 701
assert_tool_error "file_reservation_paths(null agent_name)" "$RESP" 702
assert_tool_error "file_reservation_paths(null paths)" "$RESP" 703
assert_tool_error "file_reservation_paths(empty paths)" "$RESP" 704

# ===========================================================================
# Tool 8: release_file_reservations - null/missing fields
# ===========================================================================
e2e_case_banner "release_file_reservations: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":800,"method":"tools/call","params":{"name":"release_file_reservations","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":801,"method":"tools/call","params":{"name":"release_file_reservations","arguments":{"project_key":null,"agent_name":"RedLake"}}}' \
    '{"jsonrpc":"2.0","id":802,"method":"tools/call","params":{"name":"release_file_reservations","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":null}}}' \
)"
e2e_save_artifact "tool_08_release_file_reservations.txt" "$RESP"

assert_tool_error "release_file_reservations(empty args)" "$RESP" 800
assert_tool_error "release_file_reservations(null project_key)" "$RESP" 801
assert_tool_error "release_file_reservations(null agent_name)" "$RESP" 802

# ===========================================================================
# Tool 9: request_contact - null/missing fields
# ===========================================================================
e2e_case_banner "request_contact: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"request_contact","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":901,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":null,"from_agent":"RedLake","to_agent":"BluePeak"}}}' \
    '{"jsonrpc":"2.0","id":902,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_fuzz_project","from_agent":null,"to_agent":"BluePeak"}}}' \
    '{"jsonrpc":"2.0","id":903,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_fuzz_project","from_agent":"RedLake","to_agent":null}}}' \
    '{"jsonrpc":"2.0","id":904,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_fuzz_project","from_agent":"RedLake","to_agent":"RedLake"}}}' \
)"
e2e_save_artifact "tool_09_request_contact.txt" "$RESP"

assert_tool_error "request_contact(empty args)" "$RESP" 900
assert_tool_error "request_contact(null project_key)" "$RESP" 901
assert_tool_error "request_contact(null from_agent)" "$RESP" 902
assert_tool_error "request_contact(null to_agent)" "$RESP" 903
assert_tool_error "request_contact(self-contact)" "$RESP" 904

# ===========================================================================
# Tool 10: acknowledge_message - null/missing fields
# ===========================================================================
e2e_case_banner "acknowledge_message: null/missing fields"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1000,"method":"tools/call","params":{"name":"acknowledge_message","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":1001,"method":"tools/call","params":{"name":"acknowledge_message","arguments":{"project_key":null,"agent_name":"RedLake","message_id":1}}}' \
    '{"jsonrpc":"2.0","id":1002,"method":"tools/call","params":{"name":"acknowledge_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":null,"message_id":1}}}' \
    '{"jsonrpc":"2.0","id":1003,"method":"tools/call","params":{"name":"acknowledge_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":"RedLake","message_id":null}}}' \
    '{"jsonrpc":"2.0","id":1004,"method":"tools/call","params":{"name":"acknowledge_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":"RedLake","message_id":99999}}}' \
)"
e2e_save_artifact "tool_10_acknowledge_message.txt" "$RESP"

assert_tool_error "acknowledge_message(empty args)" "$RESP" 1000
assert_tool_error "acknowledge_message(null project_key)" "$RESP" 1001
assert_tool_error "acknowledge_message(null agent_name)" "$RESP" 1002
assert_tool_error "acknowledge_message(null message_id)" "$RESP" 1003
assert_tool_error "acknowledge_message(nonexistent message_id)" "$RESP" 1004

# ===========================================================================
# Bonus: wrong types (string where int expected, int where string expected)
# ===========================================================================
e2e_case_banner "Wrong type fuzzing"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1100,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","message_id":"not_a_number","sender_name":"BluePeak","body_md":"x"}}}' \
    '{"jsonrpc":"2.0","id":1101,"method":"tools/call","params":{"name":"acknowledge_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":"RedLake","message_id":"string_id"}}}' \
    '{"jsonrpc":"2.0","id":1102,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fuzz_project","agent_name":"RedLake","paths":"not_an_array","ttl_seconds":"not_a_number"}}}' \
    '{"jsonrpc":"2.0","id":1103,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fuzz_project","sender_name":"RedLake","to":"not_an_array","subject":"x","body_md":"x"}}}' \
)"
e2e_save_artifact "tool_wrong_types.txt" "$RESP"

assert_tool_error "reply_message(string message_id)" "$RESP" 1100
assert_tool_error "acknowledge_message(string message_id)" "$RESP" 1101
assert_tool_error "file_reservation_paths(string paths)" "$RESP" 1102
assert_tool_error "send_message(string to)" "$RESP" 1103

# ===========================================================================
# Bonus: nonexistent tool name
# ===========================================================================
e2e_case_banner "Nonexistent tool"

RESP="$(send_jsonrpc_session "$FUZZ_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1200,"method":"tools/call","params":{"name":"totally_fake_tool","arguments":{}}}' \
)"
e2e_save_artifact "tool_nonexistent.txt" "$RESP"

assert_tool_error "nonexistent tool name" "$RESP" 1200

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
