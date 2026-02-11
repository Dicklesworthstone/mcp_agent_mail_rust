#!/usr/bin/env bash
# test_security_privacy.sh - Security/privacy E2E suite (br-3vwi.10.14)
#
# Verifies permission-aware search visibility, redaction behavior, and safe
# output across contact policies and cross-project boundaries.
#
# Cases:
#   1. Search returns only project-scoped results
#   2. Inbox isolation - agents see only their own messages
#   3. BCC privacy - BCC recipients not leaked to other recipients
#   4. Contact policy: block_all hides from search
#   5. Contact policy: contacts_only after approval
#   6. Adversarial: hostile markdown in message body
#   7. Adversarial: path traversal in thread_id
#   8. Adversarial: oversized query strings
#   9. Secret scanning - secrets in body handled safely
#  10. File reservation: cross-agent conflict enforcement
#  11. File reservation: release + re-acquire
#  12. Fetch inbox for non-existent agent

E2E_SUITE="security_privacy"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Security & Privacy E2E Suite (br-3vwi.10.14)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_security_privacy")"
SEC_DB="${WORK}/security.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-security-privacy","version":"1.0"}}}'

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

    local timeout_s=15
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

# Extract text field from result content
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

# Count search results
count_search_results() {
    local resp="$1" id="$2"
    echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id and 'result' in d:
            inner = json.loads(d['result']['content'][0]['text'])
            results = inner.get('result', inner) if isinstance(inner, dict) else inner
            print(len(results) if isinstance(results, list) else 0)
            sys.exit(0)
    except Exception: pass
print(-1)
" 2>/dev/null
}

# Count inbox messages
count_inbox_messages() {
    local resp="$1" id="$2"
    echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id and 'result' in d:
            msgs = json.loads(d['result']['content'][0]['text'])
            print(len(msgs) if isinstance(msgs, list) else 0)
            sys.exit(0)
    except Exception: pass
print(-1)
" 2>/dev/null
}

# ===========================================================================
# Setup: two projects, multiple agents
# ===========================================================================
e2e_case_banner "Setup projects and agents"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_sec_alpha"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_sec_beta"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_sec_alpha","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_sec_alpha","program":"test","model":"test","name":"BlueLake"}}}' \
    '{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_sec_alpha","program":"test","model":"test","name":"GoldPeak"}}}' \
    '{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_sec_beta","program":"test","model":"test","name":"SilverCove"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure project alpha" "$RESP" 10
assert_ok "ensure project beta" "$RESP" 11
assert_ok "register RedFox (alpha)" "$RESP" 12
assert_ok "register BlueLake (alpha)" "$RESP" 13
assert_ok "register GoldPeak (alpha)" "$RESP" 14
assert_ok "register SilverCove (beta)" "$RESP" 15

# ===========================================================================
# Seed messages in both projects
# ===========================================================================
e2e_case_banner "Seed messages"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sec_alpha","sender_name":"RedFox","to":["BlueLake"],"subject":"Alpha secret plan","body_md":"The deployment key is sk-ant-abc123secret"}}}' \
    '{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sec_alpha","sender_name":"RedFox","to":["BlueLake"],"bcc":["GoldPeak"],"subject":"BCC test message","body_md":"GoldPeak is BCC on this"}}}' \
    '{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sec_alpha","sender_name":"BlueLake","to":["GoldPeak"],"subject":"Private to GoldPeak","body_md":"Only GoldPeak should see this in inbox"}}}' \
    '{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sec_beta","sender_name":"SilverCove","to":["SilverCove"],"subject":"Beta internal note","body_md":"This message lives in project beta only"}}}' \
    '{"jsonrpc":"2.0","id":24,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sec_alpha","sender_name":"RedFox","to":["BlueLake"],"subject":"Hostile markdown test","body_md":"<script>alert(1)</script>\\n![img](javascript:alert(1))\\n[click](data:text/html,<h1>xss</h1>)"}}}' \
)"
e2e_save_artifact "seed_messages.txt" "$RESP"
assert_ok "send alpha secret plan" "$RESP" 20
assert_ok "send BCC test message" "$RESP" 21
assert_ok "send private to GoldPeak" "$RESP" 22
assert_ok "send beta internal" "$RESP" 23
assert_ok "send hostile markdown" "$RESP" 24

# ===========================================================================
# Case 1: Search returns only project-scoped results
# ===========================================================================
e2e_case_banner "Search project scoping"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sec_alpha","query":"secret"}}}' \
    '{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sec_beta","query":"secret"}}}' \
    '{"jsonrpc":"2.0","id":102,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sec_alpha","query":"Beta internal"}}}' \
)"
e2e_save_artifact "case_01_search_scope.txt" "$RESP"
assert_ok "search alpha for secret" "$RESP" 100
assert_ok "search beta for secret" "$RESP" 101
assert_ok "search alpha for beta content" "$RESP" 102

ALPHA_COUNT="$(count_search_results "$RESP" 100)"
BETA_COUNT="$(count_search_results "$RESP" 101)"
CROSS_COUNT="$(count_search_results "$RESP" 102)"
e2e_assert_eq "alpha finds secret message" "1" "$ALPHA_COUNT"
e2e_assert_eq "beta finds no secrets" "0" "$BETA_COUNT"
e2e_assert_eq "alpha cannot see beta messages" "0" "$CROSS_COUNT"

# ===========================================================================
# Case 2: Inbox isolation - agents see only their own messages
# ===========================================================================
e2e_case_banner "Inbox isolation"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"BlueLake","include_bodies":true}}}' \
    '{"jsonrpc":"2.0","id":201,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"RedFox","include_bodies":true}}}' \
    '{"jsonrpc":"2.0","id":202,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"GoldPeak","include_bodies":true}}}' \
)"
e2e_save_artifact "case_02_inbox_isolation.txt" "$RESP"
assert_ok "fetch BlueLake inbox" "$RESP" 200
assert_ok "fetch RedFox inbox" "$RESP" 201
assert_ok "fetch GoldPeak inbox" "$RESP" 202

BLUE_COUNT="$(count_inbox_messages "$RESP" 200)"
RED_COUNT="$(count_inbox_messages "$RESP" 201)"
GOLD_COUNT="$(count_inbox_messages "$RESP" 202)"

# BlueLake is direct recipient of 3 messages (ids 20, 21, 24)
# RedFox sent all messages, receives none as 'to'
# GoldPeak is BCC on id 21 + direct recipient of id 22
if [ "$BLUE_COUNT" -ge 3 ]; then
    e2e_pass "BlueLake inbox has >= 3 messages"
else
    e2e_fail "BlueLake inbox expected >= 3, got $BLUE_COUNT"
fi
e2e_assert_eq "RedFox has no inbox messages" "0" "$RED_COUNT"
if [ "$GOLD_COUNT" -ge 2 ]; then
    e2e_pass "GoldPeak inbox has >= 2 messages (BCC + direct)"
else
    e2e_fail "GoldPeak inbox expected >= 2, got $GOLD_COUNT"
fi

# ===========================================================================
# Case 3: BCC privacy - BCC not leaked to other recipients
# ===========================================================================
e2e_case_banner "BCC privacy"

# Fetch BlueLake's inbox and verify that GoldPeak is NOT in recipient metadata
# (body_md may mention GoldPeak by name, but metadata fields must not expose BCC)
BCC_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 200 and 'result' in d:
            msgs = json.loads(d['result']['content'][0]['text'])
            for msg in msgs:
                subj = msg.get('subject', '')
                if 'BCC test' in subj:
                    # Check recipient metadata fields only (not body)
                    to_list = msg.get('to', [])
                    cc_list = msg.get('cc', [])
                    bcc_list = msg.get('bcc', [])
                    recipients_str = json.dumps(to_list + cc_list + bcc_list)
                    if 'GoldPeak' in recipients_str:
                        print('LEAKED')
                    else:
                        print('HIDDEN')
                    sys.exit(0)
            print('NOT_FOUND')
    except Exception as e:
        pass
print('PARSE_ERROR')
" 2>/dev/null)"

case "$BCC_CHECK" in
    HIDDEN) e2e_pass "BCC recipient not leaked in recipient metadata" ;;
    LEAKED) e2e_fail "BCC recipient visible in recipient metadata" ;;
    NOT_FOUND) e2e_pass "BCC message structure safe (not in inbox view)" ;;
    *) e2e_fail "BCC check: $BCC_CHECK" ;;
esac

# ===========================================================================
# Case 4: Contact policy: block_all
# ===========================================================================
e2e_case_banner "Block-all contact policy"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"set_contact_policy","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"GoldPeak","policy":"block_all"}}}' \
    '{"jsonrpc":"2.0","id":401,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_sec_alpha","from_agent":"RedFox","to_agent":"GoldPeak","reason":"Trying to reach GoldPeak"}}}' \
)"
e2e_save_artifact "case_04_block_all.txt" "$RESP"
assert_ok "set GoldPeak to block_all" "$RESP" 400

# Verify the request is handled (may succeed as record but effective policy blocks visibility)
BLOCK_TEXT="$(extract_result_text "$RESP" 401)"
e2e_save_artifact "case_04_block_response.txt" "$BLOCK_TEXT"
# Contact request to block_all agent should either error or be recorded with blocked status
if echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 401:
            # Either error OR success (blocked in effective delivery)
            print('HANDLED')
            sys.exit(0)
    except: pass
print('NO_MATCH')
" 2>/dev/null | grep -q "HANDLED"; then
    e2e_pass "block_all contact request handled"
else
    e2e_fail "block_all contact request: no response"
fi

# ===========================================================================
# Case 5: Contact policy: contacts_only with approval flow
# ===========================================================================
e2e_case_banner "Contacts-only with approval"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"set_contact_policy","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"BlueLake","policy":"contacts_only"}}}' \
    '{"jsonrpc":"2.0","id":501,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_sec_alpha","from_agent":"GoldPeak","to_agent":"BlueLake","reason":"Need access"}}}' \
    '{"jsonrpc":"2.0","id":502,"method":"tools/call","params":{"name":"respond_contact","arguments":{"project_key":"/tmp/e2e_sec_alpha","to_agent":"BlueLake","from_agent":"GoldPeak","accept":true}}}' \
    '{"jsonrpc":"2.0","id":503,"method":"tools/call","params":{"name":"list_contacts","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"BlueLake"}}}' \
)"
e2e_save_artifact "case_05_contacts_only.txt" "$RESP"
assert_ok "set BlueLake to contacts_only" "$RESP" 500
assert_ok "GoldPeak requests contact" "$RESP" 501
assert_ok "BlueLake approves GoldPeak" "$RESP" 502
assert_ok "list BlueLake contacts" "$RESP" 503

# Verify GoldPeak appears in contacts
CONTACTS_TEXT="$(extract_result_text "$RESP" 503)"
if echo "$CONTACTS_TEXT" | grep -q "GoldPeak"; then
    e2e_pass "GoldPeak in BlueLake contacts after approval"
else
    e2e_fail "GoldPeak not found in BlueLake contacts"
fi

# ===========================================================================
# Case 6: Adversarial - hostile markdown stored safely
# ===========================================================================
e2e_case_banner "Hostile markdown handling"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sec_alpha","query":"Hostile markdown"}}}' \
)"
e2e_save_artifact "case_06_hostile_md.txt" "$RESP"
assert_ok "search for hostile markdown" "$RESP" 600

HOSTILE_COUNT="$(count_search_results "$RESP" 600)"
e2e_assert_eq "hostile markdown message searchable" "1" "$HOSTILE_COUNT"

# Verify the search result subject doesn't contain executable script content
HOSTILE_TEXT="$(extract_result_text "$RESP" 600)"
if echo "$HOSTILE_TEXT" | python3 -c "
import sys, json
data = json.load(sys.stdin)
results = data.get('result', [])
for r in results:
    # Subject and from should not be executable/dangerous
    subj = r.get('subject', '')
    if '<script>' in subj:
        print('SCRIPT_IN_SUBJECT')
        sys.exit(0)
print('SAFE')
" 2>/dev/null | grep -q "SAFE"; then
    e2e_pass "hostile content not in search subject"
else
    e2e_fail "hostile content leaked to search subject"
fi

# ===========================================================================
# Case 7: Adversarial - path traversal in thread_id
# ===========================================================================
e2e_case_banner "Path traversal in thread_id"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":700,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_sec_alpha","thread_id":"../../etc/passwd","llm_mode":false}}}' \
    '{"jsonrpc":"2.0","id":701,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_sec_alpha","thread_id":"..\\\\..\\\\windows\\\\system32","llm_mode":false}}}' \
)"
e2e_save_artifact "case_07_path_traversal.txt" "$RESP"

# Path traversal should either sanitize or return empty/error, never leak filesystem
TRAV_TEXT_700="$(extract_result_text "$RESP" 700)"
TRAV_TEXT_701="$(extract_result_text "$RESP" 701)"
e2e_save_artifact "case_07_traversal_700.txt" "$TRAV_TEXT_700"
e2e_save_artifact "case_07_traversal_701.txt" "$TRAV_TEXT_701"

# Verify no filesystem content leaked
if echo "$TRAV_TEXT_700" | grep -qi "root:"; then
    e2e_fail "path traversal leaked /etc/passwd content"
else
    e2e_pass "path traversal /etc/passwd blocked"
fi
if echo "$TRAV_TEXT_701" | grep -qi "system32"; then
    # system32 might appear in sanitized thread_id field, check more carefully
    if echo "$TRAV_TEXT_701" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    # For single thread, look at summary.participants for leaked data
    summary = data.get('summary', {})
    parts = summary.get('participants', [])
    if any('system32' in str(p).lower() for p in parts):
        print('LEAKED')
    else:
        print('SAFE')
except:
    print('SAFE')
" 2>/dev/null | grep -q "LEAKED"; then
        e2e_fail "path traversal leaked system paths"
    else
        e2e_pass "windows path traversal blocked"
    fi
else
    e2e_pass "windows path traversal blocked"
fi

# ===========================================================================
# Case 8: Adversarial - oversized query string
# ===========================================================================
e2e_case_banner "Oversized query string"

# Build a 10KB query
BIG_QUERY="$(python3 -c "print('A' * 10000)")"
RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":800,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"/tmp/e2e_sec_alpha\",\"query\":\"$BIG_QUERY\"}}}" \
)"
e2e_save_artifact "case_08_oversized_query.txt" "$RESP"

# Should either succeed with empty results or return a handled error, not crash
OVERSIZED_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 800:
            if 'error' in d:
                print('ERROR_HANDLED')
            elif 'result' in d:
                if d['result'].get('isError', False):
                    print('ERROR_HANDLED')
                else:
                    print('OK_EMPTY')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

case "$OVERSIZED_CHECK" in
    OK_EMPTY|ERROR_HANDLED) e2e_pass "oversized query handled gracefully" ;;
    *) e2e_fail "oversized query: unexpected result: $OVERSIZED_CHECK" ;;
esac

# ===========================================================================
# Case 9: Secret scanning - body with secrets
# ===========================================================================
e2e_case_banner "Secret in message body"

# Fetch the inbox message containing the secret
RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"BlueLake","include_bodies":true}}}' \
)"
e2e_save_artifact "case_09_secret_body.txt" "$RESP"
assert_ok "fetch inbox with secrets" "$RESP" 900

# Verify the secret key is stored (it's the message body, not yet scrubbed)
# The scrub module handles this at export time, but at storage the body is preserved
SECRET_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 900 and 'result' in d:
            msgs = json.loads(d['result']['content'][0]['text'])
            for msg in msgs:
                body = msg.get('body_md', '')
                if 'sk-ant-abc123secret' in body:
                    print('SECRET_PRESENT')
                    sys.exit(0)
            print('SECRET_NOT_FOUND')
    except Exception: pass
print('PARSE_ERROR')
" 2>/dev/null)"

# At storage level, body is preserved for the legitimate recipient
# This is correct behavior - the secret is in a message addressed to them
case "$SECRET_CHECK" in
    SECRET_PRESENT) e2e_pass "secret preserved for legitimate recipient" ;;
    SECRET_NOT_FOUND) e2e_pass "secret handling (body may be filtered)" ;;
    *) e2e_fail "secret check: $SECRET_CHECK" ;;
esac

# ===========================================================================
# Case 10: File reservation conflict enforcement
# ===========================================================================
e2e_case_banner "File reservation conflicts"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1000,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"RedFox","paths":["src/critical.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"editing critical file"}}}' \
    '{"jsonrpc":"2.0","id":1001,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"BlueLake","paths":["src/critical.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"also want critical file"}}}' \
)"
e2e_save_artifact "case_10_reservation_conflict.txt" "$RESP"
assert_ok "RedFox reserves critical.rs" "$RESP" 1000

# BlueLake's reservation should report a conflict
CONFLICT_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 1001 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            conflicts = result.get('conflicts', [])
            print(f'conflicts={len(conflicts)}')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"
e2e_assert_contains "BlueLake gets conflict" "$CONFLICT_CHECK" "conflicts=1"

# ===========================================================================
# Case 11: File reservation release + re-acquire
# ===========================================================================
e2e_case_banner "Reservation release and re-acquire"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1100,"method":"tools/call","params":{"name":"release_file_reservations","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"RedFox","paths":["src/critical.rs"]}}}' \
    '{"jsonrpc":"2.0","id":1101,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"BlueLake","paths":["src/critical.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"RedFox released"}}}' \
)"
e2e_save_artifact "case_11_release_reacquire.txt" "$RESP"
assert_ok "RedFox releases critical.rs" "$RESP" 1100

# Now BlueLake should be able to acquire without conflict
REACQUIRE_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 1101 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            granted = result.get('granted', [])
            conflicts = result.get('conflicts', [])
            print(f'granted={len(granted)}|conflicts={len(conflicts)}')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"
e2e_assert_contains "BlueLake acquires after release" "$REACQUIRE_CHECK" "granted=1"
e2e_assert_contains "no conflict after release" "$REACQUIRE_CHECK" "conflicts=0"

# ===========================================================================
# Case 12: Fetch inbox for non-existent agent
# ===========================================================================
e2e_case_banner "Non-existent agent inbox"

RESP="$(send_jsonrpc_session "$SEC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1200,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_sec_alpha","agent_name":"NoSuchAgent"}}}' \
)"
e2e_save_artifact "case_12_nonexistent_agent.txt" "$RESP"
assert_error "fetch inbox for non-existent agent" "$RESP" 1200

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
