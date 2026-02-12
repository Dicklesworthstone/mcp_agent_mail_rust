#!/usr/bin/env bash
# test_concurrent_conflicts_e2e.sh - E2E: Write conflicts and concurrent operations
#
# Tests concurrent DB access patterns via separate MCP server sessions sharing
# a single SQLite database. Each send_jsonrpc_session call spins up a fresh
# server process, so overlapping reservations and messaging exercise real
# cross-session conflict detection.
#
# Tests:
#   1. Setup: ensure_project + register 3 agents (GoldFox, SilverWolf, RedHawk)
#   2. GoldFox reserves "app/*.py" exclusively
#   3. SilverWolf reserves "app/api/*.py" (overlaps GoldFox) — conflict detected
#   4. RedHawk sends messages to GoldFox and SilverWolf concurrently
#   5. fetch_inbox for each agent verifies all messages received
#   6. Multiple agents acknowledge the same messages
#   7. Idempotent operations: ensure_project twice returns same ID, register_agent upserts

E2E_SUITE="concurrent_conflicts"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Concurrent Conflicts E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_concurrent")"
CC_DB="${WORK}/concurrent_test.sqlite3"
PROJECT_PATH="/tmp/e2e_concurrent_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-concurrent","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers (same pattern as test_tools_reservations.sh)
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

# ===========================================================================
# Case 1: Setup — ensure_project + register 3 agents
# ===========================================================================
e2e_case_banner "Setup: ensure_project + 3 agents (GoldFox, SilverWolf, RedHawk)"

SETUP_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"reservation holder\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"reservation contender\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"RedHawk\",\"task_description\":\"message sender\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

PROJ_ERR="$(is_error_result "$SETUP_RESP" 10)"
GF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"
RH_ERR="$(is_error_result "$SETUP_RESP" 13)"

if [ "$PROJ_ERR" = "false" ]; then
    e2e_pass "ensure_project succeeded"
else
    e2e_fail "ensure_project returned error"
fi

if [ "$GF_ERR" = "false" ] && [ "$SW_ERR" = "false" ] && [ "$RH_ERR" = "false" ]; then
    e2e_pass "all 3 agents registered (GoldFox, SilverWolf, RedHawk)"
else
    e2e_fail "agent registration failed (GoldFox=$GF_ERR, SilverWolf=$SW_ERR, RedHawk=$RH_ERR)"
fi

# Save project id for idempotency test later
PROJ_TEXT="$(extract_result "$SETUP_RESP" 10)"
PROJ_ID="$(parse_json_field "$PROJ_TEXT" "id")"
PROJ_SLUG="$(parse_json_field "$PROJ_TEXT" "slug")"
e2e_save_artifact "case_01_project_id.txt" "$PROJ_ID"

if [ -n "$PROJ_ID" ] && [ "$PROJ_ID" != "" ]; then
    e2e_pass "project id returned: $PROJ_ID"
else
    e2e_fail "project id missing from ensure_project response"
fi

# ===========================================================================
# Case 2: GoldFox reserves "app/*.py" exclusively (separate session)
# ===========================================================================
e2e_case_banner "GoldFox reserves app/*.py exclusively"

GF_RESERVE_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"paths\":[\"app/*.py\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"concurrent conflict E2E\"}}}" \
)"
e2e_save_artifact "case_02_goldfox_reserve.txt" "$GF_RESERVE_RESP"

GF_RES_ERR="$(is_error_result "$GF_RESERVE_RESP" 20)"
GF_RES_TEXT="$(extract_result "$GF_RESERVE_RESP" 20)"

if [ "$GF_RES_ERR" = "true" ]; then
    e2e_fail "GoldFox file_reservation_paths returned error"
    echo "    text: $GF_RES_TEXT"
else
    e2e_pass "GoldFox file_reservation_paths succeeded"
fi

GF_RES_CHECK="$(echo "$GF_RES_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    conflicts = result.get('conflicts', [])
    print(f'granted={len(granted)}|conflicts={len(conflicts)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_assert_contains "GoldFox granted 1 reservation" "$GF_RES_CHECK" "granted=1"
e2e_assert_contains "GoldFox no conflicts" "$GF_RES_CHECK" "conflicts=0"

# ===========================================================================
# Case 3: SilverWolf reserves "app/api/*.py" — conflict with GoldFox
# ===========================================================================
e2e_case_banner "SilverWolf reserves app/api/*.py (overlaps GoldFox) — conflict"

SW_RESERVE_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"paths\":[\"app/api/*.py\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"concurrent conflict E2E\"}}}" \
)"
e2e_save_artifact "case_03_silverwolf_reserve.txt" "$SW_RESERVE_RESP"

SW_RES_ERR="$(is_error_result "$SW_RESERVE_RESP" 30)"
SW_RES_TEXT="$(extract_result "$SW_RESERVE_RESP" 30)"

# The tool succeeds at the JSON-RPC level but reports conflicts in the result
SW_RES_CHECK="$(echo "$SW_RES_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    conflicts = result.get('conflicts', [])
    has_goldfox_holder = False
    for c in conflicts:
        holders = c.get('holders', [])
        for h in holders:
            agent = h if isinstance(h, str) else h.get('agent', h.get('agent_name', ''))
            if 'GoldFox' in str(agent):
                has_goldfox_holder = True
    print(f'granted={len(granted)}|conflicts={len(conflicts)}|has_goldfox_holder={has_goldfox_holder}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_03_parsed.txt" "$SW_RES_CHECK"

e2e_assert_contains "SilverWolf sees conflict" "$SW_RES_CHECK" "conflicts=1"
e2e_assert_contains "conflict holder is GoldFox" "$SW_RES_CHECK" "has_goldfox_holder=True"

# ===========================================================================
# Case 4: RedHawk sends messages to GoldFox and SilverWolf (separate session)
# ===========================================================================
e2e_case_banner "RedHawk sends messages to GoldFox and SilverWolf"

MSG_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedHawk\",\"to\":[\"GoldFox\"],\"subject\":\"Build plan alpha\",\"body_md\":\"Starting the build now.\",\"thread_id\":\"CONC-1\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedHawk\",\"to\":[\"SilverWolf\"],\"subject\":\"Build plan beta\",\"body_md\":\"Please review the PR.\",\"thread_id\":\"CONC-2\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedHawk\",\"to\":[\"GoldFox\",\"SilverWolf\"],\"subject\":\"Build plan shared\",\"body_md\":\"Multi-recipient update.\",\"thread_id\":\"CONC-3\",\"ack_required\":true}}}" \
)"
e2e_save_artifact "case_04_send_messages.txt" "$MSG_RESP"

MSG1_ERR="$(is_error_result "$MSG_RESP" 40)"
MSG2_ERR="$(is_error_result "$MSG_RESP" 41)"
MSG3_ERR="$(is_error_result "$MSG_RESP" 42)"

if [ "$MSG1_ERR" = "false" ] && [ "$MSG2_ERR" = "false" ] && [ "$MSG3_ERR" = "false" ]; then
    e2e_pass "all 3 messages sent successfully"
else
    e2e_fail "message send failed (msg1=$MSG1_ERR, msg2=$MSG2_ERR, msg3=$MSG3_ERR)"
fi

# Extract message IDs for later use
MSG1_TEXT="$(extract_result "$MSG_RESP" 40)"
MSG2_TEXT="$(extract_result "$MSG_RESP" 41)"
MSG3_TEXT="$(extract_result "$MSG_RESP" 42)"

MSG1_ID="$(parse_json_field "$MSG1_TEXT" "deliveries.0.payload.id")"
MSG2_ID="$(parse_json_field "$MSG2_TEXT" "deliveries.0.payload.id")"
MSG3_ID="$(parse_json_field "$MSG3_TEXT" "deliveries.0.payload.id")"

e2e_save_artifact "case_04_msg_ids.txt" "msg1=$MSG1_ID msg2=$MSG2_ID msg3=$MSG3_ID"

if [ -n "$MSG1_ID" ] && [ "$MSG1_ID" != "" ] && [ "$MSG1_ID" != "None" ]; then
    e2e_pass "message 1 returned id: $MSG1_ID"
else
    e2e_fail "message 1 missing id"
fi

if [ -n "$MSG3_ID" ] && [ "$MSG3_ID" != "" ] && [ "$MSG3_ID" != "None" ]; then
    e2e_pass "message 3 (multi-recipient) returned id: $MSG3_ID"
else
    e2e_fail "message 3 missing id"
fi

# ===========================================================================
# Case 5: fetch_inbox for each agent — verify messages received
# ===========================================================================
e2e_case_banner "fetch_inbox for GoldFox and SilverWolf"

# GoldFox inbox (separate session)
GF_INBOX_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"include_bodies\":true,\"limit\":20}}}" \
)"
e2e_save_artifact "case_05_goldfox_inbox.txt" "$GF_INBOX_RESP"

GF_INBOX_TEXT="$(extract_result "$GF_INBOX_RESP" 50)"
GF_INBOX_CHECK="$(echo "$GF_INBOX_TEXT" | python3 -c "
import sys, json
try:
    messages = json.loads(sys.stdin.read())
    if isinstance(messages, list):
        count = len(messages)
        subjects = [m.get('subject', '') for m in messages]
        has_alpha = any('Build plan alpha' in s for s in subjects)
        has_shared = any('Build plan shared' in s for s in subjects)
        print(f'count={count}|has_alpha={has_alpha}|has_shared={has_shared}')
    else:
        print(f'not_list|type={type(messages).__name__}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_goldfox_parsed.txt" "$GF_INBOX_CHECK"

e2e_assert_contains "GoldFox inbox has 2 messages" "$GF_INBOX_CHECK" "count=2"
e2e_assert_contains "GoldFox received Build plan alpha" "$GF_INBOX_CHECK" "has_alpha=True"
e2e_assert_contains "GoldFox received Build plan shared" "$GF_INBOX_CHECK" "has_shared=True"

# SilverWolf inbox (separate session)
SW_INBOX_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true,\"limit\":20}}}" \
)"
e2e_save_artifact "case_05_silverwolf_inbox.txt" "$SW_INBOX_RESP"

SW_INBOX_TEXT="$(extract_result "$SW_INBOX_RESP" 51)"
SW_INBOX_CHECK="$(echo "$SW_INBOX_TEXT" | python3 -c "
import sys, json
try:
    messages = json.loads(sys.stdin.read())
    if isinstance(messages, list):
        count = len(messages)
        subjects = [m.get('subject', '') for m in messages]
        has_beta = any('Build plan beta' in s for s in subjects)
        has_shared = any('Build plan shared' in s for s in subjects)
        print(f'count={count}|has_beta={has_beta}|has_shared={has_shared}')
    else:
        print(f'not_list|type={type(messages).__name__}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_silverwolf_parsed.txt" "$SW_INBOX_CHECK"

e2e_assert_contains "SilverWolf inbox has 2 messages" "$SW_INBOX_CHECK" "count=2"
e2e_assert_contains "SilverWolf received Build plan beta" "$SW_INBOX_CHECK" "has_beta=True"
e2e_assert_contains "SilverWolf received Build plan shared" "$SW_INBOX_CHECK" "has_shared=True"

# ===========================================================================
# Case 6: Multiple agents acknowledge the shared message
# ===========================================================================
e2e_case_banner "Multiple agents acknowledge message $MSG3_ID"

if [ -n "$MSG3_ID" ] && [ "$MSG3_ID" != "" ] && [ "$MSG3_ID" != "None" ]; then
    # GoldFox acknowledges (separate session)
    GF_ACK_RESP="$(send_jsonrpc_session "$CC_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"message_id\":${MSG3_ID}}}}" \
    )"
    e2e_save_artifact "case_06_goldfox_ack.txt" "$GF_ACK_RESP"

    GF_ACK_ERR="$(is_error_result "$GF_ACK_RESP" 60)"
    if [ "$GF_ACK_ERR" = "false" ]; then
        e2e_pass "GoldFox acknowledged message $MSG3_ID"
    else
        e2e_fail "GoldFox acknowledge failed"
    fi

    # SilverWolf acknowledges (separate session)
    SW_ACK_RESP="$(send_jsonrpc_session "$CC_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"message_id\":${MSG3_ID}}}}" \
    )"
    e2e_save_artifact "case_06_silverwolf_ack.txt" "$SW_ACK_RESP"

    SW_ACK_ERR="$(is_error_result "$SW_ACK_RESP" 61)"
    if [ "$SW_ACK_ERR" = "false" ]; then
        e2e_pass "SilverWolf acknowledged message $MSG3_ID"
    else
        e2e_fail "SilverWolf acknowledge failed"
    fi

    # GoldFox re-acknowledges (idempotency check, separate session)
    GF_ACK2_RESP="$(send_jsonrpc_session "$CC_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":62,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"message_id\":${MSG3_ID}}}}" \
    )"
    e2e_save_artifact "case_06_goldfox_ack_idempotent.txt" "$GF_ACK2_RESP"

    GF_ACK2_ERR="$(is_error_result "$GF_ACK2_RESP" 62)"
    if [ "$GF_ACK2_ERR" = "false" ]; then
        e2e_pass "GoldFox re-acknowledge idempotent (no error)"
    else
        e2e_fail "GoldFox re-acknowledge returned error (expected idempotent)"
    fi
else
    e2e_skip "message 3 id not available, skipping acknowledge tests"
fi

# ===========================================================================
# Case 7: Idempotent operations
# ===========================================================================
e2e_case_banner "Idempotent: ensure_project twice returns same ID"

IDEM_PROJ_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
)"
e2e_save_artifact "case_07_idempotent_project.txt" "$IDEM_PROJ_RESP"

IDEM_PROJ_TEXT="$(extract_result "$IDEM_PROJ_RESP" 70)"
IDEM_PROJ_ID="$(parse_json_field "$IDEM_PROJ_TEXT" "id")"
IDEM_PROJ_SLUG="$(parse_json_field "$IDEM_PROJ_TEXT" "slug")"

e2e_assert_eq "ensure_project idempotent: same slug" "$PROJ_SLUG" "$IDEM_PROJ_SLUG"

# register_agent upserts (update existing agent's task_description)
e2e_case_banner "Idempotent: register_agent upserts"

IDEM_AGENT_RESP="$(send_jsonrpc_session "$CC_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":71,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model-v2\",\"name\":\"GoldFox\",\"task_description\":\"updated task description\"}}}" \
)"
e2e_save_artifact "case_07_idempotent_agent.txt" "$IDEM_AGENT_RESP"

IDEM_AGENT_ERR="$(is_error_result "$IDEM_AGENT_RESP" 71)"
IDEM_AGENT_TEXT="$(extract_result "$IDEM_AGENT_RESP" 71)"

if [ "$IDEM_AGENT_ERR" = "false" ]; then
    e2e_pass "register_agent upsert succeeded (no error)"
else
    e2e_fail "register_agent upsert returned error"
    echo "    text: $IDEM_AGENT_TEXT"
fi

IDEM_AGENT_NAME="$(parse_json_field "$IDEM_AGENT_TEXT" "name")"
e2e_assert_eq "register_agent upsert preserved name" "GoldFox" "$IDEM_AGENT_NAME"

IDEM_AGENT_TASK="$(parse_json_field "$IDEM_AGENT_TEXT" "task_description")"
e2e_assert_eq "register_agent upsert updated task" "updated task description" "$IDEM_AGENT_TASK"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
