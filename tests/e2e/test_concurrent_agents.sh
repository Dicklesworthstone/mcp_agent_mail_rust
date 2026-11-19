#!/usr/bin/env bash
# test_concurrent_agents.sh - E2E concurrent agent scenarios
#
# Verifies:
#   1. Two agents racing for the same exclusive file reservation
#   2. Multiple agents sending messages simultaneously
#   3. Concurrent inbox reads don't interfere
#   4. Concurrent acknowledge operations
#   5. File reservation conflict detection

E2E_SUITE="concurrent_agents"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Concurrent Agent Scenarios E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_concurrent")"
CA_DB="${WORK}/concurrent.sqlite3"
# The previous runner polled fifteen times at 300 ms intervals.
export E2E_STDIO_TIMEOUT_SECONDS=4.5

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-concurrent","version":"1.0"}}}'

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
" 2>/dev/null)"
    case "$check" in
        OK) e2e_pass "$label" ;;
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label → error: $check" ;;
        NO_MATCH) e2e_fail "$label → no response for id=$id" ;;
    esac
}

# ===========================================================================
# Setup: project + 3 agents
# ===========================================================================
e2e_case_banner "Setup project + agents"

RESP="$(e2e_stdio_session "$CA_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_concurrent"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_concurrent","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_concurrent","program":"test","model":"test","name":"BlueLake"}}}' \
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_concurrent","program":"test","model":"test","name":"GoldPeak"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure project" "$RESP" 10
assert_ok "register RedFox" "$RESP" 11
assert_ok "register BlueLake" "$RESP" 12
assert_ok "register GoldPeak" "$RESP" 13

# ===========================================================================
# Case 1: File reservation conflict - RedFox reserves, BlueLake tries same path
# ===========================================================================
e2e_case_banner "File reservation conflict"

RESP="$(e2e_stdio_session "$CA_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_concurrent","agent_name":"RedFox","paths":["src/shared.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"editing"}}}' \
    '{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_concurrent","agent_name":"BlueLake","paths":["src/shared.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"also editing"}}}' \
)"
e2e_save_artifact "case_01_reservation_conflict.txt" "$RESP"
assert_ok "RedFox reserves src/shared.rs" "$RESP" 100

# BlueLake should get a conflict
CONFLICT="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 101 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            conflicts = result.get('conflicts', [])
            if len(conflicts) > 0:
                print(f'CONFLICT={len(conflicts)}')
            else:
                print('NO_CONFLICT')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"

e2e_assert_contains "BlueLake gets conflict" "$CONFLICT" "CONFLICT="

# ===========================================================================
# Case 2: Multiple agents sending messages in rapid succession
# ===========================================================================
e2e_case_banner "Rapid multi-agent messaging"

RESP="$(e2e_stdio_session --pipeline "$CA_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_concurrent","sender_name":"RedFox","to":["BlueLake","GoldPeak"],"subject":"From RedFox 1","body_md":"msg 1"}}}' \
    '{"jsonrpc":"2.0","id":201,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_concurrent","sender_name":"BlueLake","to":["RedFox","GoldPeak"],"subject":"From BlueLake 1","body_md":"msg 2"}}}' \
    '{"jsonrpc":"2.0","id":202,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_concurrent","sender_name":"GoldPeak","to":["RedFox","BlueLake"],"subject":"From GoldPeak 1","body_md":"msg 3"}}}' \
    '{"jsonrpc":"2.0","id":203,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_concurrent","sender_name":"RedFox","to":["BlueLake"],"subject":"From RedFox 2","body_md":"msg 4"}}}' \
    '{"jsonrpc":"2.0","id":204,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_concurrent","sender_name":"BlueLake","to":["RedFox"],"subject":"From BlueLake 2","body_md":"msg 5"}}}' \
)"
e2e_save_artifact "case_02_rapid_messaging.txt" "$RESP"
assert_ok "RedFox sends msg 1" "$RESP" 200
assert_ok "BlueLake sends msg 2" "$RESP" 201
assert_ok "GoldPeak sends msg 3" "$RESP" 202
assert_ok "RedFox sends msg 4" "$RESP" 203
assert_ok "BlueLake sends msg 5" "$RESP" 204

# ===========================================================================
# Case 3: All agents fetch inbox concurrently
# ===========================================================================
e2e_case_banner "Concurrent inbox fetches"

RESP="$(e2e_stdio_session --pipeline "$CA_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_concurrent","agent_name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":301,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_concurrent","agent_name":"BlueLake"}}}' \
    '{"jsonrpc":"2.0","id":302,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_concurrent","agent_name":"GoldPeak"}}}' \
)"
e2e_save_artifact "case_03_concurrent_inbox.txt" "$RESP"
assert_ok "RedFox inbox fetch" "$RESP" 300
assert_ok "BlueLake inbox fetch" "$RESP" 301
assert_ok "GoldPeak inbox fetch" "$RESP" 302

# A successful fetch can still return an empty inbox or another agent's mail.
if python3 -c '
import json, sys
expected = {
    300: ["From BlueLake 1", "From BlueLake 2", "From GoldPeak 1"],
    301: ["From GoldPeak 1", "From RedFox 1", "From RedFox 2"],
    302: ["From BlueLake 1", "From RedFox 1"],
}
for line in sys.stdin:
    reply = json.loads(line)
    if reply.get("id") in expected:
        inbox = json.loads(reply["result"]["content"][0]["text"])
        actual = sorted(m["subject"] for m in inbox if m["subject"].startswith("From "))
        if actual != expected.pop(reply["id"]):
            sys.exit(1)
sys.exit(bool(expected))
' <<< "$RESP"; then
    e2e_pass "each agent receives exactly its addressed test messages"
else
    e2e_fail "per-recipient inbox contents differ from the five sends"
fi

# ===========================================================================
# Case 4: Concurrent ack operations
# ===========================================================================
e2e_case_banner "Concurrent acknowledge"

# First send ack_required messages
RESP="$(e2e_stdio_session "$CA_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_concurrent","sender_name":"RedFox","to":["BlueLake","GoldPeak"],"subject":"Ack needed","body_md":"Please confirm","ack_required":true}}}' \
)"
e2e_save_artifact "case_04a_send_ack_msg.txt" "$RESP"
assert_ok "send ack-required message" "$RESP" 400

# Get the message ID
MSG_ID="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 400 and 'result' in d:
            inner = json.loads(d['result']['content'][0]['text'])
            deliveries = inner.get('deliveries', [])
            if deliveries:
                print(deliveries[0]['payload']['id'])
                sys.exit(0)
    except Exception: pass
print('0')
" 2>/dev/null)"

if [ "$MSG_ID" != "0" ] && [ -n "$MSG_ID" ]; then
    # Both agents ack the same message
    RESP="$(e2e_stdio_session --pipeline "$CA_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":401,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_concurrent\",\"agent_name\":\"BlueLake\",\"message_id\":${MSG_ID}}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":402,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_concurrent\",\"agent_name\":\"GoldPeak\",\"message_id\":${MSG_ID}}}}" \
    )"
    e2e_save_artifact "case_04b_concurrent_ack.txt" "$RESP"
    assert_ok "BlueLake acks message" "$RESP" 401
    assert_ok "GoldPeak acks message" "$RESP" 402
else
    e2e_fail "could not extract message ID for ack test"
fi

# ===========================================================================
# Case 5: Release + re-acquire reservation
# ===========================================================================
e2e_case_banner "Release and re-acquire reservation"

RESP="$(e2e_stdio_session "$CA_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"release_file_reservations","arguments":{"project_key":"/tmp/e2e_concurrent","agent_name":"RedFox","paths":["src/shared.rs"]}}}' \
    '{"jsonrpc":"2.0","id":501,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_concurrent","agent_name":"BlueLake","paths":["src/shared.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"now available"}}}' \
)"
e2e_save_artifact "case_05_release_reacquire.txt" "$RESP"
assert_ok "RedFox releases reservation" "$RESP" 500

# BlueLake should now be able to acquire without conflict
REACQ="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 501 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            conflicts = result.get('conflicts', [])
            granted = result.get('granted', [])
            print(f'granted={len(granted)}|conflicts={len(conflicts)}')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"

e2e_assert_contains "BlueLake acquires after release" "$REACQ" "granted=1"
e2e_assert_contains "no conflicts after release" "$REACQ" "conflicts=0"

# Each server session has exited and flushed its archive by this point.
for mailbox in messages agents/RedFox/outbox agents/BlueLake/inbox agents/GoldPeak/inbox; do
    if git -C "$STORAGE_ROOT" grep -q -F 'Please confirm' HEAD -- \
        ":(glob)projects/*/${mailbox}/**/*__ack-needed__${MSG_ID}.md"; then
        e2e_pass "ack-required message committed in $mailbox"
    else
        e2e_fail "suite archive is missing message $MSG_ID in $mailbox"
    fi
done

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
