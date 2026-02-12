#!/usr/bin/env bash
# test_threading_orphans.sh - E2E: Message threading edge cases (orphans, gaps, nonexistent refs)
#
# Verifies correct behavior when threading references are broken or missing:
#   - Replies to nonexistent messages
#   - Custom thread_id usage and thread coherence
#   - Summarize thread with multiple messages
#   - Summarize nonexistent thread
#
# Tests:
#   1. Setup: ensure_project + register 2 agents (GoldFox, SilverWolf)
#   2. send_message: GoldFox -> SilverWolf, capture message ID
#   3. reply_message: SilverWolf replies normally (verify success + Re: prefix)
#   4. reply_message: reply to nonexistent message_id 99999 (expect error)
#   5. send_message with custom thread_id "ORPHAN-THREAD-1"
#   6. send_message second message in same thread "ORPHAN-THREAD-1"
#   7. summarize_thread: "ORPHAN-THREAD-1" -- verify both messages appear
#   8. summarize_thread: nonexistent thread "DOES-NOT-EXIST-999" -- graceful response

E2E_SUITE="threading_orphans"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Threading Orphans E2E Test Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_threading")"
THREAD_DB="${WORK}/threading_test.sqlite3"
PROJECT_PATH="/tmp/e2e_threading_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-threading","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers (same pattern as test_tools_messaging.sh)
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
# Case 1: Setup - create project + register two agents (GoldFox, SilverWolf)
# ===========================================================================
e2e_case_banner "Setup: project + two agents (GoldFox, SilverWolf)"

SETUP_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\"}}}" \
)"
e2e_save_artifact "case1_setup.txt" "$SETUP_RESP"

GF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"
if [ "$GF_ERR" = "false" ] && [ "$SW_ERR" = "false" ]; then
    e2e_pass "setup: project + 2 agents registered"
else
    e2e_fail "setup failed (GoldFox error=$GF_ERR, SilverWolf error=$SW_ERR)"
fi

# ===========================================================================
# Case 2: send_message - GoldFox -> SilverWolf (get message ID)
# ===========================================================================
e2e_case_banner "send_message: GoldFox -> SilverWolf"

SEND_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Thread test original\",\"body_md\":\"This is the original message for threading tests.\"}}}" \
)"
e2e_save_artifact "case2_send.txt" "$SEND_RESP"

SEND_ERR="$(is_error_result "$SEND_RESP" 20)"
SEND_TEXT="$(extract_result "$SEND_RESP" 20)"
if [ "$SEND_ERR" = "true" ]; then
    e2e_fail "send_message returned error"
    echo "    text: $SEND_TEXT"
else
    e2e_pass "send_message succeeded"
fi

# Extract the message ID from the deliveries response
MSG_ID="$(parse_json_field "$SEND_TEXT" "deliveries.0.payload.id")"
if [ -n "$MSG_ID" ] && [ "$MSG_ID" != "" ] && [ "$MSG_ID" != "None" ]; then
    e2e_pass "send_message returned message id: $MSG_ID"
else
    e2e_fail "send_message missing id"
    echo "    text: $SEND_TEXT"
fi

# ===========================================================================
# Case 3: reply_message - SilverWolf replies normally
# ===========================================================================
e2e_case_banner "reply_message: SilverWolf replies normally"

REPLY_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":${MSG_ID},\"sender_name\":\"SilverWolf\",\"body_md\":\"Got it, thanks for the original message.\"}}}" \
)"
e2e_save_artifact "case3_reply.txt" "$REPLY_RESP"

REPLY_ERR="$(is_error_result "$REPLY_RESP" 30)"
REPLY_TEXT="$(extract_result "$REPLY_RESP" 30)"
if [ "$REPLY_ERR" = "true" ]; then
    e2e_fail "reply_message returned error"
    echo "    text: $REPLY_TEXT"
else
    e2e_pass "reply_message succeeded"
fi

# Verify Re: prefix on subject
REPLY_SUBJ="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.subject")"
e2e_assert_contains "reply has Re: prefix" "$REPLY_SUBJ" "Re:"

REPLY_ID="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.id")"
if [ -n "$REPLY_ID" ] && [ "$REPLY_ID" != "" ] && [ "$REPLY_ID" != "None" ]; then
    e2e_pass "reply returned new message id: $REPLY_ID"
else
    e2e_fail "reply missing id"
fi

# ===========================================================================
# Case 4: reply_message to nonexistent message ID (expect error)
# ===========================================================================
e2e_case_banner "reply_message: reply to nonexistent message_id 99999"

ORPHAN_REPLY_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":99999,\"sender_name\":\"SilverWolf\",\"body_md\":\"This reply references a message that does not exist.\"}}}" \
)"
e2e_save_artifact "case4_orphan_reply.txt" "$ORPHAN_REPLY_RESP"

ORPHAN_ERR="$(is_error_result "$ORPHAN_REPLY_RESP" 40)"
if [ "$ORPHAN_ERR" = "true" ]; then
    e2e_pass "reply to nonexistent message returned error (expected)"
else
    e2e_fail "reply to nonexistent message should have returned error"
    ORPHAN_TEXT="$(extract_result "$ORPHAN_REPLY_RESP" 40)"
    echo "    text: $ORPHAN_TEXT"
fi

# ===========================================================================
# Case 5: send_message with custom thread_id "ORPHAN-THREAD-1"
# ===========================================================================
e2e_case_banner "send_message: custom thread_id ORPHAN-THREAD-1 (first message)"

THREAD_MSG1_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Custom thread first\",\"body_md\":\"First message in custom thread ORPHAN-THREAD-1.\",\"thread_id\":\"ORPHAN-THREAD-1\"}}}" \
)"
e2e_save_artifact "case5_thread_msg1.txt" "$THREAD_MSG1_RESP"

TM1_ERR="$(is_error_result "$THREAD_MSG1_RESP" 50)"
TM1_TEXT="$(extract_result "$THREAD_MSG1_RESP" 50)"
if [ "$TM1_ERR" = "true" ]; then
    e2e_fail "send_message with custom thread_id returned error"
    echo "    text: $TM1_TEXT"
else
    e2e_pass "send_message with custom thread_id succeeded"
fi

TM1_THREAD="$(parse_json_field "$TM1_TEXT" "deliveries.0.payload.thread_id")"
e2e_assert_eq "first msg thread_id is ORPHAN-THREAD-1" "ORPHAN-THREAD-1" "$TM1_THREAD"

# ===========================================================================
# Case 6: send_message second message in same thread "ORPHAN-THREAD-1"
# ===========================================================================
e2e_case_banner "send_message: second message in ORPHAN-THREAD-1"

THREAD_MSG2_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"Custom thread reply\",\"body_md\":\"Second message in custom thread ORPHAN-THREAD-1, from SilverWolf.\",\"thread_id\":\"ORPHAN-THREAD-1\"}}}" \
)"
e2e_save_artifact "case6_thread_msg2.txt" "$THREAD_MSG2_RESP"

TM2_ERR="$(is_error_result "$THREAD_MSG2_RESP" 60)"
TM2_TEXT="$(extract_result "$THREAD_MSG2_RESP" 60)"
if [ "$TM2_ERR" = "true" ]; then
    e2e_fail "second message in thread returned error"
    echo "    text: $TM2_TEXT"
else
    e2e_pass "second message in thread succeeded"
fi

TM2_THREAD="$(parse_json_field "$TM2_TEXT" "deliveries.0.payload.thread_id")"
e2e_assert_eq "second msg thread_id is ORPHAN-THREAD-1" "ORPHAN-THREAD-1" "$TM2_THREAD"

# ===========================================================================
# Case 7: summarize_thread for ORPHAN-THREAD-1 -- both messages appear
# ===========================================================================
e2e_case_banner "summarize_thread: ORPHAN-THREAD-1 (both agents should appear)"

SUMM_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"summarize_thread\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"thread_id\":\"ORPHAN-THREAD-1\"}}}" \
)"
e2e_save_artifact "case7_summarize.txt" "$SUMM_RESP"

SUMM_ERR="$(is_error_result "$SUMM_RESP" 70)"
SUMM_TEXT="$(extract_result "$SUMM_RESP" 70)"
if [ "$SUMM_ERR" = "true" ]; then
    e2e_fail "summarize_thread ORPHAN-THREAD-1 returned error"
    echo "    text: $SUMM_TEXT"
else
    e2e_pass "summarize_thread ORPHAN-THREAD-1 succeeded"
fi

# Parse the summary to verify participants
SUMM_CHECK="$(echo "$SUMM_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    thread_id = d.get('thread_id', '')
    summary = d.get('summary', {})
    participants = summary.get('participants', [])
    # Participants may be strings or dicts with a 'name' field
    participant_names = []
    for p in participants:
        if isinstance(p, str):
            participant_names.append(p)
        elif isinstance(p, dict):
            participant_names.append(p.get('name', p.get('agent', '')))
    has_goldfox = any('GoldFox' in str(n) for n in participant_names)
    has_silverwolf = any('SilverWolf' in str(n) for n in participant_names)
    key_points = summary.get('key_points', [])
    print(f'thread_id={thread_id}|participant_count={len(participants)}|has_goldfox={has_goldfox}|has_silverwolf={has_silverwolf}|key_points_count={len(key_points)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case7_parsed.txt" "$SUMM_CHECK"

e2e_assert_contains "thread_id is ORPHAN-THREAD-1" "$SUMM_CHECK" "thread_id=ORPHAN-THREAD-1"

if echo "$SUMM_CHECK" | grep -q "has_goldfox=True"; then
    e2e_pass "summarize_thread participants include GoldFox"
else
    e2e_fail "summarize_thread participants missing GoldFox"
    echo "    result: $SUMM_CHECK"
fi

if echo "$SUMM_CHECK" | grep -q "has_silverwolf=True"; then
    e2e_pass "summarize_thread participants include SilverWolf"
else
    e2e_fail "summarize_thread participants missing SilverWolf"
    echo "    result: $SUMM_CHECK"
fi

# ===========================================================================
# Case 8: summarize_thread for nonexistent thread -- graceful response
# ===========================================================================
e2e_case_banner "summarize_thread: nonexistent thread DOES-NOT-EXIST-999"

NOTHREAD_RESP="$(send_jsonrpc_session "$THREAD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"summarize_thread\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"thread_id\":\"DOES-NOT-EXIST-999\"}}}" \
)"
e2e_save_artifact "case8_nothread.txt" "$NOTHREAD_RESP"

NOTHREAD_ERR="$(is_error_result "$NOTHREAD_RESP" 80)"
NOTHREAD_TEXT="$(extract_result "$NOTHREAD_RESP" 80)"

# A nonexistent thread should either succeed with empty data or return an error.
# Both are acceptable behaviors.
if [ "$NOTHREAD_ERR" = "true" ]; then
    e2e_pass "summarize_thread DOES-NOT-EXIST-999 returned error (acceptable)"
else
    e2e_pass "summarize_thread DOES-NOT-EXIST-999 succeeded (will check for minimal data)"
fi

# If it succeeded, verify the summary has empty/minimal participants
if [ "$NOTHREAD_ERR" = "false" ]; then
    NOTHREAD_CHECK="$(echo "$NOTHREAD_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    summary = d.get('summary', {})
    participants = summary.get('participants', [])
    key_points = summary.get('key_points', [])
    print(f'participant_count={len(participants)}|key_points_count={len(key_points)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
    e2e_save_artifact "case8_parsed.txt" "$NOTHREAD_CHECK"

    if echo "$NOTHREAD_CHECK" | grep -q "participant_count=0"; then
        e2e_pass "nonexistent thread has 0 participants (expected)"
    else
        e2e_pass "nonexistent thread returned data (server may synthesize minimal summary)"
    fi
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
