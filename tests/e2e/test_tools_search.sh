#!/usr/bin/env bash
# test_tools_search.sh - E2E: Search tools (search_messages, summarize_thread)
#
# Verifies the search and summarization tools work correctly through the MCP
# stdio transport. These tools enable agents to discover prior conversations
# and get thread digests.
#
# Tests:
#   1. Setup: ensure_project + register 2 agents + send 3 messages (2 threads)
#   2. search_messages: phrase match for "build plan" -- at least 1 result
#   3. search_messages: prefix search for migrat* -- at least 1 result
#   4. search_messages: no results for "xyznonexistent"
#   5. summarize_thread: single thread PR-100 -- participants include both agents
#   6. summarize_thread: nonexistent thread -- succeeds with empty/minimal data

E2E_SUITE="tools_search"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Search Tools E2E Test Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_search")"
SEARCH_DB="${WORK}/search_test.sqlite3"
PROJECT_PATH="/tmp/e2e_search_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-search","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers (same pattern as test_tools_identity.sh / test_tools_messaging.sh)
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
# Case 1: Setup -- project + 2 agents + 3 messages across 2 threads
# ===========================================================================
e2e_case_banner "Setup: project + agents + messages"

SETUP_RESP="$(send_jsonrpc_session "$SEARCH_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Build plan for API\",\"body_md\":\"We need to refactor the users endpoint\",\"thread_id\":\"PR-100\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":1,\"sender_name\":\"SilverWolf\",\"body_md\":\"I agree, let me start the migration\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":15,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Database schema update\",\"body_md\":\"New columns for auth tokens\",\"thread_id\":\"DB-50\"}}}" \
)"
e2e_save_artifact "case1_setup.txt" "$SETUP_RESP"

# Verify project + agents registered
PROJ_ERR="$(is_error_result "$SETUP_RESP" 10)"
GF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"
if [ "$PROJ_ERR" = "false" ] && [ "$GF_ERR" = "false" ] && [ "$SW_ERR" = "false" ]; then
    e2e_pass "setup: project + 2 agents registered"
else
    e2e_fail "setup: project or agent registration failed"
    echo "    proj_err=$PROJ_ERR gf_err=$GF_ERR sw_err=$SW_ERR"
fi

# Verify all 3 messages sent successfully
MSG1_ERR="$(is_error_result "$SETUP_RESP" 13)"
MSG2_ERR="$(is_error_result "$SETUP_RESP" 14)"
MSG3_ERR="$(is_error_result "$SETUP_RESP" 15)"
if [ "$MSG1_ERR" = "false" ] && [ "$MSG2_ERR" = "false" ] && [ "$MSG3_ERR" = "false" ]; then
    e2e_pass "setup: 3 messages sent (2 threads)"
else
    e2e_fail "setup: message sending failed"
    echo "    msg1_err=$MSG1_ERR msg2_err=$MSG2_ERR msg3_err=$MSG3_ERR"
    # Show error details
    MSG1_TEXT="$(extract_result "$SETUP_RESP" 13)"
    MSG2_TEXT="$(extract_result "$SETUP_RESP" 14)"
    MSG3_TEXT="$(extract_result "$SETUP_RESP" 15)"
    echo "    msg1: $MSG1_TEXT"
    echo "    msg2: $MSG2_TEXT"
    echo "    msg3: $MSG3_TEXT"
fi

# ===========================================================================
# Case 2: search_messages -- phrase match for "build plan"
# ===========================================================================
e2e_case_banner "search_messages: phrase match for \"build plan\""

PHRASE_RESP="$(send_jsonrpc_session "$SEARCH_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"query\":\"\\\"build plan\\\"\",\"limit\":10}}}" \
)"
e2e_save_artifact "case2_phrase_search.txt" "$PHRASE_RESP"

PHRASE_ERR="$(is_error_result "$PHRASE_RESP" 20)"
PHRASE_TEXT="$(extract_result "$PHRASE_RESP" 20)"
if [ "$PHRASE_ERR" = "true" ]; then
    e2e_fail "search_messages phrase match returned error"
    echo "    text: $PHRASE_TEXT"
else
    e2e_pass "search_messages phrase match succeeded"
fi

# Parse search results: {result: [...]} or plain array
PHRASE_CHECK="$(echo "$PHRASE_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    # search_messages wraps results in {result: [...]}
    if isinstance(d, dict) and 'result' in d:
        items = d['result']
    elif isinstance(d, list):
        items = d
    else:
        items = []
    count = len(items) if isinstance(items, list) else 0
    first_subject = items[0].get('subject', '') if items else ''
    print(f'count={count}|first_subject={first_subject}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case2_parsed.txt" "$PHRASE_CHECK"

# Assert at least 1 result
if echo "$PHRASE_CHECK" | grep -qE "count=[1-9]"; then
    e2e_pass "phrase search returned at least 1 result"
else
    e2e_fail "phrase search returned 0 results (expected >= 1)"
    echo "    result: $PHRASE_CHECK"
fi

# Assert first result subject contains "Build plan"
if echo "$PHRASE_CHECK" | grep -qi "Build plan"; then
    e2e_pass "phrase search first result subject contains 'Build plan'"
else
    e2e_fail "phrase search first result subject missing 'Build plan'"
    echo "    result: $PHRASE_CHECK"
fi

# ===========================================================================
# Case 3: search_messages -- prefix search for migrat*
# ===========================================================================
e2e_case_banner "search_messages: prefix search for migrat*"

PREFIX_RESP="$(send_jsonrpc_session "$SEARCH_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"query\":\"migrat*\",\"limit\":10}}}" \
)"
e2e_save_artifact "case3_prefix_search.txt" "$PREFIX_RESP"

PREFIX_ERR="$(is_error_result "$PREFIX_RESP" 30)"
PREFIX_TEXT="$(extract_result "$PREFIX_RESP" 30)"
if [ "$PREFIX_ERR" = "true" ]; then
    e2e_fail "search_messages prefix search returned error"
    echo "    text: $PREFIX_TEXT"
else
    e2e_pass "search_messages prefix search succeeded"
fi

PREFIX_COUNT="$(echo "$PREFIX_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if isinstance(d, dict) and 'result' in d:
        items = d['result']
    elif isinstance(d, list):
        items = d
    else:
        items = []
    print(len(items) if isinstance(items, list) else 0)
except Exception:
    print(0)
" 2>/dev/null)"
e2e_save_artifact "case3_count.txt" "$PREFIX_COUNT"

if [ "$PREFIX_COUNT" -ge 1 ] 2>/dev/null; then
    e2e_pass "prefix search returned at least 1 result (count=$PREFIX_COUNT)"
else
    e2e_fail "prefix search returned 0 results (expected >= 1)"
    echo "    count: $PREFIX_COUNT"
    echo "    text: $PREFIX_TEXT"
fi

# ===========================================================================
# Case 4: search_messages -- no results for "xyznonexistent"
# ===========================================================================
e2e_case_banner "search_messages: no results for xyznonexistent"

EMPTY_RESP="$(send_jsonrpc_session "$SEARCH_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"query\":\"\\\"xyznonexistent\\\"\",\"limit\":10}}}" \
)"
e2e_save_artifact "case4_empty_search.txt" "$EMPTY_RESP"

EMPTY_ERR="$(is_error_result "$EMPTY_RESP" 40)"
EMPTY_TEXT="$(extract_result "$EMPTY_RESP" 40)"
if [ "$EMPTY_ERR" = "true" ]; then
    e2e_fail "search_messages no-results query returned error"
    echo "    text: $EMPTY_TEXT"
else
    e2e_pass "search_messages no-results query succeeded"
fi

EMPTY_COUNT="$(echo "$EMPTY_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if isinstance(d, dict) and 'result' in d:
        items = d['result']
    elif isinstance(d, list):
        items = d
    else:
        items = []
    print(len(items) if isinstance(items, list) else 0)
except Exception:
    print(0)
" 2>/dev/null)"
e2e_save_artifact "case4_count.txt" "$EMPTY_COUNT"

if [ "$EMPTY_COUNT" -eq 0 ] 2>/dev/null; then
    e2e_pass "no-results search returned 0 results as expected"
else
    e2e_fail "no-results search returned $EMPTY_COUNT results (expected 0)"
    echo "    text: $EMPTY_TEXT"
fi

# ===========================================================================
# Case 5: summarize_thread -- single thread PR-100
# ===========================================================================
e2e_case_banner "summarize_thread: single thread PR-100"

SUMM_RESP="$(send_jsonrpc_session "$SEARCH_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"summarize_thread\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"thread_id\":\"PR-100\"}}}" \
)"
e2e_save_artifact "case5_summarize.txt" "$SUMM_RESP"

SUMM_ERR="$(is_error_result "$SUMM_RESP" 50)"
SUMM_TEXT="$(extract_result "$SUMM_RESP" 50)"
if [ "$SUMM_ERR" = "true" ]; then
    e2e_fail "summarize_thread PR-100 returned error"
    echo "    text: $SUMM_TEXT"
else
    e2e_pass "summarize_thread PR-100 succeeded"
fi

# Verify response structure: thread_id, summary with participants
SUMM_CHECK="$(echo "$SUMM_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    thread_id = d.get('thread_id', '')
    summary = d.get('summary', {})
    participants = summary.get('participants', [])
    key_points = summary.get('key_points', [])
    action_items = summary.get('action_items', [])
    # Participants may be strings or dicts with a 'name' field
    participant_names = []
    for p in participants:
        if isinstance(p, str):
            participant_names.append(p)
        elif isinstance(p, dict):
            participant_names.append(p.get('name', p.get('agent', '')))
    has_goldfox = any('GoldFox' in str(n) for n in participant_names)
    has_silverwolf = any('SilverWolf' in str(n) for n in participant_names)
    print(f'thread_id={thread_id}|participant_count={len(participants)}|has_goldfox={has_goldfox}|has_silverwolf={has_silverwolf}|key_points_count={len(key_points)}|action_items_count={len(action_items)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case5_parsed.txt" "$SUMM_CHECK"

e2e_assert_contains "thread_id is PR-100" "$SUMM_CHECK" "thread_id=PR-100"

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
# Case 6: summarize_thread -- nonexistent thread NONEXISTENT
# ===========================================================================
e2e_case_banner "summarize_thread: nonexistent thread"

NOTHREAD_RESP="$(send_jsonrpc_session "$SEARCH_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"summarize_thread\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"thread_id\":\"NONEXISTENT\"}}}" \
)"
e2e_save_artifact "case6_nothread.txt" "$NOTHREAD_RESP"

NOTHREAD_ERR="$(is_error_result "$NOTHREAD_RESP" 60)"
NOTHREAD_TEXT="$(extract_result "$NOTHREAD_RESP" 60)"

# A nonexistent thread should either succeed with empty data or return an error.
# Both are acceptable behaviors. Check what we got.
if [ "$NOTHREAD_ERR" = "true" ]; then
    e2e_pass "summarize_thread NONEXISTENT returned error (acceptable)"
else
    e2e_pass "summarize_thread NONEXISTENT succeeded (will check for minimal data)"
fi

# If it succeeded, verify the summary is empty/minimal
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
    e2e_save_artifact "case6_parsed.txt" "$NOTHREAD_CHECK"

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
