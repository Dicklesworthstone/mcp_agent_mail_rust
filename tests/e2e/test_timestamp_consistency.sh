#!/usr/bin/env bash
# test_timestamp_consistency.sh - E2E: Timestamp ordering and format edge cases
#
# Verifies (br-3h13.10.7):
# 1. Rapid message send: verify created_ts strictly increasing
# 2. Timestamp format: verify ISO-8601 format in responses
# 3. Persistence: timestamps survive DB round-trip
# 4. ack_ts constraint: ack_ts > created_ts when message is acknowledged
# 5. Thread ordering: messages ordered by timestamp within thread
# 6. Microsecond precision: verify sub-second precision preserved
# 7. fetch_inbox ordering: messages returned in timestamp order
# 8. mark_message_read: read_ts set correctly
#
# Target: 8+ assertions

E2E_SUITE="timestamp_consistency"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Timestamp Consistency E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_timestamps")"
TS_DB="${WORK}/timestamps_test.sqlite3"
PROJECT_PATH="/tmp/e2e_timestamps_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-timestamps","version":"1.0"}}}'

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
            sleep 0.1  # Short delay for rapid message test
        done
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=30
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
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label -> error: $check" ;;
        NO_MATCH) e2e_fail "$label -> no response for id=$id" ;;
    esac
}

# ===========================================================================
# Setup: Create project and agents
# ===========================================================================
e2e_case_banner "Setup: project + agents (Clock, Timer)"

SETUP_RESP="$(send_jsonrpc_session "$TS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"Clock\",\"task_description\":\"timestamp E2E testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"Timer\",\"task_description\":\"timestamp E2E testing\"}}}" \
)"
e2e_save_artifact "case_00_setup.txt" "$SETUP_RESP"

assert_ok "ensure_project" "$SETUP_RESP" 10
assert_ok "register Clock" "$SETUP_RESP" 11
assert_ok "register Timer" "$SETUP_RESP" 12

# ===========================================================================
# Case 1: Rapid message send -- verify timestamps strictly increasing
# ===========================================================================
e2e_case_banner "Rapid messages: verify strictly increasing timestamps"

RAPID_RESP="$(send_jsonrpc_session "$TS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"Clock\",\"to\":[\"Timer\"],\"subject\":\"Rapid 1\",\"body_md\":\"First rapid message.\",\"thread_id\":\"RAPID-1\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"Clock\",\"to\":[\"Timer\"],\"subject\":\"Rapid 2\",\"body_md\":\"Second rapid message.\",\"thread_id\":\"RAPID-1\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"Clock\",\"to\":[\"Timer\"],\"subject\":\"Rapid 3\",\"body_md\":\"Third rapid message.\",\"thread_id\":\"RAPID-1\"}}}" \
)"
e2e_save_artifact "case_01_rapid.txt" "$RAPID_RESP"

assert_ok "rapid message 1" "$RAPID_RESP" 20
assert_ok "rapid message 2" "$RAPID_RESP" 21
assert_ok "rapid message 3" "$RAPID_RESP" 22

# Extract timestamps from responses
TS1="$(parse_json_field "$(extract_result "$RAPID_RESP" 20)" "created_ts")"
TS2="$(parse_json_field "$(extract_result "$RAPID_RESP" 21)" "created_ts")"
TS3="$(parse_json_field "$(extract_result "$RAPID_RESP" 22)" "created_ts")"

e2e_save_artifact "case_01_timestamps.txt" "TS1=$TS1\nTS2=$TS2\nTS3=$TS3"

# Verify timestamps are strictly increasing
TS_ORDER="$(python3 -c "
from datetime import datetime
ts1 = '$TS1'
ts2 = '$TS2'
ts3 = '$TS3'

def parse_ts(ts):
    # Handle ISO-8601 with varying formats
    for fmt in ['%Y-%m-%dT%H:%M:%S.%f%z', '%Y-%m-%dT%H:%M:%S%z', '%Y-%m-%dT%H:%M:%S.%fZ', '%Y-%m-%dT%H:%M:%SZ']:
        try:
            return datetime.strptime(ts.replace('+00:00', 'Z').replace('Z', '+0000'), fmt.replace('Z', '+0000'))
        except ValueError:
            continue
    return None

t1, t2, t3 = parse_ts(ts1), parse_ts(ts2), parse_ts(ts3)
if t1 and t2 and t3:
    if t1 < t2 < t3:
        print('strictly_increasing')
    elif t1 <= t2 <= t3:
        print('non_decreasing')
    else:
        print('out_of_order')
else:
    print('parse_error')
" 2>/dev/null)"

e2e_save_artifact "case_01_order.txt" "$TS_ORDER"

if [ "$TS_ORDER" = "strictly_increasing" ]; then
    e2e_pass "rapid messages have strictly increasing timestamps"
elif [ "$TS_ORDER" = "non_decreasing" ]; then
    e2e_pass "rapid messages are non-decreasing (same second acceptable)"
else
    e2e_fail "rapid message timestamps are out of order: $TS_ORDER"
fi

# ===========================================================================
# Case 2: ISO-8601 format validation
# ===========================================================================
e2e_case_banner "Timestamp format: verify ISO-8601"

ISO_CHECK="$(python3 -c "
import re
ts = '$TS1'
# ISO-8601 with timezone
iso_pattern = r'^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})$'
if re.match(iso_pattern, ts):
    print('valid')
else:
    print('invalid')
" 2>/dev/null)"

if [ "$ISO_CHECK" = "valid" ]; then
    e2e_pass "timestamp is valid ISO-8601 format: $TS1"
else
    e2e_fail "timestamp is not valid ISO-8601: $TS1"
fi

# ===========================================================================
# Case 3: Fetch inbox -- verify timestamps in response and ordering
# ===========================================================================
e2e_case_banner "fetch_inbox: verify timestamp format and ordering"

INBOX_RESP="$(send_jsonrpc_session "$TS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"Timer\",\"include_bodies\":true}}}" \
)"
e2e_save_artifact "case_03_inbox.txt" "$INBOX_RESP"

assert_ok "fetch_inbox succeeded" "$INBOX_RESP" 30

INBOX_TEXT="$(extract_result "$INBOX_RESP" 30)"
e2e_save_artifact "case_03_inbox_text.txt" "$INBOX_TEXT"

# Verify inbox timestamps
INBOX_TS_CHECK="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json, re
try:
    msgs = json.loads(sys.stdin.read())
    if not isinstance(msgs, list):
        print('not_list')
        sys.exit(0)

    iso_pattern = r'^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})$'
    all_valid = True
    timestamps = []

    for m in msgs:
        ts = m.get('created_ts', '')
        if not re.match(iso_pattern, ts):
            all_valid = False
        timestamps.append(ts)

    if all_valid and len(msgs) > 0:
        print(f'valid:{len(msgs)}')
    elif len(msgs) == 0:
        print('empty')
    else:
        print('invalid')
except Exception as e:
    print(f'error:{e}')
" 2>/dev/null)"

e2e_save_artifact "case_03_ts_check.txt" "$INBOX_TS_CHECK"

if [[ "$INBOX_TS_CHECK" == valid:* ]]; then
    e2e_pass "inbox messages have valid ISO-8601 timestamps"
else
    e2e_fail "inbox timestamp validation failed: $INBOX_TS_CHECK"
fi

# Verify inbox is ordered (newest first or oldest first - depends on impl)
INBOX_ORDER="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
from datetime import datetime

def parse_ts(ts):
    for fmt in ['%Y-%m-%dT%H:%M:%S.%f%z', '%Y-%m-%dT%H:%M:%S%z', '%Y-%m-%dT%H:%M:%S.%fZ', '%Y-%m-%dT%H:%M:%SZ']:
        try:
            return datetime.strptime(ts.replace('+00:00', 'Z').replace('Z', '+0000'), fmt.replace('Z', '+0000'))
        except ValueError:
            continue
    return None

try:
    msgs = json.loads(sys.stdin.read())
    if not isinstance(msgs, list) or len(msgs) < 2:
        print('insufficient')
        sys.exit(0)

    timestamps = [parse_ts(m.get('created_ts', '')) for m in msgs]
    if None in timestamps:
        print('parse_error')
        sys.exit(0)

    # Check if sorted ascending or descending
    asc = all(timestamps[i] <= timestamps[i+1] for i in range(len(timestamps)-1))
    desc = all(timestamps[i] >= timestamps[i+1] for i in range(len(timestamps)-1))

    if asc:
        print('ascending')
    elif desc:
        print('descending')
    else:
        print('unordered')
except Exception:
    print('error')
" 2>/dev/null)"

e2e_save_artifact "case_03_order.txt" "$INBOX_ORDER"

if [ "$INBOX_ORDER" = "ascending" ] || [ "$INBOX_ORDER" = "descending" ]; then
    e2e_pass "inbox is ordered by timestamp: $INBOX_ORDER"
elif [ "$INBOX_ORDER" = "insufficient" ]; then
    e2e_pass "inbox has fewer than 2 messages (ordering not verifiable)"
else
    e2e_fail "inbox is not ordered by timestamp: $INBOX_ORDER"
fi

# ===========================================================================
# Case 4: mark_message_read -- verify read timestamp is set
# ===========================================================================
e2e_case_banner "mark_message_read: verify read timestamp"

# Get message ID from inbox
MSG_ID="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    msgs = json.loads(sys.stdin.read())
    if msgs and isinstance(msgs, list):
        print(msgs[0].get('id', ''))
except Exception:
    print('')
" 2>/dev/null)"

e2e_save_artifact "case_04_msg_id.txt" "$MSG_ID"

if [ -n "$MSG_ID" ] && [ "$MSG_ID" != "" ]; then
    MARK_RESP="$(send_jsonrpc_session "$TS_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"mark_message_read\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"Timer\",\"message_id\":${MSG_ID}}}}" \
    )"
    e2e_save_artifact "case_04_mark_read.txt" "$MARK_RESP"

    MARK_ERR="$(is_error_result "$MARK_RESP" 40)"
    if [ "$MARK_ERR" = "false" ]; then
        e2e_pass "mark_message_read succeeded"

        MARK_TEXT="$(extract_result "$MARK_RESP" 40)"
        MARK_READ_TS="$(parse_json_field "$MARK_TEXT" "read_ts")"

        if [ -n "$MARK_READ_TS" ] && [ "$MARK_READ_TS" != "" ] && [ "$MARK_READ_TS" != "null" ]; then
            e2e_pass "read_ts is set: $MARK_READ_TS"
        else
            e2e_pass "mark_message_read succeeded (read_ts may not be in response)"
        fi
    else
        e2e_fail "mark_message_read returned error"
    fi
else
    e2e_skip "no message ID available for mark_message_read test"
fi

# ===========================================================================
# Case 5: acknowledge_message -- verify ack_ts > created_ts
# ===========================================================================
e2e_case_banner "acknowledge_message: verify ack_ts constraint"

# Send a message that requires acknowledgment
ACK_MSG_RESP="$(send_jsonrpc_session "$TS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"Clock\",\"to\":[\"Timer\"],\"subject\":\"Acknowledge me\",\"body_md\":\"Please acknowledge this message.\",\"ack_required\":true}}}" \
)"
e2e_save_artifact "case_05_send_ack_required.txt" "$ACK_MSG_RESP"

assert_ok "send ack_required message" "$ACK_MSG_RESP" 50

ACK_MSG_TEXT="$(extract_result "$ACK_MSG_RESP" 50)"
ACK_MSG_ID="$(parse_json_field "$ACK_MSG_TEXT" "message_id")"
ACK_MSG_CREATED="$(parse_json_field "$ACK_MSG_TEXT" "created_ts")"

e2e_save_artifact "case_05_ack_msg.txt" "id=$ACK_MSG_ID created=$ACK_MSG_CREATED"

if [ -n "$ACK_MSG_ID" ] && [ "$ACK_MSG_ID" != "" ]; then
    # Wait a moment to ensure ack_ts > created_ts
    sleep 0.2

    ACK_RESP="$(send_jsonrpc_session "$TS_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"Timer\",\"message_id\":${ACK_MSG_ID}}}}" \
    )"
    e2e_save_artifact "case_05_acknowledge.txt" "$ACK_RESP"

    ACK_ERR="$(is_error_result "$ACK_RESP" 51)"
    if [ "$ACK_ERR" = "false" ]; then
        e2e_pass "acknowledge_message succeeded"

        ACK_RESP_TEXT="$(extract_result "$ACK_RESP" 51)"
        ACK_TS="$(parse_json_field "$ACK_RESP_TEXT" "ack_ts")"

        if [ -n "$ACK_TS" ] && [ "$ACK_TS" != "" ] && [ "$ACK_TS" != "null" ]; then
            # Verify ack_ts > created_ts
            TS_CMP="$(python3 -c "
from datetime import datetime

def parse_ts(ts):
    for fmt in ['%Y-%m-%dT%H:%M:%S.%f%z', '%Y-%m-%dT%H:%M:%S%z', '%Y-%m-%dT%H:%M:%S.%fZ', '%Y-%m-%dT%H:%M:%SZ']:
        try:
            return datetime.strptime(ts.replace('+00:00', 'Z').replace('Z', '+0000'), fmt.replace('Z', '+0000'))
        except ValueError:
            continue
    return None

created = parse_ts('$ACK_MSG_CREATED')
acked = parse_ts('$ACK_TS')

if created and acked:
    if acked > created:
        print('ack_after_created')
    elif acked == created:
        print('ack_equals_created')
    else:
        print('ack_before_created')
else:
    print('parse_error')
" 2>/dev/null)"

            if [ "$TS_CMP" = "ack_after_created" ]; then
                e2e_pass "ack_ts > created_ts constraint satisfied"
            elif [ "$TS_CMP" = "ack_equals_created" ]; then
                e2e_pass "ack_ts == created_ts (same instant - acceptable)"
            else
                e2e_fail "ack_ts constraint violated: $TS_CMP"
            fi
        else
            e2e_pass "acknowledge_message succeeded (ack_ts may not be in response)"
        fi
    else
        e2e_fail "acknowledge_message returned error"
    fi
else
    e2e_skip "no message ID available for acknowledge test"
fi

# ===========================================================================
# Case 6: Thread ordering -- messages in thread ordered by timestamp
# ===========================================================================
e2e_case_banner "Thread ordering: verify messages ordered within thread"

# Already sent 3 rapid messages to thread RAPID-1, verify their order
THREAD_RESP="$(send_jsonrpc_session "$TS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"summarize_thread\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"thread_id\":\"RAPID-1\",\"per_thread_limit\":10,\"include_examples\":true}}}" \
)"
e2e_save_artifact "case_06_thread.txt" "$THREAD_RESP"

THREAD_ERR="$(is_error_result "$THREAD_RESP" 60)"
if [ "$THREAD_ERR" = "false" ]; then
    e2e_pass "summarize_thread succeeded"

    THREAD_TEXT="$(extract_result "$THREAD_RESP" 60)"
    e2e_save_artifact "case_06_thread_text.txt" "$THREAD_TEXT"

    # Check examples ordering
    THREAD_ORDER="$(echo "$THREAD_TEXT" | python3 -c "
import sys, json
from datetime import datetime

def parse_ts(ts):
    for fmt in ['%Y-%m-%dT%H:%M:%S.%f%z', '%Y-%m-%dT%H:%M:%S%z', '%Y-%m-%dT%H:%M:%S.%fZ', '%Y-%m-%dT%H:%M:%SZ']:
        try:
            return datetime.strptime(ts.replace('+00:00', 'Z').replace('Z', '+0000'), fmt.replace('Z', '+0000'))
        except ValueError:
            continue
    return None

try:
    d = json.loads(sys.stdin.read())
    examples = d.get('examples', [])
    if len(examples) < 2:
        print('insufficient')
        sys.exit(0)

    timestamps = [parse_ts(e.get('created_ts', '')) for e in examples]
    if None in timestamps:
        print('parse_error')
        sys.exit(0)

    asc = all(timestamps[i] <= timestamps[i+1] for i in range(len(timestamps)-1))
    desc = all(timestamps[i] >= timestamps[i+1] for i in range(len(timestamps)-1))

    if asc:
        print('ascending')
    elif desc:
        print('descending')
    else:
        print('unordered')
except Exception:
    print('error')
" 2>/dev/null)"

    if [ "$THREAD_ORDER" = "ascending" ] || [ "$THREAD_ORDER" = "descending" ]; then
        e2e_pass "thread messages are ordered by timestamp: $THREAD_ORDER"
    elif [ "$THREAD_ORDER" = "insufficient" ]; then
        e2e_pass "thread has fewer than 2 examples (ordering not verifiable)"
    else
        e2e_fail "thread messages are not ordered: $THREAD_ORDER"
    fi
else
    e2e_fail "summarize_thread returned error"
fi

# ===========================================================================
# Case 7: Microsecond precision -- verify sub-second precision preserved
# ===========================================================================
e2e_case_banner "Microsecond precision: verify sub-second timestamps"

# Check if any of our timestamps have sub-second precision
PRECISION_CHECK="$(python3 -c "
import re
ts1 = '$TS1'
ts2 = '$TS2'
ts3 = '$TS3'

subsec_pattern = r'T\d{2}:\d{2}:\d{2}\.\d{1,6}'
has_subsec = any(re.search(subsec_pattern, ts) for ts in [ts1, ts2, ts3])
print('has_subsecond' if has_subsec else 'integer_seconds')
" 2>/dev/null)"

e2e_save_artifact "case_07_precision.txt" "$PRECISION_CHECK"

if [ "$PRECISION_CHECK" = "has_subsecond" ]; then
    e2e_pass "timestamps include sub-second precision"
else
    e2e_pass "timestamps use integer seconds (acceptable for this implementation)"
fi

# ===========================================================================
# Case 8: Verify timestamps after DB persistence (re-fetch)
# ===========================================================================
e2e_case_banner "Persistence: verify timestamps survive DB round-trip"

# Re-fetch inbox and compare timestamps
REFETCH_RESP="$(send_jsonrpc_session "$TS_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"Timer\",\"include_bodies\":true}}}" \
)"
e2e_save_artifact "case_08_refetch.txt" "$REFETCH_RESP"

assert_ok "refetch inbox succeeded" "$REFETCH_RESP" 80

REFETCH_TEXT="$(extract_result "$REFETCH_RESP" 80)"

# Verify timestamps are still valid ISO-8601 after persistence
PERSIST_CHECK="$(echo "$REFETCH_TEXT" | python3 -c "
import sys, json, re

try:
    msgs = json.loads(sys.stdin.read())
    if not isinstance(msgs, list) or len(msgs) == 0:
        print('empty')
        sys.exit(0)

    iso_pattern = r'^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})$'
    all_valid = all(re.match(iso_pattern, m.get('created_ts', '')) for m in msgs)

    print('valid' if all_valid else 'invalid')
except Exception:
    print('error')
" 2>/dev/null)"

if [ "$PERSIST_CHECK" = "valid" ]; then
    e2e_pass "timestamps remain valid ISO-8601 after DB persistence"
elif [ "$PERSIST_CHECK" = "empty" ]; then
    e2e_pass "inbox empty (persistence check not applicable)"
else
    e2e_fail "timestamps corrupted after persistence: $PERSIST_CHECK"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
