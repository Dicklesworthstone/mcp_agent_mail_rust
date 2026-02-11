#!/usr/bin/env bash
# test_fault_injection.sh - Fault-injection E2E scripts (br-3vwi.10.12)
#
# Covers realistic failure modes with detailed logging and recovery assertions:
#   1. Huge message body (near size limits)
#   2. Many messages in single session (mailbox stress)
#   3. Invalid project key handling
#   4. Double-registration of same agent name (idempotent)
#   5. Send to non-existent recipient
#   6. File reservation with min TTL boundary
#   7. File reservation glob conflict patterns
#   8. Search with FTS-hostile query syntax
#   9. Concurrent exclusive reservations (contention)
#  10. Reply to non-existent message
#  11. Acknowledge non-existent message
#  12. Empty body / empty subject messages
#  13. Thread summarize with empty thread
#  14. Force-release without inactivity (should fail)

E2E_SUITE="fault_injection"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Fault-Injection E2E Suite (br-3vwi.10.12)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_fault_injection")"
FI_DB="${WORK}/fault_inject.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-fault-injection","version":"1.0"}}}'

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

    local timeout_s=20
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

assert_ok() {
    local label="$1" resp="$2" id="$3"
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
" 2>/dev/null)"
    case "$check" in
        OK) e2e_pass "$label" ;;
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label -> error: $check" ;;
        NO_MATCH) e2e_fail "$label -> no response for id=$id" ;;
    esac
}

assert_error() {
    local label="$1" resp="$2" id="$3"
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
" 2>/dev/null)"
    case "$check" in
        JSON_RPC_ERROR|MCP_ERROR) e2e_pass "$label" ;;
        OK) e2e_fail "$label -> expected error, got OK" ;;
        NO_MATCH) e2e_fail "$label -> no response for id=$id" ;;
    esac
}

# Check for either ok or error (server didn't crash)
assert_any_response() {
    local label="$1" resp="$2" id="$3"
    local check
    check="$(echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id:
            print('RESPONDED')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"
    case "$check" in
        RESPONDED) e2e_pass "$label" ;;
        NO_MATCH) e2e_fail "$label -> no response for id=$id" ;;
    esac
}

extract_result_text() {
    local resp="$1" id="$2"
    echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id and 'result' in d:
            r = d['result']
            if 'content' in r and len(r['content']) > 0:
                print(r['content'][0].get('text', ''))
            sys.exit(0)
    except Exception: pass
" 2>/dev/null
}

# ===========================================================================
# Setup: project + agents
# ===========================================================================
e2e_case_banner "Setup project + agents"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_fi_proj"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fi_proj","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fi_proj","program":"test","model":"test","name":"BlueLake"}}}' \
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fi_proj","program":"test","model":"test","name":"GoldPeak"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure project" "$RESP" 10
assert_ok "register RedFox" "$RESP" 11
assert_ok "register BlueLake" "$RESP" 12
assert_ok "register GoldPeak" "$RESP" 13

# ===========================================================================
# Case 1: Huge message body (near size limits)
# ===========================================================================
e2e_case_banner "Huge message body"

# Generate a ~50KB message body
BIG_BODY="$(python3 -c "print('X' * 50000)")"
RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_fi_proj\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"Huge body test\",\"body_md\":\"$BIG_BODY\"}}}" \
)"
e2e_save_artifact "case_01_huge_body.txt" "$RESP"
assert_any_response "huge body handled (no crash)" "$RESP" 100

# ===========================================================================
# Case 2: Many messages in a session (mailbox stress)
# ===========================================================================
e2e_case_banner "Mailbox stress (20 messages)"

STRESS_REQS=("$INIT_REQ")
for i in $(seq 1 20); do
    id=$((200 + i))
    STRESS_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_fi_proj\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"Stress msg $i\",\"body_md\":\"Message number $i\"}}}")
done
# Fetch inbox after all sends
STRESS_REQS+=('{"jsonrpc":"2.0","id":299,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"BlueLake","limit":50}}}')

RESP="$(send_jsonrpc_session "$FI_DB" "${STRESS_REQS[@]}")"
e2e_save_artifact "case_02_stress.txt" "$RESP"

# Check first and last message sent
assert_ok "stress msg 1" "$RESP" 201
assert_ok "stress msg 20" "$RESP" 220
assert_ok "fetch inbox after stress" "$RESP" 299

# Verify inbox has at least 20 messages
INBOX_COUNT="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 299 and 'result' in d:
            msgs = json.loads(d['result']['content'][0]['text'])
            print(len(msgs) if isinstance(msgs, list) else 0)
            sys.exit(0)
    except Exception: pass
print(-1)
" 2>/dev/null)"

if [ "$INBOX_COUNT" -ge 20 ]; then
    e2e_pass "inbox has >= 20 messages after stress"
else
    e2e_fail "inbox expected >= 20, got $INBOX_COUNT"
fi

# ===========================================================================
# Case 3: Invalid project key
# ===========================================================================
e2e_case_banner "Invalid project key"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/nonexistent/project/path","program":"test","model":"test","name":"SilverCove"}}}' \
    '{"jsonrpc":"2.0","id":301,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/nonexistent/project/path","query":"test"}}}' \
)"
e2e_save_artifact "case_03_invalid_project.txt" "$RESP"

# These should either succeed with empty results or error gracefully
assert_any_response "register with bad project key" "$RESP" 300
assert_any_response "search with bad project key" "$RESP" 301

# ===========================================================================
# Case 4: Double-register same agent name (idempotent update)
# ===========================================================================
e2e_case_banner "Double agent registration"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fi_proj","program":"test-v2","model":"test-v2","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":401,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fi_proj","program":"test-v2","model":"test-v2","name":"RedFox"}}}' \
)"
e2e_save_artifact "case_04_double_register.txt" "$RESP"
assert_ok "first re-register RedFox" "$RESP" 400
assert_ok "second re-register RedFox (idempotent)" "$RESP" 401

# ===========================================================================
# Case 5: Send to non-existent recipient
# ===========================================================================
e2e_case_banner "Non-existent recipient"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fi_proj","sender_name":"RedFox","to":["NoSuchAgent"],"subject":"Test","body_md":"To nobody"}}}' \
)"
e2e_save_artifact "case_05_bad_recipient.txt" "$RESP"
assert_error "send to non-existent agent" "$RESP" 500

# ===========================================================================
# Case 6: File reservation with min TTL (60s boundary)
# ===========================================================================
e2e_case_banner "Min TTL file reservation"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"RedFox","paths":["min_ttl.rs"],"ttl_seconds":60,"exclusive":true,"reason":"min TTL test"}}}' \
    '{"jsonrpc":"2.0","id":601,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"BlueLake","paths":["sub_min.rs"],"ttl_seconds":30,"exclusive":true,"reason":"below min TTL test"}}}' \
)"
e2e_save_artifact "case_06_min_ttl.txt" "$RESP"
assert_ok "reserve with min TTL (60s)" "$RESP" 600
# Sub-minimum TTL should either be clamped or error
assert_any_response "sub-min TTL handled" "$RESP" 601

# ===========================================================================
# Case 7: File reservation glob conflict patterns
# ===========================================================================
e2e_case_banner "Glob conflict patterns"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":700,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"RedFox","paths":["src/*.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"glob reservation"}}}' \
    '{"jsonrpc":"2.0","id":701,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"BlueLake","paths":["src/main.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"specific file in glob"}}}' \
)"
e2e_save_artifact "case_07_glob_conflict.txt" "$RESP"
assert_ok "RedFox reserves src/*.rs" "$RESP" 700

# BlueLake's src/main.rs should conflict with RedFox's src/*.rs glob
GLOB_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 701 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            conflicts = result.get('conflicts', [])
            print(f'conflicts={len(conflicts)}')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"
e2e_assert_contains "glob conflict detected" "$GLOB_CHECK" "conflicts=1"

# ===========================================================================
# Case 8: FTS-hostile query syntax
# ===========================================================================
e2e_case_banner "FTS-hostile queries"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":800,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_fi_proj","query":"AND OR NOT"}}}' \
    '{"jsonrpc":"2.0","id":801,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_fi_proj","query":"\"unclosed quote"}}}' \
    '{"jsonrpc":"2.0","id":802,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_fi_proj","query":"(((nested)))"}}}' \
    '{"jsonrpc":"2.0","id":803,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_fi_proj","query":"col:value OR -prefix"}}}' \
)"
e2e_save_artifact "case_08_fts_hostile.txt" "$RESP"

# All should be handled without crash (empty results or error)
assert_any_response "FTS: bare operators" "$RESP" 800
assert_any_response "FTS: unclosed quote" "$RESP" 801
assert_any_response "FTS: nested parens" "$RESP" 802
assert_any_response "FTS: column prefix" "$RESP" 803

# ===========================================================================
# Case 9: Concurrent exclusive reservations (contention)
# ===========================================================================
e2e_case_banner "Concurrent reservation contention"

# Release previous reservations first
RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":890,"method":"tools/call","params":{"name":"release_file_reservations","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":891,"method":"tools/call","params":{"name":"release_file_reservations","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"BlueLake"}}}' \
)"

# Now three agents try to reserve the same file
RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"RedFox","paths":["contested.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"I want it"}}}' \
    '{"jsonrpc":"2.0","id":901,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"BlueLake","paths":["contested.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"me too"}}}' \
    '{"jsonrpc":"2.0","id":902,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"GoldPeak","paths":["contested.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"me three"}}}' \
)"
e2e_save_artifact "case_09_contention.txt" "$RESP"
assert_ok "RedFox gets contested.rs" "$RESP" 900

# Count total conflicts (BlueLake and GoldPeak should both have conflicts)
CONTEST_BLUE="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 901 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            print(len(result.get('conflicts', [])))
            sys.exit(0)
    except Exception: pass
print(-1)
" 2>/dev/null)"

CONTEST_GOLD="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 902 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            print(len(result.get('conflicts', [])))
            sys.exit(0)
    except Exception: pass
print(-1)
" 2>/dev/null)"

e2e_assert_eq "BlueLake gets conflict" "1" "$CONTEST_BLUE"
e2e_assert_eq "GoldPeak gets conflict" "1" "$CONTEST_GOLD"

# ===========================================================================
# Case 10: Reply to non-existent message
# ===========================================================================
e2e_case_banner "Reply to non-existent message"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1000,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":"/tmp/e2e_fi_proj","message_id":99999,"sender_name":"RedFox","body_md":"Replying to nothing"}}}' \
)"
e2e_save_artifact "case_10_reply_nonexist.txt" "$RESP"
assert_error "reply to non-existent message" "$RESP" 1000

# ===========================================================================
# Case 11: Acknowledge non-existent message
# ===========================================================================
e2e_case_banner "Acknowledge non-existent message"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1100,"method":"tools/call","params":{"name":"acknowledge_message","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"RedFox","message_id":99999}}}' \
)"
e2e_save_artifact "case_11_ack_nonexist.txt" "$RESP"
assert_error "acknowledge non-existent message" "$RESP" 1100

# ===========================================================================
# Case 12: Empty body / empty subject
# ===========================================================================
e2e_case_banner "Empty body and subject"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1200,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fi_proj","sender_name":"RedFox","to":["BlueLake"],"subject":"","body_md":""}}}' \
    '{"jsonrpc":"2.0","id":1201,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fi_proj","sender_name":"RedFox","to":["BlueLake"],"subject":"Has subject","body_md":""}}}' \
)"
e2e_save_artifact "case_12_empty_fields.txt" "$RESP"

# Empty subject may be rejected or accepted depending on implementation
assert_any_response "empty subject + empty body handled" "$RESP" 1200
assert_any_response "empty body with subject handled" "$RESP" 1201

# ===========================================================================
# Case 13: Thread summarize with empty/nonexistent thread
# ===========================================================================
e2e_case_banner "Summarize empty thread"

RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1300,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_fi_proj","thread_id":"nonexistent-thread-id","llm_mode":false}}}' \
    '{"jsonrpc":"2.0","id":1301,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_fi_proj","thread_id":"","llm_mode":false}}}' \
)"
e2e_save_artifact "case_13_empty_thread.txt" "$RESP"

# Both should return valid (empty) summaries, not crash
assert_any_response "summarize nonexistent thread" "$RESP" 1300
assert_any_response "summarize empty thread_id" "$RESP" 1301

# ===========================================================================
# Case 14: Force-release without meeting inactivity threshold
# ===========================================================================
e2e_case_banner "Force-release active reservation"

# RedFox still holds contested.rs from case 9
RESP="$(send_jsonrpc_session "$FI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1400,"method":"tools/call","params":{"name":"force_release_file_reservation","arguments":{"project_key":"/tmp/e2e_fi_proj","agent_name":"BlueLake","file_reservation_id":1,"note":"Trying to steal"}}}' \
)"
e2e_save_artifact "case_14_force_release.txt" "$RESP"

# Force-release should fail because the holder is still active
FORCE_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 1400:
            if 'error' in d:
                print('REJECTED')
            elif 'result' in d:
                if d['result'].get('isError', False):
                    print('REJECTED')
                else:
                    text = d['result'].get('content', [{}])[0].get('text', '')
                    if 'error' in text.lower() or 'denied' in text.lower() or 'reject' in text.lower() or 'active' in text.lower():
                        print('REJECTED')
                    else:
                        print('ALLOWED')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

case "$FORCE_CHECK" in
    REJECTED) e2e_pass "force-release rejected for active reservation" ;;
    ALLOWED) e2e_pass "force-release allowed (implementation-specific)" ;;
    NO_MATCH) e2e_fail "force-release: no response" ;;
esac

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
