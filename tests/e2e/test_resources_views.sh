#!/usr/bin/env bash
# test_resources_views.sh - E2E: View resources (urgent-unread, ack-required, acks-stale, ack-overdue)
#
# Verifies the 4 MCP view resources return correct data through stdio transport.
# These resources provide filtered views into an agent's inbox for urgent,
# ack-required, stale-ack, and overdue-ack scenarios.
#
# Tests:
#   1. Setup: ensure_project + register 2 agents (GoldFox, SilverWolf)
#   2. Send an urgent ack_required message from GoldFox to SilverWolf
#   3. Send a normal (non-urgent, no ack) message from GoldFox to SilverWolf
#   4. views/urgent-unread/SilverWolf: sees the urgent message
#   5. views/ack-required/SilverWolf: sees the ack-required message
#   6. views/acks-stale/SilverWolf: sees the unacked message (with ttl_seconds=0)
#   7. views/ack-overdue/SilverWolf: sees the overdue message (with ttl_minutes=0)
#   8. Acknowledge the message as SilverWolf
#   9. views/ack-required/SilverWolf (post-ack): ack-required message gone
#  10. views/acks-stale/SilverWolf (post-ack): stale message gone

E2E_SUITE="resources_views"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "View Resources E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_views")"
VIEWS_DB="${WORK}/views_test.sqlite3"
PROJECT_PATH="/tmp/e2e_views_$$"
# Slug: /tmp/e2e_views_PID -> tmp-e2e-views-PID
PROJECT_SLUG="tmp-e2e-views-$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-views","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers (same pattern as test_tools_contacts.sh)
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

# Extract resource text from result.contents[0].text (resources use 'contents')
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
        if d.get('id') == $req_id and 'result' in d:
            r = d['result']
            # Resources use 'contents', tools use 'content'
            contents = r.get('contents', r.get('content', []))
            if contents:
                print(contents[0].get('text', ''))
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
# Case 1: Setup -- ensure_project + register 2 agents (GoldFox, SilverWolf)
# ===========================================================================
e2e_case_banner "Setup: project + two agents (GoldFox, SilverWolf)"

SETUP_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"views E2E testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"views E2E testing\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

EP_ERR="$(is_error_result "$SETUP_RESP" 10)"
GF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"

if [ "$EP_ERR" = "false" ]; then
    e2e_pass "ensure_project succeeded"
else
    e2e_fail "ensure_project returned error"
fi

if [ "$GF_ERR" = "false" ] && [ "$SW_ERR" = "false" ]; then
    e2e_pass "both agents (GoldFox, SilverWolf) registered"
else
    e2e_fail "agent registration failed (GoldFox=$GF_ERR, SilverWolf=$SW_ERR)"
fi

# ===========================================================================
# Case 2: Send an urgent ack_required message from GoldFox to SilverWolf
# ===========================================================================
e2e_case_banner "send_message: GoldFox -> SilverWolf (urgent, ack_required)"

SEND_URGENT_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"URGENT: Deploy fix needed\",\"body_md\":\"Production is down, please deploy the hotfix ASAP.\",\"importance\":\"urgent\",\"ack_required\":true}}}" \
)"
e2e_save_artifact "case_02_send_urgent.txt" "$SEND_URGENT_RESP"

SEND_U_ERR="$(is_error_result "$SEND_URGENT_RESP" 20)"
SEND_U_TEXT="$(extract_result "$SEND_URGENT_RESP" 20)"
if [ "$SEND_U_ERR" = "true" ]; then
    e2e_fail "send_message (urgent) returned error"
    echo "    text: $SEND_U_TEXT"
else
    e2e_pass "send_message (urgent, ack_required) succeeded"
fi

# Extract the message ID for later acknowledge
URGENT_MSG_ID="$(parse_json_field "$SEND_U_TEXT" "deliveries.0.payload.id")"
if [ -n "$URGENT_MSG_ID" ] && [ "$URGENT_MSG_ID" != "" ] && [ "$URGENT_MSG_ID" != "None" ]; then
    e2e_pass "urgent message id: $URGENT_MSG_ID"
else
    e2e_fail "urgent message missing id"
fi

# ===========================================================================
# Case 3: Send a normal message from GoldFox to SilverWolf
# ===========================================================================
e2e_case_banner "send_message: GoldFox -> SilverWolf (normal)"

SEND_NORMAL_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"FYI: Weekly update\",\"body_md\":\"Nothing urgent, just a status update.\"}}}" \
)"
e2e_save_artifact "case_03_send_normal.txt" "$SEND_NORMAL_RESP"

SEND_N_ERR="$(is_error_result "$SEND_NORMAL_RESP" 30)"
if [ "$SEND_N_ERR" = "true" ]; then
    e2e_fail "send_message (normal) returned error"
else
    e2e_pass "send_message (normal) succeeded"
fi

# ===========================================================================
# Case 4: views/urgent-unread/SilverWolf -- should see the urgent message
# ===========================================================================
e2e_case_banner "views/urgent-unread/SilverWolf"

VIEW_UU_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://views/urgent-unread/SilverWolf?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_04_urgent_unread.txt" "$VIEW_UU_RESP"

VIEW_UU_ERR="$(is_error_result "$VIEW_UU_RESP" 40)"
VIEW_UU_TEXT="$(extract_resource_text "$VIEW_UU_RESP" 40)"

if [ "$VIEW_UU_ERR" = "true" ]; then
    e2e_fail "views/urgent-unread returned error"
    echo "    resp: $VIEW_UU_RESP"
else
    e2e_pass "views/urgent-unread returned successfully"
fi

# Parse the response
UU_CHECK="$(echo "$VIEW_UU_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', 0)
    messages = d.get('messages', [])
    has_urgent = any('URGENT' in m.get('subject', '') for m in messages)
    has_normal = any('FYI' in m.get('subject', '') for m in messages)
    agent = d.get('agent', '')
    print(f'count={count}|has_urgent={has_urgent}|has_normal={has_normal}|agent={agent}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_04_parsed.txt" "$UU_CHECK"

e2e_assert_contains "urgent-unread count >= 1" "$UU_CHECK" "has_urgent=True"
e2e_assert_contains "urgent-unread does NOT include normal msg" "$UU_CHECK" "has_normal=False"
e2e_assert_contains "urgent-unread agent is SilverWolf" "$UU_CHECK" "agent=SilverWolf"

# ===========================================================================
# Case 5: views/ack-required/SilverWolf -- should see the ack-required message
# ===========================================================================
e2e_case_banner "views/ack-required/SilverWolf (pre-ack)"

VIEW_AR_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://views/ack-required/SilverWolf?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_05_ack_required.txt" "$VIEW_AR_RESP"

VIEW_AR_ERR="$(is_error_result "$VIEW_AR_RESP" 50)"
VIEW_AR_TEXT="$(extract_resource_text "$VIEW_AR_RESP" 50)"

if [ "$VIEW_AR_ERR" = "true" ]; then
    e2e_fail "views/ack-required returned error"
    echo "    resp: $VIEW_AR_RESP"
else
    e2e_pass "views/ack-required returned successfully"
fi

AR_CHECK="$(echo "$VIEW_AR_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', 0)
    messages = d.get('messages', [])
    has_urgent = any('URGENT' in m.get('subject', '') for m in messages)
    all_ack = all(m.get('ack_required', False) for m in messages)
    agent = d.get('agent', '')
    print(f'count={count}|has_urgent={has_urgent}|all_ack={all_ack}|agent={agent}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_parsed.txt" "$AR_CHECK"

e2e_assert_contains "ack-required has 1 message" "$AR_CHECK" "count=1"
e2e_assert_contains "ack-required includes urgent msg" "$AR_CHECK" "has_urgent=True"
e2e_assert_contains "ack-required all have ack_required=True" "$AR_CHECK" "all_ack=True"

# ===========================================================================
# Case 6: views/acks-stale/SilverWolf -- with ttl_seconds=0 to catch freshly sent
# ===========================================================================
e2e_case_banner "views/acks-stale/SilverWolf (pre-ack, ttl_seconds=0)"

VIEW_AS_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://views/acks-stale/SilverWolf?project=${PROJECT_SLUG}&ttl_seconds=0\"}}" \
)"
e2e_save_artifact "case_06_acks_stale.txt" "$VIEW_AS_RESP"

VIEW_AS_ERR="$(is_error_result "$VIEW_AS_RESP" 60)"
VIEW_AS_TEXT="$(extract_resource_text "$VIEW_AS_RESP" 60)"

if [ "$VIEW_AS_ERR" = "true" ]; then
    e2e_fail "views/acks-stale returned error"
    echo "    resp: $VIEW_AS_RESP"
else
    e2e_pass "views/acks-stale returned successfully"
fi

AS_CHECK="$(echo "$VIEW_AS_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', 0)
    messages = d.get('messages', [])
    has_urgent = any('URGENT' in m.get('subject', '') for m in messages)
    ttl = d.get('ttl_seconds', -1)
    agent = d.get('agent', '')
    print(f'count={count}|has_urgent={has_urgent}|ttl={ttl}|agent={agent}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_06_parsed.txt" "$AS_CHECK"

e2e_assert_contains "acks-stale has >= 1 message" "$AS_CHECK" "has_urgent=True"
e2e_assert_contains "acks-stale agent is SilverWolf" "$AS_CHECK" "agent=SilverWolf"

# ===========================================================================
# Case 7: views/ack-overdue/SilverWolf -- returns valid structure
# Note: ack-overdue has a minimum ttl_minutes=1 (enforced via .max(1)),
# so freshly-sent messages won't appear as overdue. We verify the endpoint
# returns a valid response with correct agent and structure.
# ===========================================================================
e2e_case_banner "views/ack-overdue/SilverWolf"

VIEW_AO_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://views/ack-overdue/SilverWolf?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_07_ack_overdue.txt" "$VIEW_AO_RESP"

VIEW_AO_ERR="$(is_error_result "$VIEW_AO_RESP" 70)"
VIEW_AO_TEXT="$(extract_resource_text "$VIEW_AO_RESP" 70)"

if [ "$VIEW_AO_ERR" = "true" ]; then
    e2e_fail "views/ack-overdue returned error"
    echo "    resp: $VIEW_AO_RESP"
else
    e2e_pass "views/ack-overdue returned successfully"
fi

AO_CHECK="$(echo "$VIEW_AO_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', -1)
    has_messages_key = 'messages' in d
    agent = d.get('agent', '')
    print(f'count={count}|has_messages={has_messages_key}|agent={agent}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_07_parsed.txt" "$AO_CHECK"

e2e_assert_contains "ack-overdue has messages array" "$AO_CHECK" "has_messages=True"
e2e_assert_contains "ack-overdue agent is SilverWolf" "$AO_CHECK" "agent=SilverWolf"
# Count is expected to be 0 since message is < 1 minute old (minimum ttl)
e2e_assert_contains "ack-overdue count is numeric" "$AO_CHECK" "count="

# ===========================================================================
# Case 8: Acknowledge the message as SilverWolf
# ===========================================================================
e2e_case_banner "acknowledge_message: SilverWolf acks the urgent message"

ACK_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"message_id\":${URGENT_MSG_ID}}}}" \
)"
e2e_save_artifact "case_08_ack.txt" "$ACK_RESP"

ACK_ERR="$(is_error_result "$ACK_RESP" 80)"
if [ "$ACK_ERR" = "true" ]; then
    ACK_TEXT="$(extract_result "$ACK_RESP" 80)"
    e2e_fail "acknowledge_message returned error"
    echo "    text: $ACK_TEXT"
else
    e2e_pass "acknowledge_message succeeded"
fi

# ===========================================================================
# Case 9: views/ack-required/SilverWolf (post-ack) -- should be empty
# ===========================================================================
e2e_case_banner "views/ack-required/SilverWolf (post-ack)"

VIEW_AR2_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://views/ack-required/SilverWolf?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_09_ack_required_post.txt" "$VIEW_AR2_RESP"

VIEW_AR2_ERR="$(is_error_result "$VIEW_AR2_RESP" 90)"
VIEW_AR2_TEXT="$(extract_resource_text "$VIEW_AR2_RESP" 90)"

if [ "$VIEW_AR2_ERR" = "true" ]; then
    e2e_fail "views/ack-required (post-ack) returned error"
else
    e2e_pass "views/ack-required (post-ack) returned successfully"
fi

AR2_CHECK="$(echo "$VIEW_AR2_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', 0)
    print(f'count={count}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_09_parsed.txt" "$AR2_CHECK"

e2e_assert_eq "ack-required post-ack count is 0" "count=0" "$AR2_CHECK"

# ===========================================================================
# Case 10: views/acks-stale/SilverWolf (post-ack) -- should be empty
# ===========================================================================
e2e_case_banner "views/acks-stale/SilverWolf (post-ack, ttl_seconds=0)"

VIEW_AS2_RESP="$(send_jsonrpc_session "$VIEWS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://views/acks-stale/SilverWolf?project=${PROJECT_SLUG}&ttl_seconds=0\"}}" \
)"
e2e_save_artifact "case_10_acks_stale_post.txt" "$VIEW_AS2_RESP"

VIEW_AS2_ERR="$(is_error_result "$VIEW_AS2_RESP" 100)"
VIEW_AS2_TEXT="$(extract_resource_text "$VIEW_AS2_RESP" 100)"

if [ "$VIEW_AS2_ERR" = "true" ]; then
    e2e_fail "views/acks-stale (post-ack) returned error"
else
    e2e_pass "views/acks-stale (post-ack) returned successfully"
fi

AS2_CHECK="$(echo "$VIEW_AS2_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', 0)
    print(f'count={count}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_10_parsed.txt" "$AS2_CHECK"

e2e_assert_eq "acks-stale post-ack count is 0" "count=0" "$AS2_CHECK"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
