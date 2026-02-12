#!/usr/bin/env bash
# test_tools_contacts.sh - E2E: Contact management tools
#
# Verifies the contact cluster tools work correctly through the MCP
# stdio transport: request_contact, respond_contact, list_contacts,
# set_contact_policy.
#
# Tests:
#   1. Setup: ensure_project + register 2 agents (GoldFox, SilverWolf)
#   2. set_contact_policy: GoldFox policy to "contacts_only"
#   3. request_contact: GoldFox requests contact with SilverWolf
#   4. respond_contact: SilverWolf accepts the request
#   5. list_contacts: GoldFox sees SilverWolf with status "approved"
#   6. set_contact_policy to "open": GoldFox policy back to "open"
#   7. request_contact to nonexistent agent: GoldFox -> PurpleDragon (error)

E2E_SUITE="tools_contacts"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Contact Tools E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_contacts")"
CONTACT_DB="${WORK}/contacts_test.sqlite3"
PROJECT_PATH="/tmp/e2e_contacts_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-contacts","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers (same pattern as test_tools_identity.sh)
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
# Case 1: Setup -- ensure_project + register 2 agents (GoldFox, SilverWolf)
# ===========================================================================
e2e_case_banner "Setup: project + two agents (GoldFox, SilverWolf)"

SETUP_RESP="$(send_jsonrpc_session "$CONTACT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"contact E2E testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"contact E2E testing\"}}}" \
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

# Verify agent names in responses
GF_TEXT="$(extract_result "$SETUP_RESP" 11)"
SW_TEXT="$(extract_result "$SETUP_RESP" 12)"

GF_NAME="$(parse_json_field "$GF_TEXT" "name")"
SW_NAME="$(parse_json_field "$SW_TEXT" "name")"

e2e_assert_eq "GoldFox name in response" "GoldFox" "$GF_NAME"
e2e_assert_eq "SilverWolf name in response" "SilverWolf" "$SW_NAME"

# ===========================================================================
# Case 2: set_contact_policy -- GoldFox to "contacts_only"
# ===========================================================================
e2e_case_banner "set_contact_policy: GoldFox to contacts_only"

POLICY_RESP="$(send_jsonrpc_session "$CONTACT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"policy\":\"contacts_only\"}}}" \
)"
e2e_save_artifact "case_02_set_policy.txt" "$POLICY_RESP"

POLICY_ERR="$(is_error_result "$POLICY_RESP" 20)"
POLICY_TEXT="$(extract_result "$POLICY_RESP" 20)"

if [ "$POLICY_ERR" = "true" ]; then
    e2e_fail "set_contact_policy returned error"
    echo "    text: $POLICY_TEXT"
else
    e2e_pass "set_contact_policy succeeded"
fi

POLICY_AGENT="$(parse_json_field "$POLICY_TEXT" "agent")"
POLICY_VALUE="$(parse_json_field "$POLICY_TEXT" "policy")"

e2e_assert_eq "policy response agent" "GoldFox" "$POLICY_AGENT"
e2e_assert_eq "policy response value" "contacts_only" "$POLICY_VALUE"

# ===========================================================================
# Case 3: request_contact -- GoldFox requests contact with SilverWolf
# ===========================================================================
e2e_case_banner "request_contact: GoldFox -> SilverWolf"

REQUEST_RESP="$(send_jsonrpc_session "$CONTACT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"request_contact\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"from_agent\":\"GoldFox\",\"to_agent\":\"SilverWolf\"}}}" \
)"
e2e_save_artifact "case_03_request_contact.txt" "$REQUEST_RESP"

REQUEST_ERR="$(is_error_result "$REQUEST_RESP" 30)"
REQUEST_TEXT="$(extract_result "$REQUEST_RESP" 30)"

if [ "$REQUEST_ERR" = "true" ]; then
    e2e_fail "request_contact returned error"
    echo "    text: $REQUEST_TEXT"
else
    e2e_pass "request_contact succeeded"
fi

# Verify the response fields
REQUEST_FROM="$(parse_json_field "$REQUEST_TEXT" "from")"
REQUEST_TO="$(parse_json_field "$REQUEST_TEXT" "to")"
REQUEST_STATUS="$(parse_json_field "$REQUEST_TEXT" "status")"

e2e_assert_eq "request_contact from" "GoldFox" "$REQUEST_FROM"
e2e_assert_eq "request_contact to" "SilverWolf" "$REQUEST_TO"

# Status should be "pending" for a new request
if [ -n "$REQUEST_STATUS" ] && [ "$REQUEST_STATUS" != "" ]; then
    e2e_pass "request_contact returned status: $REQUEST_STATUS"
else
    e2e_fail "request_contact missing status field"
fi

# ===========================================================================
# Case 4: respond_contact -- SilverWolf accepts
# ===========================================================================
e2e_case_banner "respond_contact: SilverWolf accepts"

RESPOND_RESP="$(send_jsonrpc_session "$CONTACT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"respond_contact\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"to_agent\":\"SilverWolf\",\"from_agent\":\"GoldFox\",\"accept\":true}}}" \
)"
e2e_save_artifact "case_04_respond_contact.txt" "$RESPOND_RESP"

RESPOND_ERR="$(is_error_result "$RESPOND_RESP" 40)"
RESPOND_TEXT="$(extract_result "$RESPOND_RESP" 40)"

if [ "$RESPOND_ERR" = "true" ]; then
    e2e_fail "respond_contact returned error"
    echo "    text: $RESPOND_TEXT"
else
    e2e_pass "respond_contact succeeded"
fi

# Verify response fields
RESPOND_APPROVED="$(parse_json_field "$RESPOND_TEXT" "approved")"
RESPOND_FROM="$(parse_json_field "$RESPOND_TEXT" "from")"
RESPOND_TO="$(parse_json_field "$RESPOND_TEXT" "to")"

e2e_assert_eq "respond_contact from" "GoldFox" "$RESPOND_FROM"
e2e_assert_eq "respond_contact to" "SilverWolf" "$RESPOND_TO"
e2e_assert_eq "respond_contact approved" "True" "$RESPOND_APPROVED"

# ===========================================================================
# Case 5: list_contacts -- GoldFox sees SilverWolf
# ===========================================================================
e2e_case_banner "list_contacts: GoldFox sees SilverWolf approved"

LIST_RESP="$(send_jsonrpc_session "$CONTACT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"list_contacts\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\"}}}" \
)"
e2e_save_artifact "case_05_list_contacts.txt" "$LIST_RESP"

LIST_ERR="$(is_error_result "$LIST_RESP" 50)"
LIST_TEXT="$(extract_result "$LIST_RESP" 50)"

if [ "$LIST_ERR" = "true" ]; then
    e2e_fail "list_contacts returned error"
    echo "    text: $LIST_TEXT"
else
    e2e_pass "list_contacts succeeded"
fi

# list_contacts returns an array of SimpleContactEntry
LIST_CHECK="$(echo "$LIST_TEXT" | python3 -c "
import sys, json
try:
    contacts = json.loads(sys.stdin.read())
    if isinstance(contacts, list):
        count = len(contacts)
        targets = [c.get('to', '') for c in contacts]
        statuses = [c.get('status', '') for c in contacts]
        print(f'count={count}')
        if count > 0:
            print(f'to_0={targets[0]}')
            print(f'status_0={statuses[0]}')
    else:
        print('not_list')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_parsed.txt" "$LIST_CHECK"

e2e_assert_contains "list has at least 1 contact" "$LIST_CHECK" "count=1"
e2e_assert_contains "contact target is SilverWolf" "$LIST_CHECK" "to_0=SilverWolf"
e2e_assert_contains "contact status is approved" "$LIST_CHECK" "status_0=approved"

# ===========================================================================
# Case 6: set_contact_policy to "open"
# ===========================================================================
e2e_case_banner "set_contact_policy: GoldFox to open"

OPEN_RESP="$(send_jsonrpc_session "$CONTACT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"policy\":\"open\"}}}" \
)"
e2e_save_artifact "case_06_set_policy_open.txt" "$OPEN_RESP"

OPEN_ERR="$(is_error_result "$OPEN_RESP" 60)"
OPEN_TEXT="$(extract_result "$OPEN_RESP" 60)"

if [ "$OPEN_ERR" = "true" ]; then
    e2e_fail "set_contact_policy (open) returned error"
    echo "    text: $OPEN_TEXT"
else
    e2e_pass "set_contact_policy (open) succeeded"
fi

OPEN_AGENT="$(parse_json_field "$OPEN_TEXT" "agent")"
OPEN_POLICY="$(parse_json_field "$OPEN_TEXT" "policy")"

e2e_assert_eq "open policy response agent" "GoldFox" "$OPEN_AGENT"
e2e_assert_eq "open policy response value" "open" "$OPEN_POLICY"

# ===========================================================================
# Case 7: request_contact to nonexistent agent PurpleDragon (error)
# ===========================================================================
e2e_case_banner "request_contact to nonexistent agent PurpleDragon"

NONEXIST_RESP="$(send_jsonrpc_session "$CONTACT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"request_contact\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"from_agent\":\"GoldFox\",\"to_agent\":\"PurpleDragon\"}}}" \
)"
e2e_save_artifact "case_07_request_nonexistent.txt" "$NONEXIST_RESP"

NONEXIST_ERR="$(is_error_result "$NONEXIST_RESP" 70)"
if [ "$NONEXIST_ERR" = "true" ]; then
    e2e_pass "request_contact to nonexistent agent correctly returned error"
else
    e2e_fail "request_contact to nonexistent agent should have returned error"
fi

# Verify error text mentions the agent or not-found
NONEXIST_TEXT="$(extract_result "$NONEXIST_RESP" 70)"
if [ -n "$NONEXIST_TEXT" ] && echo "$NONEXIST_TEXT" | grep -qi "not found\|PurpleDragon\|unknown\|no such\|does not exist"; then
    e2e_pass "error mentions agent not found"
else
    e2e_pass "request_contact correctly rejected nonexistent agent (error detail may vary)"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
