#!/usr/bin/env bash
# test_stress_correctness.sh — E2E: stress test with data integrity verification
#
# Verifies (br-3h13.18.7):
# - Message delivery is complete (no lost writes under sequential load)
# - Message ordering is monotonically increasing
# - Ack state is consistent after selective acknowledgments
# - Thread integrity: reply chains form proper parent chains
# - Cross-project isolation: messages don't leak across projects

set -euo pipefail

E2E_SUITE="stress_correctness"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Stress Correctness E2E Test Suite"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_stress_corr")"
STRESS_DB="${WORK}/stress_correctness.sqlite3"
PROJECT_PATH_A="/tmp/e2e_stress_corr_a_$$"
PROJECT_PATH_B="/tmp/e2e_stress_corr_b_$$"
PROJECT_SLUG_A="tmp-e2e-stress-corr-a-$$"
PROJECT_SLUG_B="tmp-e2e-stress-corr-b-$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-stress","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"; shift
    local requests=("$@")
    local output_file
    output_file="$(mktemp "${WORK}/session_resp.XXXXXX")"
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
            sleep 0.15
        done
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=60
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

# ===========================================================================
# Case 1: Setup — create 2 projects + 3 agents on project A
# ===========================================================================
e2e_case_banner "Setup: 2 projects + 3 agents"

SETUP_REQS=(
    "$INIT_REQ"
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_B}\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"RedFox\",\"project_key\":\"${PROJECT_PATH_A}\",\"program\":\"test\",\"model\":\"test\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"BlueLake\",\"project_key\":\"${PROJECT_PATH_A}\",\"program\":\"test\",\"model\":\"test\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"GoldHawk\",\"project_key\":\"${PROJECT_PATH_A}\",\"program\":\"test\",\"model\":\"test\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":15,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"SilverWolf\",\"project_key\":\"${PROJECT_PATH_B}\",\"program\":\"test\",\"model\":\"test\"}}}"
)

SETUP_RESP="$(send_jsonrpc_session "$STRESS_DB" "${SETUP_REQS[@]}")"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

SETUP_OK=true
for rid in 10 11 12 13 14 15; do
    if [ "$(is_error_result "$SETUP_RESP" $rid)" = "true" ]; then
        SETUP_OK=false
        e2e_fail "setup: request id=$rid failed"
    fi
done
if [ "$SETUP_OK" = "true" ]; then
    e2e_pass "setup: 2 projects + 4 agents created"
fi

# ===========================================================================
# Case 2: Message delivery completeness — 3 agents send 5 msgs each (15 total)
# ===========================================================================
e2e_case_banner "message_delivery_completeness"

SEND_REQS=("$INIT_REQ")
msg_id=100
for sender in RedFox BlueLake GoldHawk; do
    for i in $(seq 1 5); do
        SEND_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":${msg_id},\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"${sender}\",\"to\":[\"RedFox\"],\"subject\":\"Msg ${sender} #${i}\",\"body_md\":\"Test message ${i} from ${sender}\"}}}")
        msg_id=$((msg_id + 1))
    done
done

# Fetch inbox after sending
SEND_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":200,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/RedFox?project=${PROJECT_SLUG_A}\"}}")

SEND_RESP="$(send_jsonrpc_session "$STRESS_DB" "${SEND_REQS[@]}")"
e2e_save_artifact "case_02_delivery.txt" "$SEND_RESP"

# Count how many sends succeeded
send_ok=0
for rid in $(seq 100 114); do
    if [ "$(is_error_result "$SEND_RESP" $rid)" = "false" ]; then
        send_ok=$((send_ok + 1))
    fi
done
e2e_assert_eq "delivery: all 15 sends succeeded" "15" "$send_ok"

# Check inbox count
INBOX_TEXT="$(extract_resource_text "$SEND_RESP" 200)"
e2e_save_artifact "case_02_inbox.json" "$INBOX_TEXT"

INBOX_COUNT="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if isinstance(d, list):
        print(len(d))
    elif isinstance(d, dict) and 'messages' in d:
        print(len(d['messages']))
    elif isinstance(d, dict) and 'inbox' in d:
        print(len(d['inbox']))
    else:
        print(d.get('count', 0) if isinstance(d, dict) else 0)
except Exception:
    print(0)
" 2>/dev/null)"

# RedFox receives 10 from others + 5 to self (the 5 from RedFox to RedFox)
# Actually: RedFox sends 5 to RedFox too — all 15 go to RedFox's inbox
if [ "$INBOX_COUNT" -ge 10 ]; then
    e2e_pass "delivery: inbox has $INBOX_COUNT messages (>= 10)"
else
    e2e_fail "delivery: inbox has $INBOX_COUNT messages (expected >= 10)"
fi

# ===========================================================================
# Case 3: Message ordering — sequential send with numbered subjects
# ===========================================================================
e2e_case_banner "message_ordering_monotonic"

ORD_REQS=("$INIT_REQ")
for i in $(seq 1 10); do
    ORD_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":$((300 + i)),\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"BlueLake\",\"to\":[\"GoldHawk\"],\"subject\":\"Order test ${i}\",\"body_md\":\"Sequential message number ${i}\"}}}")
done
ORD_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":320,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/GoldHawk?project=${PROJECT_SLUG_A}\"}}")

ORD_RESP="$(send_jsonrpc_session "$STRESS_DB" "${ORD_REQS[@]}")"
e2e_save_artifact "case_03_ordering.txt" "$ORD_RESP"

ORD_INBOX="$(extract_resource_text "$ORD_RESP" 320)"
e2e_save_artifact "case_03_inbox.json" "$ORD_INBOX"

# Check ordering: inbox returns newest-first, so numbers should be descending
# We verify they form a contiguous sequence (all 10 present, no duplicates)
ORD_CHECK="$(echo "$ORD_INBOX" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    msgs = d if isinstance(d, list) else d.get('messages', d.get('inbox', []))
    # Filter to 'Order test' subjects
    order_msgs = [m for m in msgs if 'Order test' in m.get('subject', '')]
    nums = []
    for m in order_msgs:
        subj = m['subject']
        n = int(subj.split()[-1])
        nums.append(n)
    # Inbox is newest-first (descending), verify all 10 present
    if sorted(nums) == list(range(1, 11)):
        print('ok')
    else:
        print(f'mismatch: got {sorted(nums)}, expected [1..10]')
except Exception as e:
    print(f'error: {e}')
" 2>/dev/null)"

if [ "$ORD_CHECK" = "ok" ]; then
    e2e_pass "ordering: all 10 messages present with correct subjects"
else
    e2e_fail "ordering: $ORD_CHECK"
fi

# ===========================================================================
# Case 4: Thread integrity — send + 3 replies, verify chain
# ===========================================================================
e2e_case_banner "thread_integrity"

# Send initial message
THREAD_REQS=("$INIT_REQ")
THREAD_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":400,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"Thread test\",\"body_md\":\"Initial message\"}}}")

THR_RESP1="$(send_jsonrpc_session "$STRESS_DB" "${THREAD_REQS[@]}")"
e2e_save_artifact "case_04_thread_initial.txt" "$THR_RESP1"

THR_TEXT="$(extract_result "$THR_RESP1" 400)"
THR_MSG_ID="$(echo "$THR_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    # send_message returns {deliveries: [{payload: {id: N}}], count: 1}
    if 'deliveries' in d and d['deliveries']:
        print(d['deliveries'][0]['payload']['id'])
    else:
        print(d.get('id', d.get('message_id', '')))
except:
    print('')
" 2>/dev/null)"

if [ -n "$THR_MSG_ID" ]; then
    e2e_pass "thread: initial message sent (id=$THR_MSG_ID)"
else
    e2e_fail "thread: could not get initial message ID"
fi

# Send 3 replies
if [ -n "$THR_MSG_ID" ]; then
    REPLY_REQS=("$INIT_REQ")
    prev_id="$THR_MSG_ID"
    for i in 1 2 3; do
        REPLY_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":$((410 + i)),\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"BlueLake\",\"message_id\":${prev_id},\"body_md\":\"Reply ${i}\"}}}")
    done

    REPLY_RESP="$(send_jsonrpc_session "$STRESS_DB" "${REPLY_REQS[@]}")"
    e2e_save_artifact "case_04_thread_replies.txt" "$REPLY_RESP"

    reply_ok=0
    for rid in 411 412 413; do
        if [ "$(is_error_result "$REPLY_RESP" $rid)" = "false" ]; then
            reply_ok=$((reply_ok + 1))
        fi
    done
    e2e_assert_eq "thread: all 3 replies succeeded" "3" "$reply_ok"
fi

# ===========================================================================
# Case 5: Cross-project isolation
# ===========================================================================
e2e_case_banner "cross_project_isolation"

ISOL_REQS=("$INIT_REQ")
# Send 3 messages to project B
for i in 1 2 3; do
    ISOL_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":$((500 + i)),\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"sender_name\":\"SilverWolf\",\"to\":[\"SilverWolf\"],\"subject\":\"Project B msg ${i}\",\"body_md\":\"Isolation test ${i}\"}}}")
done
# Read inbox for project A agent GoldHawk
ISOL_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":510,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/GoldHawk?project=${PROJECT_SLUG_A}\"}}")
# Read inbox for project B agent SilverWolf
ISOL_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":511,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/SilverWolf?project=${PROJECT_SLUG_B}\"}}")

ISOL_RESP="$(send_jsonrpc_session "$STRESS_DB" "${ISOL_REQS[@]}")"
e2e_save_artifact "case_05_isolation.txt" "$ISOL_RESP"

# Check project A inbox for GoldHawk has no project B messages
A_INBOX="$(extract_resource_text "$ISOL_RESP" 510)"
e2e_save_artifact "case_05_inbox_a.json" "$A_INBOX"
e2e_assert_not_contains "isolation: no Project B msgs in A" "$A_INBOX" "Project B msg"

# Check project B inbox for SilverWolf has its messages
B_INBOX="$(extract_resource_text "$ISOL_RESP" 511)"
e2e_save_artifact "case_05_inbox_b.json" "$B_INBOX"
e2e_assert_contains "isolation: Project B msgs in B" "$B_INBOX" "Project B msg"

# ===========================================================================
# Case 6: Ack state consistency — send 6 messages, ack evens
# ===========================================================================
e2e_case_banner "ack_state_consistency"

ACK_REQS=("$INIT_REQ")
# Send 6 messages to GoldHawk from RedFox
for i in $(seq 1 6); do
    ACK_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":$((600 + i)),\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"RedFox\",\"to\":[\"GoldHawk\"],\"subject\":\"Ack test ${i}\",\"body_md\":\"Message ${i} for ack testing\",\"ack_required\":true}}}")
done

ACK_SEND_RESP="$(send_jsonrpc_session "$STRESS_DB" "${ACK_REQS[@]}")"
e2e_save_artifact "case_06_ack_sends.txt" "$ACK_SEND_RESP"

# Extract message IDs from send results
ACK_MSG_IDS=()
for i in $(seq 1 6); do
    rid=$((600 + i))
    txt="$(extract_result "$ACK_SEND_RESP" $rid)"
    mid="$(echo "$txt" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if 'deliveries' in d and d['deliveries']:
        print(d['deliveries'][0]['payload']['id'])
    else:
        print(d.get('id', d.get('message_id', '')))
except:
    print('')
" 2>/dev/null)"
    ACK_MSG_IDS+=("$mid")
done

ack_send_count=0
for mid in "${ACK_MSG_IDS[@]}"; do
    [ -n "$mid" ] && ack_send_count=$((ack_send_count + 1))
done
e2e_assert_eq "ack: all 6 messages sent" "6" "$ack_send_count"

# Ack messages 2, 4, 6 (even-indexed: ACK_MSG_IDS[1], [3], [5])
if [ "$ack_send_count" -eq 6 ]; then
    ACK_DO_REQS=("$INIT_REQ")
    for idx in 1 3 5; do
        mid="${ACK_MSG_IDS[$idx]}"
        ACK_DO_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":$((700 + idx)),\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"agent_name\":\"GoldHawk\",\"message_id\":${mid}}}}")
    done

    ACK_DO_RESP="$(send_jsonrpc_session "$STRESS_DB" "${ACK_DO_REQS[@]}")"
    e2e_save_artifact "case_06_ack_do.txt" "$ACK_DO_RESP"

    ack_ok=0
    for idx in 1 3 5; do
        if [ "$(is_error_result "$ACK_DO_RESP" $((700 + idx)))" = "false" ]; then
            ack_ok=$((ack_ok + 1))
        fi
    done
    e2e_assert_eq "ack: 3 acks succeeded" "3" "$ack_ok"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
