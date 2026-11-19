#!/usr/bin/env bash
# test_resource_errors.sh - E2E resource error handling tests
#
# Verifies the MCP server returns proper errors for:
#   1. resources/list returns a list of available resources
#   2. resources/read with valid URI (projects)
#   3. resources/read with unknown/nonexistent URI scheme
#   4. resources/read with nonexistent project slug
#   5. resources/read with nonexistent agent name
#   6. resources/read with non-numeric message ID
#   7. resources/read with nonexistent message ID
#   8. resources/read with empty URI
#   9. resources/read with malformed URI (no scheme)
#  10. resources/read with valid URI + bad query params
#  11. resources/read for inbox of nonexistent agent
#  12. resources/read for thread with nonexistent ID

E2E_SUITE="resource_errors"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Resource Error Handling E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_resource_errors")"
RES_DB="${WORK}/resource_errors.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-resource-errors","version":"1.0"}}}'

send_jsonrpc_session() {
    E2E_STDIO_TIMEOUT_SECONDS=3.6 RUST_LOG=error e2e_stdio_session "$@"
}

# Helper: check tool/resource call succeeded (id match, no isError)
assert_ok() {
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
        if d.get('id') == $id:
            if 'error' in d:
                print('JSON_RPC_ERROR')
                sys.exit(0)
            if 'result' in d:
                if d['result'].get('isError', False):
                    print('MCP_ERROR')
                else:
                    print('OK')
                sys.exit(0)
    except Exception: pass
print('NO_MATCH')
")"

    case "$check" in
        OK) e2e_pass "$label" ;;
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label → error: $check" ;;
        NO_MATCH) e2e_fail "$label → no response for id=$id" ;;
    esac
}

# Helper: check that a response for given id is an error (JSON-RPC or MCP)
assert_error() {
    local label="$1"
    local resp="$2"
    local id="$3"
    local expected_detail="${4:-}"

    local check
    check="$(echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id:
            if '$expected_detail'.lower() not in json.dumps(d).lower():
                print('WRONG_ERROR')
                sys.exit(0)
            if 'error' in d:
                code = d['error'].get('code')
                print('TRANSPORT_OR_INTERNAL_ERROR' if code in (-32700, -32600, -32603) else 'ERROR')
                sys.exit(0)
            r = d.get('result', {})
            if isinstance(r, dict) and r.get('isError', False):
                print('ERROR')
                sys.exit(0)
            print('OK')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
")"

    case "$check" in
        ERROR) e2e_pass "$label" ;;
        WRONG_ERROR) e2e_fail "$label → missing expected error detail: $expected_detail" ;;
        TRANSPORT_OR_INTERNAL_ERROR) e2e_fail "$label → transport or internal failure" ;;
        OK) e2e_fail "$label → expected error but got success" ;;
        NO_MATCH) e2e_fail "$label → no response for id=$id" ;;
    esac
}

# Helper: check response has result with contents array
assert_has_contents() {
    local label="$1"
    local resp="$2"
    local id="$3"
    local fixture="${4:-}"

    local check
    check="$(echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id:
            if 'error' in d:
                print('ERROR')
                sys.exit(0)
            r = d.get('result', {})
            if isinstance(r, dict) and r.get('contents'):
                payload = json.loads(r['contents'][0]['text']) if '$fixture' else None
                if '$fixture' == 'projects':
                    assert any(p['human_key'] == '/tmp/e2e_res_test' for p in payload)
                elif '$fixture' == 'message':
                    assert payload['id'] == 1 and payload['from'] == 'RedPeak'
                    assert payload['subject'] == 'Test msg' and payload['body_md'] == 'Hello'
                elif '$fixture' == 'inbox':
                    assert payload['agent'] == 'RedPeak' and payload['count'] == 1
                    assert payload['messages'][0]['subject'] == 'Test msg'
                elif '$fixture' == 'empty_thread':
                    assert payload['thread_id'] == 'TKT-99999' and payload['messages'] == []
                print('HAS_CONTENTS')
                sys.exit(0)
            print('NO_CONTENTS')
            sys.exit(0)
    except Exception as error:
        print(f'Invalid resource contents: {error}', file=sys.stderr)
        print('INVALID_CONTENTS')
        sys.exit(0)
print('NO_MATCH')
")"

    case "$check" in
        HAS_CONTENTS) e2e_pass "$label" ;;
        ERROR) e2e_fail "$label → got error" ;;
        NO_CONTENTS) e2e_fail "$label → no contents in result" ;;
        INVALID_CONTENTS) e2e_fail "$label → contents did not match the fixture" ;;
        NO_MATCH) e2e_fail "$label → no response for id=$id" ;;
    esac
}

# ===========================================================================
# Setup: create a project and agent for valid resource tests
# ===========================================================================
e2e_case_banner "Setup project + agent"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_res_test"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_res_test","program":"test","model":"test","name":"RedPeak"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_res_test","sender_name":"RedPeak","to":["RedPeak"],"subject":"Test msg","body_md":"Hello"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure_project" "$RESP" 10
assert_ok "register RedPeak" "$RESP" 11
assert_ok "send test message" "$RESP" 12

# ===========================================================================
# Case 1: resources/list returns valid listing
# ===========================================================================
e2e_case_banner "resources/list"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"resources/list","params":{}}' \
)"
e2e_save_artifact "case_01_resources_list.txt" "$RESP"

# Check we got a result with resources array
LIST_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 100 and 'result' in d:
            r = d['result']
            resources = r.get('resources', r.get('resourceTemplates', []))
            if isinstance(resources, list) and len(resources) > 0:
                print(f'COUNT={len(resources)}')
            else:
                print('EMPTY')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
")"

e2e_assert_contains "resources/list returns resources" "$LIST_CHECK" "COUNT="

# ===========================================================================
# Case 2: resources/read with valid URI (projects)
# ===========================================================================
e2e_case_banner "Valid resource read (projects)"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"resources/read","params":{"uri":"resource://projects?format=json"}}' \
)"
e2e_save_artifact "case_02_valid_projects.txt" "$RESP"
assert_has_contents "resources/read projects contains fixture" "$RESP" 200 projects

# ===========================================================================
# Case 3: resources/read with unknown URI scheme
# ===========================================================================
e2e_case_banner "Unknown resource URI"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"resources/read","params":{"uri":"resource://definitely-not-real/foo"}}' \
)"
e2e_save_artifact "case_03_unknown_uri.txt" "$RESP"
assert_error "unknown resource URI returns error" "$RESP" 300

# ===========================================================================
# Case 4: resources/read with nonexistent project slug
# ===========================================================================
e2e_case_banner "Nonexistent project slug"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"resources/read","params":{"uri":"resource://project/nonexistent-slug-xyz"}}' \
)"
e2e_save_artifact "case_04_nonexistent_project.txt" "$RESP"
assert_error "nonexistent project slug returns error" "$RESP" 400 "Project not found"

# ===========================================================================
# Case 5: resources/read with nonexistent agent
# ===========================================================================
e2e_case_banner "Nonexistent agent in agents resource"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"resources/read","params":{"uri":"resource://agents/nonexistent-slug-xyz"}}' \
)"
e2e_save_artifact "case_05_nonexistent_agent_agents.txt" "$RESP"
assert_error "nonexistent project in agents returns error" "$RESP" 500 "Project not found"

# ===========================================================================
# Case 6: resources/read with non-numeric message ID
# ===========================================================================
e2e_case_banner "Non-numeric message ID"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":600,"method":"resources/read","params":{"uri":"resource://message/not-a-number?project=tmp-e2e-res-test"}}' \
)"
e2e_save_artifact "case_06_nonnumeric_message_id.txt" "$RESP"
assert_error "non-numeric message ID returns error" "$RESP" 600 "Invalid message ID"

# ===========================================================================
# Case 7: resources/read with nonexistent message ID
# ===========================================================================
e2e_case_banner "Nonexistent message ID"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":700,"method":"resources/read","params":{"uri":"resource://message/999999?project=tmp-e2e-res-test"}}' \
)"
e2e_save_artifact "case_07_nonexistent_message.txt" "$RESP"
assert_error "nonexistent message ID returns error" "$RESP" 700 "not found"

# ===========================================================================
# Case 8: resources/read with empty URI
# ===========================================================================
e2e_case_banner "Empty URI"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":800,"method":"resources/read","params":{"uri":""}}' \
)"
e2e_save_artifact "case_08_empty_uri.txt" "$RESP"
assert_error "empty URI returns error" "$RESP" 800

# ===========================================================================
# Case 9: resources/read with malformed URI (no resource:// prefix)
# ===========================================================================
e2e_case_banner "Malformed URI (no scheme)"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":900,"method":"resources/read","params":{"uri":"just-some-string"}}' \
)"
e2e_save_artifact "case_09_malformed_uri.txt" "$RESP"
assert_error "malformed URI returns error" "$RESP" 900

# ===========================================================================
# Case 10: resources/read with inbox for nonexistent agent
# ===========================================================================
e2e_case_banner "Inbox for nonexistent agent"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1000,"method":"resources/read","params":{"uri":"resource://inbox/GhostAgent?project=tmp-e2e-res-test"}}' \
)"
e2e_save_artifact "case_10_inbox_ghost_agent.txt" "$RESP"
assert_error "inbox for nonexistent agent returns error" "$RESP" 1000 "Agent not found"

# ===========================================================================
# Case 11: resources/read for thread with nonexistent ID
# ===========================================================================
e2e_case_banner "Nonexistent thread ID"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1100,"method":"resources/read","params":{"uri":"resource://thread/TKT-99999?project=tmp-e2e-res-test&format=json"}}' \
)"
e2e_save_artifact "case_11_nonexistent_thread.txt" "$RESP"
assert_has_contents "nonexistent thread returns an empty message list" "$RESP" 1100 empty_thread

# ===========================================================================
# Case 12: resources/read message without project param (expect error)
# ===========================================================================
e2e_case_banner "Message without project param"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1200,"method":"resources/read","params":{"uri":"resource://message/1"}}' \
)"
e2e_save_artifact "case_12_message_no_project.txt" "$RESP"
assert_error "message/1 without project param returns error" "$RESP" 1200 "project query parameter is required"

# ===========================================================================
# Case 12b: resources/read valid message with project param
# ===========================================================================
e2e_case_banner "Valid message with project param"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1201,"method":"resources/read","params":{"uri":"resource://message/1?project=tmp-e2e-res-test&format=json"}}' \
)"
e2e_save_artifact "case_12b_valid_message.txt" "$RESP"
assert_has_contents "resources/read message/1 matches fixture" "$RESP" 1201 message

# ===========================================================================
# Case 13: resources/read with missing uri param
# ===========================================================================
e2e_case_banner "Missing uri param"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1300,"method":"resources/read","params":{}}' \
)"
e2e_save_artifact "case_13_missing_uri.txt" "$RESP"
assert_error "missing uri param returns error" "$RESP" 1300

# ===========================================================================
# Case 14: resources/read inbox without project param (expect error)
# ===========================================================================
e2e_case_banner "Inbox without project param"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1400,"method":"resources/read","params":{"uri":"resource://inbox/RedPeak"}}' \
)"
e2e_save_artifact "case_14_inbox_no_project.txt" "$RESP"
assert_error "inbox without project param returns error" "$RESP" 1400 "project query parameter is required"

# ===========================================================================
# Case 14b: resources/read valid inbox with project param
# ===========================================================================
e2e_case_banner "Valid inbox with project param"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1401,"method":"resources/read","params":{"uri":"resource://inbox/RedPeak?project=tmp-e2e-res-test&format=json"}}' \
)"
e2e_save_artifact "case_14b_valid_inbox.txt" "$RESP"
assert_has_contents "resources/read inbox/RedPeak matches fixture" "$RESP" 1401 inbox

# ===========================================================================
# Case 15: resources/read tooling/directory (no project needed)
# ===========================================================================
e2e_case_banner "Tooling directory"

RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1500,"method":"resources/read","params":{"uri":"resource://tooling/directory"}}' \
)"
e2e_save_artifact "case_15_tooling_directory.txt" "$RESP"
assert_has_contents "resources/read tooling/directory" "$RESP" 1500

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
