#!/usr/bin/env bash
# test_resources_mailbox.sh - E2E: Mailbox-related MCP resource endpoints
#
# Verifies the mailbox, mailbox-with-commits, outbox, and inbox resource
# reading lifecycle through the MCP stdio transport.
#
# Tests:
#   1. Setup: ensure_project + register 2 agents (GoldFox, SilverWolf)
#   2. Send a message from GoldFox to SilverWolf
#   3. Read resource://mailbox/SilverWolf - verify message appears in inbox
#   4. Read resource://outbox/GoldFox - verify message appears in outbox
#   5. Read resource://mailbox-with-commits/SilverWolf - verify commit metadata
#   6. Read resource://inbox/SilverWolf - verify alternative inbox resource
#   7. Verify empty mailbox for GoldFox (no inbound messages)
#   8. Send second message + verify outbox count increases
#   9. Read mailbox without project param returns error

E2E_SUITE="resources_mailbox"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Mailbox Resources E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_res_mailbox")"
MBOX_DB="${WORK}/mailbox_test.sqlite3"
PROJECT_PATH="/tmp/e2e_mailbox_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-resources-mailbox","version":"1.0"}}}'

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

# Extract result text from a tools/call response (result.content[0].text)
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

# Extract text from a resources/read response (result.contents[0].text)
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
            # Resources use 'contents' (plural), not 'content'
            contents = d['result'].get('contents', [])
            if contents:
                print(contents[0].get('text', ''))
                sys.exit(0)
            # Fall back to 'content' just in case
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

# Compute the expected project slug from the project path
# slugify: lowercase, replace non-alnum with -, collapse runs, trim dashes
PROJECT_SLUG="$(echo "$PROJECT_PATH" | python3 -c "
import sys, re
raw = sys.stdin.read().strip()
# slugify: lowercase, non-alnum -> dash, collapse, trim
s = raw.lower()
out = []
prev_dash = False
for ch in s:
    if ch.isalnum():
        out.append(ch)
        prev_dash = False
    elif not prev_dash:
        out.append('-')
        prev_dash = True
result = ''.join(out).strip('-')
print(result if result else 'project')
" 2>/dev/null)"

e2e_log "Project path: $PROJECT_PATH"
e2e_log "Project slug: $PROJECT_SLUG"

# ===========================================================================
# Case 1: Setup -- ensure_project + register 2 agents (GoldFox, SilverWolf)
# ===========================================================================
e2e_case_banner "Setup: project + two agents (GoldFox, SilverWolf)"

SETUP_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"mailbox resource E2E testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"mailbox resource E2E testing\"}}}" \
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
# Case 2: Send a message from GoldFox to SilverWolf
# ===========================================================================
e2e_case_banner "send_message: GoldFox -> SilverWolf"

SEND_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Architecture review\",\"body_md\":\"Please review the new module layout.\",\"thread_id\":\"ARCH-1\",\"importance\":\"high\"}}}" \
)"
e2e_save_artifact "case_02_send.txt" "$SEND_RESP"

SEND_ERR="$(is_error_result "$SEND_RESP" 20)"
SEND_TEXT="$(extract_result "$SEND_RESP" 20)"
if [ "$SEND_ERR" = "true" ]; then
    e2e_fail "send_message returned error"
    echo "    text: $SEND_TEXT"
else
    e2e_pass "send_message succeeded"
fi

MSG_ID="$(parse_json_field "$SEND_TEXT" "deliveries.0.payload.id")"
if [ -n "$MSG_ID" ] && [ "$MSG_ID" != "" ] && [ "$MSG_ID" != "None" ]; then
    e2e_pass "send_message returned message id: $MSG_ID"
else
    e2e_fail "send_message missing id"
fi

# ===========================================================================
# Case 3: Read resource://mailbox/SilverWolf - verify message in inbox
# ===========================================================================
e2e_case_banner "resource://mailbox/SilverWolf"

MAILBOX_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://mailbox/SilverWolf?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_03_mailbox.txt" "$MAILBOX_RESP"

MAILBOX_ERR="$(is_error_result "$MAILBOX_RESP" 30)"
if [ "$MAILBOX_ERR" = "true" ]; then
    e2e_fail "resource://mailbox/SilverWolf returned error"
else
    e2e_pass "resource://mailbox/SilverWolf succeeded"
fi

MAILBOX_TEXT="$(extract_resource_text "$MAILBOX_RESP" 30)"
e2e_save_artifact "case_03_mailbox_text.txt" "$MAILBOX_TEXT"

# Parse the mailbox response JSON
MAILBOX_CHECK="$(echo "$MAILBOX_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', -1)
    agent = d.get('agent', '')
    project = d.get('project', '')
    messages = d.get('messages', [])
    msg_count = len(messages)
    subject = messages[0].get('subject', '') if messages else ''
    sender = messages[0].get('from', '') if messages else ''
    has_commit = 'commit' in messages[0] if messages else False
    print(f'count={count}')
    print(f'agent={agent}')
    print(f'msg_count={msg_count}')
    print(f'subject={subject}')
    print(f'sender={sender}')
    print(f'has_commit={has_commit}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_03_parsed.txt" "$MAILBOX_CHECK"

e2e_assert_contains "mailbox count is 1" "$MAILBOX_CHECK" "count=1"
e2e_assert_contains "mailbox agent is SilverWolf" "$MAILBOX_CHECK" "agent=SilverWolf"
e2e_assert_contains "mailbox has 1 message" "$MAILBOX_CHECK" "msg_count=1"
e2e_assert_contains "mailbox subject correct" "$MAILBOX_CHECK" "subject=Architecture review"
e2e_assert_contains "mailbox sender is GoldFox" "$MAILBOX_CHECK" "sender=GoldFox"
e2e_assert_contains "mailbox has commit metadata" "$MAILBOX_CHECK" "has_commit=True"

# ===========================================================================
# Case 4: Read resource://outbox/GoldFox - verify message in outbox
# ===========================================================================
e2e_case_banner "resource://outbox/GoldFox"

OUTBOX_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://outbox/GoldFox?project=${PROJECT_SLUG}&include_bodies=true\"}}" \
)"
e2e_save_artifact "case_04_outbox.txt" "$OUTBOX_RESP"

OUTBOX_ERR="$(is_error_result "$OUTBOX_RESP" 40)"
if [ "$OUTBOX_ERR" = "true" ]; then
    e2e_fail "resource://outbox/GoldFox returned error"
else
    e2e_pass "resource://outbox/GoldFox succeeded"
fi

OUTBOX_TEXT="$(extract_resource_text "$OUTBOX_RESP" 40)"
e2e_save_artifact "case_04_outbox_text.txt" "$OUTBOX_TEXT"

OUTBOX_CHECK="$(echo "$OUTBOX_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', -1)
    agent = d.get('agent', '')
    messages = d.get('messages', [])
    msg_count = len(messages)
    subject = messages[0].get('subject', '') if messages else ''
    sender = messages[0].get('from', '') if messages else ''
    has_body = bool(messages[0].get('body_md', '')) if messages else False
    has_to = isinstance(messages[0].get('to', None), list) if messages else False
    has_commit = 'commit' in messages[0] if messages else False
    print(f'count={count}')
    print(f'agent={agent}')
    print(f'msg_count={msg_count}')
    print(f'subject={subject}')
    print(f'sender={sender}')
    print(f'has_body={has_body}')
    print(f'has_to={has_to}')
    print(f'has_commit={has_commit}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_04_parsed.txt" "$OUTBOX_CHECK"

e2e_assert_contains "outbox count is 1" "$OUTBOX_CHECK" "count=1"
e2e_assert_contains "outbox agent is GoldFox" "$OUTBOX_CHECK" "agent=GoldFox"
e2e_assert_contains "outbox has 1 message" "$OUTBOX_CHECK" "msg_count=1"
e2e_assert_contains "outbox subject correct" "$OUTBOX_CHECK" "subject=Architecture review"
e2e_assert_contains "outbox sender is GoldFox" "$OUTBOX_CHECK" "sender=GoldFox"
e2e_assert_contains "outbox has body_md" "$OUTBOX_CHECK" "has_body=True"
e2e_assert_contains "outbox has to list" "$OUTBOX_CHECK" "has_to=True"
e2e_assert_contains "outbox has commit metadata" "$OUTBOX_CHECK" "has_commit=True"

# ===========================================================================
# Case 5: Read resource://mailbox-with-commits/SilverWolf
# ===========================================================================
e2e_case_banner "resource://mailbox-with-commits/SilverWolf"

MBOX_COMMITS_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://mailbox-with-commits/SilverWolf?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_05_mailbox_commits.txt" "$MBOX_COMMITS_RESP"

MBOX_COMMITS_ERR="$(is_error_result "$MBOX_COMMITS_RESP" 50)"
if [ "$MBOX_COMMITS_ERR" = "true" ]; then
    e2e_fail "resource://mailbox-with-commits/SilverWolf returned error"
else
    e2e_pass "resource://mailbox-with-commits/SilverWolf succeeded"
fi

MBOX_COMMITS_TEXT="$(extract_resource_text "$MBOX_COMMITS_RESP" 50)"
e2e_save_artifact "case_05_mailbox_commits_text.txt" "$MBOX_COMMITS_TEXT"

MBOX_COMMITS_CHECK="$(echo "$MBOX_COMMITS_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', -1)
    agent = d.get('agent', '')
    messages = d.get('messages', [])
    msg_count = len(messages)
    subject = messages[0].get('subject', '') if messages else ''
    # mailbox-with-commits has full commit metadata
    commit = messages[0].get('commit', {}) if messages else {}
    has_summary = bool(commit.get('summary', ''))
    has_insertions = 'insertions' in commit
    has_deletions = 'deletions' in commit
    has_diff_summary = 'diff_summary' in commit
    print(f'count={count}')
    print(f'agent={agent}')
    print(f'msg_count={msg_count}')
    print(f'subject={subject}')
    print(f'has_summary={has_summary}')
    print(f'has_insertions={has_insertions}')
    print(f'has_deletions={has_deletions}')
    print(f'has_diff_summary={has_diff_summary}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_parsed.txt" "$MBOX_COMMITS_CHECK"

e2e_assert_contains "mailbox-with-commits count is 1" "$MBOX_COMMITS_CHECK" "count=1"
e2e_assert_contains "mailbox-with-commits agent is SilverWolf" "$MBOX_COMMITS_CHECK" "agent=SilverWolf"
e2e_assert_contains "mailbox-with-commits has 1 message" "$MBOX_COMMITS_CHECK" "msg_count=1"
e2e_assert_contains "mailbox-with-commits subject correct" "$MBOX_COMMITS_CHECK" "subject=Architecture review"
e2e_assert_contains "commit has summary" "$MBOX_COMMITS_CHECK" "has_summary=True"
e2e_assert_contains "commit has insertions" "$MBOX_COMMITS_CHECK" "has_insertions=True"
e2e_assert_contains "commit has deletions" "$MBOX_COMMITS_CHECK" "has_deletions=True"
e2e_assert_contains "commit has diff_summary" "$MBOX_COMMITS_CHECK" "has_diff_summary=True"

# ===========================================================================
# Case 6: Read resource://inbox/SilverWolf - alternative inbox resource
# ===========================================================================
e2e_case_banner "resource://inbox/SilverWolf"

INBOX_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/SilverWolf?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_06_inbox.txt" "$INBOX_RESP"

INBOX_ERR="$(is_error_result "$INBOX_RESP" 60)"
if [ "$INBOX_ERR" = "true" ]; then
    e2e_fail "resource://inbox/SilverWolf returned error"
else
    e2e_pass "resource://inbox/SilverWolf succeeded"
fi

INBOX_TEXT="$(extract_resource_text "$INBOX_RESP" 60)"
e2e_save_artifact "case_06_inbox_text.txt" "$INBOX_TEXT"

INBOX_CHECK="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    # inbox resource format: has messages array
    messages = d.get('messages', [])
    count = d.get('count', len(messages))
    agent = d.get('agent', '')
    subject = messages[0].get('subject', '') if messages else ''
    sender = messages[0].get('from', '') if messages else ''
    print(f'count={count}')
    print(f'agent={agent}')
    print(f'subject={subject}')
    print(f'sender={sender}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_06_parsed.txt" "$INBOX_CHECK"

e2e_assert_contains "inbox count is 1" "$INBOX_CHECK" "count=1"
e2e_assert_contains "inbox agent is SilverWolf" "$INBOX_CHECK" "agent=SilverWolf"
e2e_assert_contains "inbox subject correct" "$INBOX_CHECK" "subject=Architecture review"
e2e_assert_contains "inbox sender is GoldFox" "$INBOX_CHECK" "sender=GoldFox"

# ===========================================================================
# Case 7: Verify empty mailbox for GoldFox (no inbound messages)
# ===========================================================================
e2e_case_banner "Empty mailbox: GoldFox has no inbound messages"

EMPTY_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://mailbox/GoldFox?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_07_empty_mailbox.txt" "$EMPTY_RESP"

EMPTY_ERR="$(is_error_result "$EMPTY_RESP" 70)"
if [ "$EMPTY_ERR" = "true" ]; then
    e2e_fail "resource://mailbox/GoldFox returned error"
else
    e2e_pass "resource://mailbox/GoldFox succeeded (empty)"
fi

EMPTY_TEXT="$(extract_resource_text "$EMPTY_RESP" 70)"
e2e_save_artifact "case_07_empty_text.txt" "$EMPTY_TEXT"

EMPTY_CHECK="$(echo "$EMPTY_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', -1)
    agent = d.get('agent', '')
    messages = d.get('messages', [])
    msg_count = len(messages)
    print(f'count={count}')
    print(f'agent={agent}')
    print(f'msg_count={msg_count}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_07_parsed.txt" "$EMPTY_CHECK"

e2e_assert_contains "GoldFox mailbox count is 0" "$EMPTY_CHECK" "count=0"
e2e_assert_contains "GoldFox mailbox agent correct" "$EMPTY_CHECK" "agent=GoldFox"
e2e_assert_contains "GoldFox mailbox has 0 messages" "$EMPTY_CHECK" "msg_count=0"

# ===========================================================================
# Case 8: Send second message + verify outbox count increases
# ===========================================================================
e2e_case_banner "Send second message + verify outbox count"

SEND2_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Follow-up on architecture\",\"body_md\":\"Any thoughts on the new module?\",\"importance\":\"normal\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":81,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://outbox/GoldFox?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_08_send2_outbox.txt" "$SEND2_RESP"

SEND2_ERR="$(is_error_result "$SEND2_RESP" 80)"
if [ "$SEND2_ERR" = "true" ]; then
    e2e_fail "second send_message returned error"
else
    e2e_pass "second send_message succeeded"
fi

OUTBOX2_TEXT="$(extract_resource_text "$SEND2_RESP" 81)"
e2e_save_artifact "case_08_outbox2_text.txt" "$OUTBOX2_TEXT"

OUTBOX2_CHECK="$(echo "$OUTBOX2_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    count = d.get('count', -1)
    messages = d.get('messages', [])
    subjects = [m.get('subject', '') for m in messages]
    print(f'count={count}')
    print(f'subjects={subjects}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_08_parsed.txt" "$OUTBOX2_CHECK"

e2e_assert_contains "outbox now has 2 messages" "$OUTBOX2_CHECK" "count=2"

# ===========================================================================
# Case 9: Read mailbox without project param returns error
# ===========================================================================
e2e_case_banner "Mailbox without project param returns error"

NOPROJ_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":90,"method":"resources/read","params":{"uri":"resource://mailbox/SilverWolf"}}' \
)"
e2e_save_artifact "case_09_no_project.txt" "$NOPROJ_RESP"

NOPROJ_ERR="$(is_error_result "$NOPROJ_RESP" 90)"
if [ "$NOPROJ_ERR" = "true" ]; then
    e2e_pass "mailbox without project param correctly returned error"
else
    e2e_fail "mailbox without project param should have returned error"
fi

# Also verify outbox without project param
NOPROJ_OUT_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":91,"method":"resources/read","params":{"uri":"resource://outbox/GoldFox"}}' \
)"
e2e_save_artifact "case_09_no_project_outbox.txt" "$NOPROJ_OUT_RESP"

NOPROJ_OUT_ERR="$(is_error_result "$NOPROJ_OUT_RESP" 91)"
if [ "$NOPROJ_OUT_ERR" = "true" ]; then
    e2e_pass "outbox without project param correctly returned error"
else
    e2e_fail "outbox without project param should have returned error"
fi

# Also verify mailbox-with-commits without project param
NOPROJ_MWC_RESP="$(send_jsonrpc_session "$MBOX_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":92,"method":"resources/read","params":{"uri":"resource://mailbox-with-commits/SilverWolf"}}' \
)"
e2e_save_artifact "case_09_no_project_mwc.txt" "$NOPROJ_MWC_RESP"

NOPROJ_MWC_ERR="$(is_error_result "$NOPROJ_MWC_RESP" 92)"
if [ "$NOPROJ_MWC_ERR" = "true" ]; then
    e2e_pass "mailbox-with-commits without project param correctly returned error"
else
    e2e_fail "mailbox-with-commits without project param should have returned error"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
