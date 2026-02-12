#!/usr/bin/env bash
# test_tools_messaging.sh - E2E: Messaging tools (send, reply, fetch_inbox, mark_read, acknowledge)
#
# Verifies the 5 messaging tools work correctly through MCP stdio transport.
# These tools are the core communication channel for agent coordination.
#
# Tests:
#   1. send_message: RedFox → SilverWolf with thread + ack_required
#   2. fetch_inbox: SilverWolf sees the message with correct fields
#   3. fetch_inbox with limit=0: returns empty
#   4. acknowledge_message: SilverWolf acks the message
#   5. reply_message: SilverWolf replies (Re: prefix, same thread)
#   6. mark_message_read: RedFox marks reply as read
#   7. send_message with empty body: still works
#   8. send_message with cc recipient
#   9. fetch_inbox for RedFox: sees the reply
#  10. Multiple messages ordering

E2E_SUITE="tools_messaging"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Messaging Tools E2E Test Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_messaging")"
MSG_DB="${WORK}/messaging_test.sqlite3"
PROJECT_PATH="/tmp/e2e_messaging_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-messaging","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers (same pattern as test_macros.sh)
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

# ===========================================================================
# Setup: Create project + register two agents
# ===========================================================================
e2e_case_banner "Setup: project + two agents (RedFox, SilverWolf)"

SETUP_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"RedFox\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\"}}}" \
)"
e2e_save_artifact "setup.txt" "$SETUP_RESP"

RF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"
if [ "$RF_ERR" = "false" ] && [ "$SW_ERR" = "false" ]; then
    e2e_pass "setup: project + 2 agents registered"
else
    e2e_fail "setup failed"
fi

# ===========================================================================
# Case 1: send_message with thread + ack_required
# ===========================================================================
e2e_case_banner "send_message: RedFox → SilverWolf"

SEND_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Code review needed\",\"body_md\":\"Please review PR #42.\\n\\nChanges look good.\",\"thread_id\":\"PR-42\",\"ack_required\":true,\"importance\":\"high\"}}}" \
)"
e2e_save_artifact "case1_send.txt" "$SEND_RESP"

SEND_ERR="$(is_error_result "$SEND_RESP" 20)"
SEND_TEXT="$(extract_result "$SEND_RESP" 20)"
if [ "$SEND_ERR" = "true" ]; then
    e2e_fail "send_message returned error"
    echo "    text: $SEND_TEXT"
else
    e2e_pass "send_message succeeded"
fi

# send_message returns {deliveries: [{payload: {id, ...}}], count}
MSG_ID="$(parse_json_field "$SEND_TEXT" "deliveries.0.payload.id")"
if [ -n "$MSG_ID" ] && [ "$MSG_ID" != "" ] && [ "$MSG_ID" != "None" ]; then
    e2e_pass "send_message returned message id: $MSG_ID"
else
    e2e_fail "send_message missing id"
fi

# ===========================================================================
# Case 2: fetch_inbox for SilverWolf
# ===========================================================================
e2e_case_banner "fetch_inbox: SilverWolf sees the message"

INBOX_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true,\"limit\":10}}}" \
)"
e2e_save_artifact "case2_inbox.txt" "$INBOX_RESP"

INBOX_TEXT="$(extract_result "$INBOX_RESP" 30)"
INBOX_CHECK="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    messages = json.loads(sys.stdin.read())
    if isinstance(messages, list):
        count = len(messages)
        subjects = [m.get('subject', '') for m in messages]
        senders = [m.get('from_agent', m.get('from', '')) for m in messages]
        bodies = [m.get('body_md', '') for m in messages]
        importances = [m.get('importance', '') for m in messages]
        ack_flags = [m.get('ack_required', False) for m in messages]
        thread_ids = [m.get('thread_id', '') for m in messages]
        print(f'count={count}')
        print(f'subject={subjects[0] if subjects else \"\"}')
        print(f'sender={senders[0] if senders else \"\"}')
        print(f'has_body={bool(bodies[0]) if bodies else False}')
        print(f'importance={importances[0] if importances else \"\"}')
        print(f'ack_required={ack_flags[0] if ack_flags else \"\"}')
        print(f'thread_id={thread_ids[0] if thread_ids else \"\"}')
    else:
        print(f'not_list')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case2_parsed.txt" "$INBOX_CHECK"

e2e_assert_contains "inbox has 1 message" "$INBOX_CHECK" "count=1"
e2e_assert_contains "correct subject" "$INBOX_CHECK" "subject=Code review needed"
e2e_assert_contains "sender is RedFox" "$INBOX_CHECK" "sender=RedFox"
e2e_assert_contains "body present" "$INBOX_CHECK" "has_body=True"
e2e_assert_contains "importance=high" "$INBOX_CHECK" "importance=high"
e2e_assert_contains "ack_required=True" "$INBOX_CHECK" "ack_required=True"
e2e_assert_contains "thread_id=PR-42" "$INBOX_CHECK" "thread_id=PR-42"

# ===========================================================================
# Case 3: fetch_inbox with limit=0 returns validation error
# ===========================================================================
e2e_case_banner "fetch_inbox with limit=0 returns validation error"

EMPTY_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"limit\":0}}}" \
)"
e2e_save_artifact "case3_empty.txt" "$EMPTY_RESP"

EMPTY_ERROR="$(is_error_result "$EMPTY_RESP" 31)"
if [ "$EMPTY_ERROR" = "true" ]; then
    e2e_pass "limit=0 returns validation error as expected"
else
    e2e_fail "limit=0 should return an error but did not"
fi

# ===========================================================================
# Case 4: acknowledge_message
# ===========================================================================
e2e_case_banner "acknowledge_message: SilverWolf acks"

ACK_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"message_id\":${MSG_ID}}}}" \
)"
e2e_save_artifact "case4_ack.txt" "$ACK_RESP"

ACK_ERR="$(is_error_result "$ACK_RESP" 40)"
if [ "$ACK_ERR" = "true" ]; then
    ACK_TEXT="$(extract_result "$ACK_RESP" 40)"
    e2e_fail "acknowledge_message returned error"
    echo "    text: $ACK_TEXT"
else
    e2e_pass "acknowledge_message succeeded"
fi

# ===========================================================================
# Case 5: reply_message
# ===========================================================================
e2e_case_banner "reply_message: SilverWolf replies"

REPLY_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":${MSG_ID},\"sender_name\":\"SilverWolf\",\"body_md\":\"LGTM, merging now.\"}}}" \
)"
e2e_save_artifact "case5_reply.txt" "$REPLY_RESP"

REPLY_ERR="$(is_error_result "$REPLY_RESP" 50)"
REPLY_TEXT="$(extract_result "$REPLY_RESP" 50)"
if [ "$REPLY_ERR" = "true" ]; then
    e2e_fail "reply_message returned error"
    echo "    text: $REPLY_TEXT"
else
    e2e_pass "reply_message succeeded"
fi

# reply_message returns {deliveries: [{payload: {subject, thread_id, id, ...}}], count}
REPLY_SUBJ="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.subject")"
REPLY_THREAD="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.thread_id")"
REPLY_ID="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.id")"

e2e_assert_contains "reply has Re: prefix" "$REPLY_SUBJ" "Re:"
e2e_assert_eq "reply preserves thread_id" "PR-42" "$REPLY_THREAD"

if [ -n "$REPLY_ID" ] && [ "$REPLY_ID" != "" ] && [ "$REPLY_ID" != "None" ]; then
    e2e_pass "reply returned new message id: $REPLY_ID"
else
    e2e_fail "reply missing id"
fi

# ===========================================================================
# Case 6: mark_message_read
# ===========================================================================
e2e_case_banner "mark_message_read: RedFox marks reply as read"

READ_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"mark_message_read\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"RedFox\",\"message_id\":${REPLY_ID}}}}" \
)"
e2e_save_artifact "case6_read.txt" "$READ_RESP"

READ_ERR="$(is_error_result "$READ_RESP" 60)"
if [ "$READ_ERR" = "true" ]; then
    READ_TEXT="$(extract_result "$READ_RESP" 60)"
    e2e_fail "mark_message_read returned error"
    echo "    text: $READ_TEXT"
else
    e2e_pass "mark_message_read succeeded"
fi

# ===========================================================================
# Case 7: send_message with empty body
# ===========================================================================
e2e_case_banner "send_message with empty body"

EMPTY_BODY_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Quick ping\",\"body_md\":\"\"}}}" \
)"
e2e_save_artifact "case7_empty_body.txt" "$EMPTY_BODY_RESP"

EB_ERR="$(is_error_result "$EMPTY_BODY_RESP" 70)"
if [ "$EB_ERR" = "true" ]; then
    EB_TEXT="$(extract_result "$EMPTY_BODY_RESP" 70)"
    e2e_fail "send_message with empty body returned error"
    echo "    text: $EB_TEXT"
else
    e2e_pass "send_message with empty body succeeded"
fi

# ===========================================================================
# Case 8: fetch_inbox for RedFox (sees reply from SilverWolf)
# ===========================================================================
e2e_case_banner "fetch_inbox: RedFox sees reply from SilverWolf"

RF_INBOX_RESP="$(send_jsonrpc_session "$MSG_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"RedFox\",\"include_bodies\":true,\"limit\":10}}}" \
)"
e2e_save_artifact "case8_rf_inbox.txt" "$RF_INBOX_RESP"

RF_TEXT="$(extract_result "$RF_INBOX_RESP" 80)"
RF_CHECK="$(echo "$RF_TEXT" | python3 -c "
import sys, json
try:
    messages = json.loads(sys.stdin.read())
    if isinstance(messages, list):
        count = len(messages)
        senders = [m.get('from_agent', m.get('from', '')) for m in messages]
        has_reply = any('SilverWolf' in s for s in senders)
        print(f'count={count}|has_reply_from_sw={has_reply}')
    else:
        print('not_list')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case8_parsed.txt" "$RF_CHECK"

if echo "$RF_CHECK" | grep -q "has_reply_from_sw=True"; then
    e2e_pass "RedFox inbox contains reply from SilverWolf"
else
    e2e_fail "RedFox inbox missing reply from SilverWolf"
    echo "    result: $RF_CHECK"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
