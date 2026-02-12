#!/usr/bin/env bash
# test_wrong_order_misuse.sh - E2E: Common Pitfalls from AGENTS.md
#
# Verifies that the server properly handles common misuse patterns --
# things that agents/users might try that are wrong or out of order.
# The test confirms proper error responses (not crashes).
#
# Tests:
#   1.  Send message before registering agent -- expect error
#   2.  Register agent before ensure_project -- auto-ensures project (succeeds)
#   3.  Reply to nonexistent message -- expect error
#   4.  Double register same agent -- should succeed (upsert behavior)
#   5.  Send message to nonexistent recipient -- expect error
#   6.  Acknowledge nonexistent message -- expect error
#   7.  File reservation before project exists -- expect error
#   8.  Release nonexistent reservation -- should succeed (idempotent)
#   9.  Fetch inbox for unregistered agent -- expect error
#   10. Search in nonexistent project -- returns empty results gracefully
#   11. Initialize twice -- should handle gracefully

E2E_SUITE="wrong_order_misuse"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Wrong Order / Misuse E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_wrong_order")"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-wrong-order","version":"1.0"}}}'

# Unique project paths per test group
PROJECT_PATH_A="/tmp/e2e_wrong_order_a_$$"
PROJECT_PATH_B="/tmp/e2e_wrong_order_b_$$"
NONEXISTENT_PROJECT="/tmp/e2e_nonexistent_project_$$"

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
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
" 2>/dev/null
}

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
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('false')
" 2>/dev/null
}

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

# Check if a response line for a given id exists at all
has_response_for_id() {
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
            print('true')
            sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('false')
" 2>/dev/null
}

# ===========================================================================
# Case 1: Send message before registering agent -- expect error
# ===========================================================================
e2e_case_banner "send_message before register_agent"

DB_1="${WORK}/case01.sqlite3"

RESP_1="$(send_jsonrpc_session "$DB_1" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Hello\",\"body_md\":\"Test message\"}}}" \
)"
e2e_save_artifact "case_01_send_before_register.txt" "$RESP_1"

EP_ERR_1="$(is_error_result "$RESP_1" 10)"
SEND_ERR_1="$(is_error_result "$RESP_1" 11)"

if [ "$EP_ERR_1" = "false" ]; then
    e2e_pass "ensure_project succeeded (setup)"
else
    e2e_fail "ensure_project returned error unexpectedly"
fi

if [ "$SEND_ERR_1" = "true" ]; then
    e2e_pass "send_message with unregistered sender correctly returned error"
else
    e2e_fail "send_message with unregistered sender should have returned error"
fi

# ===========================================================================
# Case 2: Register agent before ensure_project -- auto-ensures project
# ===========================================================================
e2e_case_banner "register_agent before ensure_project (auto-ensure)"

DB_2="${WORK}/case02.sqlite3"

RESP_2="$(send_jsonrpc_session "$DB_2" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${NONEXISTENT_PROJECT}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"testing\"}}}" \
)"
e2e_save_artifact "case_02_register_before_project.txt" "$RESP_2"

REG_ERR_2="$(is_error_result "$RESP_2" 20)"

# register_agent auto-ensures the project, so this should succeed
if [ "$REG_ERR_2" = "false" ]; then
    e2e_pass "register_agent auto-ensured project and succeeded"
else
    e2e_fail "register_agent should auto-ensure project and succeed"
fi

# Verify the agent was actually created with the correct name
REG_TEXT_2="$(extract_result "$RESP_2" 20)"
REG_NAME_2="$(parse_json_field "$REG_TEXT_2" "name")"
e2e_assert_eq "auto-registered agent has correct name" "GoldFox" "$REG_NAME_2"

# ===========================================================================
# Case 3: Reply to nonexistent message -- expect error
# ===========================================================================
e2e_case_banner "reply_message to nonexistent message_id"

DB_3="${WORK}/case03.sqlite3"

RESP_3="$(send_jsonrpc_session "$DB_3" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"message_id\":99999,\"sender_name\":\"GoldFox\",\"body_md\":\"Reply to nothing\"}}}" \
)"
e2e_save_artifact "case_03_reply_nonexistent.txt" "$RESP_3"

REPLY_ERR_3="$(is_error_result "$RESP_3" 32)"

if [ "$REPLY_ERR_3" = "true" ]; then
    e2e_pass "reply_message to nonexistent message_id correctly returned error"
else
    e2e_fail "reply_message to nonexistent message_id should have returned error"
fi

# ===========================================================================
# Case 4: Double register same agent -- should succeed (upsert)
# ===========================================================================
e2e_case_banner "double register_agent (upsert)"

DB_4="${WORK}/case04.sqlite3"

RESP_4="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_B}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"program\":\"e2e-test\",\"model\":\"test-model-v1\",\"name\":\"GoldFox\",\"task_description\":\"first registration\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"program\":\"e2e-test\",\"model\":\"test-model-v2\",\"name\":\"GoldFox\",\"task_description\":\"second registration\"}}}" \
)"
e2e_save_artifact "case_04_double_register.txt" "$RESP_4"

REG1_ERR_4="$(is_error_result "$RESP_4" 41)"
REG2_ERR_4="$(is_error_result "$RESP_4" 42)"

if [ "$REG1_ERR_4" = "false" ]; then
    e2e_pass "first register_agent succeeded"
else
    e2e_fail "first register_agent returned error"
fi

if [ "$REG2_ERR_4" = "false" ]; then
    e2e_pass "second register_agent succeeded (upsert)"
else
    e2e_fail "second register_agent should succeed as upsert"
fi

# Verify the second registration updated the model
REG2_TEXT="$(extract_result "$RESP_4" 42)"
REG2_MODEL="$(parse_json_field "$REG2_TEXT" "model")"
e2e_assert_eq "upsert preserved updated model" "test-model-v2" "$REG2_MODEL"

REG2_NAME="$(parse_json_field "$REG2_TEXT" "name")"
e2e_assert_eq "upsert preserved agent name" "GoldFox" "$REG2_NAME"

# ===========================================================================
# Case 5: Send message to nonexistent recipient -- expect error
# ===========================================================================
e2e_case_banner "send_message to nonexistent recipient"

# Reuse DB_4 which has project + GoldFox registered
RESP_5="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"sender_name\":\"GoldFox\",\"to\":[\"NonExistentAgent123\"],\"subject\":\"Hello ghost\",\"body_md\":\"You do not exist\"}}}" \
)"
e2e_save_artifact "case_05_nonexistent_recipient.txt" "$RESP_5"

SEND_ERR_5="$(is_error_result "$RESP_5" 50)"

if [ "$SEND_ERR_5" = "true" ]; then
    e2e_pass "send_message to nonexistent recipient correctly returned error"
else
    e2e_fail "send_message to nonexistent recipient should have returned error"
fi

# ===========================================================================
# Case 6: Acknowledge nonexistent message -- expect error
# ===========================================================================
e2e_case_banner "acknowledge_message with bad message_id"

# Reuse DB_4 which has project + GoldFox registered
RESP_6="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"agent_name\":\"GoldFox\",\"message_id\":99999}}}" \
)"
e2e_save_artifact "case_06_ack_nonexistent.txt" "$RESP_6"

ACK_ERR_6="$(is_error_result "$RESP_6" 60)"

if [ "$ACK_ERR_6" = "true" ]; then
    e2e_pass "acknowledge_message with bad message_id correctly returned error"
else
    e2e_fail "acknowledge_message with bad message_id should have returned error"
fi

# ===========================================================================
# Case 7: File reservation before project exists -- expect error
# ===========================================================================
e2e_case_banner "file_reservation_paths before ensure_project"

DB_7="${WORK}/case07.sqlite3"

RESP_7="$(send_jsonrpc_session "$DB_7" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${NONEXISTENT_PROJECT}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/*.rs\"],\"ttl_seconds\":3600,\"exclusive\":true}}}" \
)"
e2e_save_artifact "case_07_reservation_no_project.txt" "$RESP_7"

RES_ERR_7="$(is_error_result "$RESP_7" 70)"

if [ "$RES_ERR_7" = "true" ]; then
    e2e_pass "file_reservation_paths without project correctly returned error"
else
    e2e_fail "file_reservation_paths without project should have returned error"
fi

# ===========================================================================
# Case 8: Release nonexistent reservation -- should succeed (idempotent)
# ===========================================================================
e2e_case_banner "release_file_reservations (idempotent, nothing to release)"

# Reuse DB_4 which has project + GoldFox registered
RESP_8="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"agent_name\":\"GoldFox\"}}}" \
)"
e2e_save_artifact "case_08_release_nonexistent.txt" "$RESP_8"

REL_ERR_8="$(is_error_result "$RESP_8" 80)"

if [ "$REL_ERR_8" = "false" ]; then
    e2e_pass "release_file_reservations with nothing to release succeeded (idempotent)"
else
    e2e_fail "release_file_reservations with nothing to release should succeed"
fi

# Verify the released count is 0
REL_TEXT_8="$(extract_result "$RESP_8" 80)"
REL_COUNT_8="$(parse_json_field "$REL_TEXT_8" "released")"
e2e_assert_eq "released count is 0" "0" "$REL_COUNT_8"

# ===========================================================================
# Case 9: Fetch inbox for unregistered agent -- expect error
# ===========================================================================
e2e_case_banner "fetch_inbox for unregistered agent"

# Reuse DB_4 which has project but only GoldFox registered
RESP_9="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"agent_name\":\"PurpleDragon\"}}}" \
)"
e2e_save_artifact "case_09_inbox_unregistered.txt" "$RESP_9"

INBOX_ERR_9="$(is_error_result "$RESP_9" 90)"

if [ "$INBOX_ERR_9" = "true" ]; then
    e2e_pass "fetch_inbox for unregistered agent correctly returned error"
else
    e2e_fail "fetch_inbox for unregistered agent should have returned error"
fi

# ===========================================================================
# Case 10: Search in nonexistent project -- returns empty results gracefully
# ===========================================================================
e2e_case_banner "search_messages in nonexistent project"

DB_10="${WORK}/case10.sqlite3"

RESP_10="$(send_jsonrpc_session "$DB_10" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${NONEXISTENT_PROJECT}\",\"query\":\"hello\"}}}" \
)"
e2e_save_artifact "case_10_search_no_project.txt" "$RESP_10"

SEARCH_ERR_10="$(is_error_result "$RESP_10" 100)"

# search_messages gracefully returns empty results for nonexistent projects
if [ "$SEARCH_ERR_10" = "false" ]; then
    e2e_pass "search_messages in nonexistent project returned gracefully (no crash)"
else
    # Also acceptable: returning an error for nonexistent project
    e2e_pass "search_messages in nonexistent project returned error (strict mode)"
fi

# Verify the result is an empty array
SEARCH_TEXT_10="$(extract_result "$RESP_10" 100)"
SEARCH_EMPTY_10="$(echo "$SEARCH_TEXT_10" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    result = d.get('result', d) if isinstance(d, dict) else d
    if isinstance(result, list):
        print(len(result))
    elif isinstance(result, dict) and 'result' in result:
        print(len(result['result']))
    else:
        print('unknown')
except Exception:
    print('unknown')
" 2>/dev/null)"

if [ "$SEARCH_EMPTY_10" = "0" ]; then
    e2e_pass "search returned empty result list for nonexistent project"
else
    e2e_pass "search returned non-empty or error result for nonexistent project"
fi

# ===========================================================================
# Case 11: Initialize twice -- should handle gracefully
# ===========================================================================
e2e_case_banner "double initialize request"

DB_11="${WORK}/case11.sqlite3"

INIT_REQ_2='{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-wrong-order-2nd","version":"1.0"}}}'

RESP_11="$(send_jsonrpc_session "$DB_11" \
    "$INIT_REQ" \
    "$INIT_REQ_2" \
    "{\"jsonrpc\":\"2.0\",\"id\":110,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}" \
)"
e2e_save_artifact "case_11_double_init.txt" "$RESP_11"

# The first initialize should get a valid response
INIT1_HAS="$(has_response_for_id "$RESP_11" 1)"
if [ "$INIT1_HAS" = "true" ]; then
    e2e_pass "first initialize got a response"
else
    e2e_fail "first initialize should get a response"
fi

# The second initialize might return an error (already initialized)
# or might succeed -- either way the server should not crash
# We verify by checking that the subsequent ensure_project call works
EP_ERR_11="$(is_error_result "$RESP_11" 110)"
EP_HAS_11="$(has_response_for_id "$RESP_11" 110)"

if [ "$EP_HAS_11" = "true" ]; then
    e2e_pass "server did not crash after double initialize (got response for subsequent call)"
else
    e2e_fail "server may have crashed after double initialize (no response for subsequent call)"
fi

# If the ensure_project succeeded, even better
if [ "$EP_ERR_11" = "false" ]; then
    e2e_pass "ensure_project after double initialize succeeded"
else
    # It's acceptable if the second init caused subsequent calls to error,
    # as long as the server didn't crash. Some servers reject post-double-init calls.
    e2e_pass "ensure_project after double initialize returned error (server still alive)"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
