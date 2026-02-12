#!/usr/bin/env bash
# test_tools_build_slots.sh - E2E test for build slot tools
#
# Verifies the build slot lifecycle through the MCP stdio transport:
# acquire_build_slot, renew_build_slot, release_build_slot, and
# conflict detection when a second agent tries to acquire a held slot.
#
# Tests:
#   1. Setup: ensure_project + register 2 agents (GoldFox, SilverWolf)
#   2. acquire_build_slot: GoldFox acquires slot "build-1"
#   3. Verify granted response has slot name and agent
#   4. renew_build_slot: GoldFox renews slot "build-1"
#   5. acquire_build_slot: SilverWolf tries same slot (conflict expected)
#   6. release_build_slot: GoldFox releases slot "build-1"
#   7. acquire_build_slot: SilverWolf acquires slot "build-1" (succeeds now)

E2E_SUITE="tools_build_slots"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Build Slot Tools E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Build slots require WORKTREES_ENABLED=true in the server environment
export WORKTREES_ENABLED=true

# Temp workspace
WORK="$(e2e_mktemp "e2e_build_slots")"
SLOT_DB="${WORK}/build_slots_test.sqlite3"
PROJECT_PATH="/tmp/e2e_build_slots_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-build-slots","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers (same pattern as other E2E test scripts)
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

# Helper: extract JSON-RPC result content text by request id
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

# Helper: check if result is an MCP tool error or JSON-RPC error
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
            # JSON-RPC level error
            if 'error' in d:
                print('true')
                sys.exit(0)
            # MCP tool error (isError in result)
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
# Case 1: Setup - ensure_project + register 2 agents (GoldFox, SilverWolf)
# ===========================================================================
e2e_case_banner "Setup: project + two agents (GoldFox, SilverWolf)"

SETUP_RESP="$(send_jsonrpc_session "$SLOT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"build slot E2E testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"build slot E2E testing\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

PROJ_ERR="$(is_error_result "$SETUP_RESP" 10)"
GF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"

if [ "$PROJ_ERR" = "false" ]; then
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
# Case 2: acquire_build_slot - GoldFox acquires slot "build-1"
# ===========================================================================
e2e_case_banner "acquire_build_slot: GoldFox acquires slot build-1"

ACQUIRE_RESP="$(send_jsonrpc_session "$SLOT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"acquire_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-1\",\"ttl_seconds\":3600}}}" \
)"
e2e_save_artifact "case_02_acquire.txt" "$ACQUIRE_RESP"

ACQUIRE_ERR="$(is_error_result "$ACQUIRE_RESP" 20)"
ACQUIRE_TEXT="$(extract_result "$ACQUIRE_RESP" 20)"

if [ "$ACQUIRE_ERR" = "true" ]; then
    e2e_fail "acquire_build_slot GoldFox returned error"
    echo "    text: $ACQUIRE_TEXT"
else
    e2e_pass "acquire_build_slot GoldFox succeeded"
fi

# ===========================================================================
# Case 3: Verify granted response has slot name, agent, expires_ts, no conflicts
# ===========================================================================
e2e_case_banner "Verify acquire_build_slot granted response structure"

ACQUIRE_CHECK="$(echo "$ACQUIRE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', {})
    conflicts = result.get('conflicts', [])
    slot = granted.get('slot', '')
    agent = granted.get('agent', '')
    has_expires = bool(granted.get('expires_ts', ''))
    has_acquired = bool(granted.get('acquired_ts', ''))
    exclusive = granted.get('exclusive', False)
    print(f'slot={slot}|agent={agent}|has_expires={has_expires}|has_acquired={has_acquired}|exclusive={exclusive}|conflicts_len={len(conflicts)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_03_parsed.txt" "$ACQUIRE_CHECK"

e2e_assert_contains "granted slot is build-1" "$ACQUIRE_CHECK" "slot=build-1"
e2e_assert_contains "granted agent is GoldFox" "$ACQUIRE_CHECK" "agent=GoldFox"
e2e_assert_contains "granted has expires_ts" "$ACQUIRE_CHECK" "has_expires=True"
e2e_assert_contains "granted has acquired_ts" "$ACQUIRE_CHECK" "has_acquired=True"
e2e_assert_contains "granted is exclusive" "$ACQUIRE_CHECK" "exclusive=True"
e2e_assert_contains "no conflicts on first acquire" "$ACQUIRE_CHECK" "conflicts_len=0"

# ===========================================================================
# Case 4: renew_build_slot - GoldFox renews slot "build-1"
# ===========================================================================
e2e_case_banner "renew_build_slot: GoldFox renews slot build-1"

RENEW_RESP="$(send_jsonrpc_session "$SLOT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"renew_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-1\",\"extend_seconds\":1800}}}" \
)"
e2e_save_artifact "case_04_renew.txt" "$RENEW_RESP"

RENEW_ERR="$(is_error_result "$RENEW_RESP" 30)"
RENEW_TEXT="$(extract_result "$RENEW_RESP" 30)"

if [ "$RENEW_ERR" = "true" ]; then
    e2e_fail "renew_build_slot returned error"
    echo "    text: $RENEW_TEXT"
else
    e2e_pass "renew_build_slot succeeded"
fi

# Verify renew response: {renewed: true, expires_ts: "..."}
RENEW_CHECK="$(echo "$RENEW_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    renewed = result.get('renewed', False)
    has_expires = bool(result.get('expires_ts', ''))
    print(f'renewed={renewed}|has_expires={has_expires}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_04_parsed.txt" "$RENEW_CHECK"

e2e_assert_contains "renewed is True" "$RENEW_CHECK" "renewed=True"
e2e_assert_contains "renew response has expires_ts" "$RENEW_CHECK" "has_expires=True"

# ===========================================================================
# Case 5: acquire_build_slot - SilverWolf tries same slot (conflict expected)
# ===========================================================================
e2e_case_banner "acquire_build_slot: SilverWolf conflict on build-1"

CONFLICT_RESP="$(send_jsonrpc_session "$SLOT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"acquire_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"slot\":\"build-1\",\"ttl_seconds\":3600}}}" \
)"
e2e_save_artifact "case_05_conflict.txt" "$CONFLICT_RESP"

CONFLICT_ERR="$(is_error_result "$CONFLICT_RESP" 40)"
CONFLICT_TEXT="$(extract_result "$CONFLICT_RESP" 40)"

# The tool should succeed (not error) but report conflicts in the response
if [ "$CONFLICT_ERR" = "true" ]; then
    # Some implementations may return an MCP error for conflicts; that is also acceptable
    e2e_pass "acquire_build_slot SilverWolf returned error (conflict signaled)"
else
    e2e_pass "acquire_build_slot SilverWolf returned result (checking for conflicts)"
fi

CONFLICT_CHECK="$(echo "$CONFLICT_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', {})
    conflicts = result.get('conflicts', [])
    conflicts_len = len(conflicts)
    # Check if any conflict mentions GoldFox as the holder
    has_goldfox_conflict = False
    for c in conflicts:
        agent = c.get('agent', '')
        if 'GoldFox' in agent:
            has_goldfox_conflict = True
    print(f'conflicts_len={conflicts_len}|has_goldfox_conflict={has_goldfox_conflict}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_parsed.txt" "$CONFLICT_CHECK"

e2e_assert_contains "conflicts detected for SilverWolf" "$CONFLICT_CHECK" "conflicts_len=1"
e2e_assert_contains "GoldFox is the conflicting holder" "$CONFLICT_CHECK" "has_goldfox_conflict=True"

# ===========================================================================
# Case 6: release_build_slot - GoldFox releases slot "build-1"
# ===========================================================================
e2e_case_banner "release_build_slot: GoldFox releases slot build-1"

RELEASE_RESP="$(send_jsonrpc_session "$SLOT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"release_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-1\"}}}" \
)"
e2e_save_artifact "case_06_release.txt" "$RELEASE_RESP"

RELEASE_ERR="$(is_error_result "$RELEASE_RESP" 50)"
RELEASE_TEXT="$(extract_result "$RELEASE_RESP" 50)"

if [ "$RELEASE_ERR" = "true" ]; then
    e2e_fail "release_build_slot returned error"
    echo "    text: $RELEASE_TEXT"
else
    e2e_pass "release_build_slot succeeded"
fi

# Verify release response: {released: true, released_at: "..."}
RELEASE_CHECK="$(echo "$RELEASE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    released = result.get('released', False)
    has_released_at = bool(result.get('released_at', ''))
    print(f'released={released}|has_released_at={has_released_at}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_06_parsed.txt" "$RELEASE_CHECK"

e2e_assert_contains "released is True" "$RELEASE_CHECK" "released=True"
e2e_assert_contains "release response has released_at" "$RELEASE_CHECK" "has_released_at=True"

# ===========================================================================
# Case 7: acquire_build_slot - SilverWolf acquires after GoldFox released
# ===========================================================================
e2e_case_banner "acquire_build_slot: SilverWolf acquires build-1 after release"

POST_RELEASE_RESP="$(send_jsonrpc_session "$SLOT_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"acquire_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"slot\":\"build-1\",\"ttl_seconds\":3600}}}" \
)"
e2e_save_artifact "case_07_post_release_acquire.txt" "$POST_RELEASE_RESP"

POST_RELEASE_ERR="$(is_error_result "$POST_RELEASE_RESP" 60)"
POST_RELEASE_TEXT="$(extract_result "$POST_RELEASE_RESP" 60)"

if [ "$POST_RELEASE_ERR" = "true" ]; then
    e2e_fail "acquire_build_slot SilverWolf after release returned error"
    echo "    text: $POST_RELEASE_TEXT"
else
    e2e_pass "acquire_build_slot SilverWolf after release succeeded"
fi

# Verify: granted with SilverWolf as agent and no conflicts
POST_RELEASE_CHECK="$(echo "$POST_RELEASE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', {})
    conflicts = result.get('conflicts', [])
    slot = granted.get('slot', '')
    agent = granted.get('agent', '')
    has_expires = bool(granted.get('expires_ts', ''))
    print(f'slot={slot}|agent={agent}|has_expires={has_expires}|conflicts_len={len(conflicts)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_07_parsed.txt" "$POST_RELEASE_CHECK"

e2e_assert_contains "SilverWolf granted slot build-1" "$POST_RELEASE_CHECK" "slot=build-1"
e2e_assert_contains "SilverWolf is the agent" "$POST_RELEASE_CHECK" "agent=SilverWolf"
e2e_assert_contains "grant has expires_ts" "$POST_RELEASE_CHECK" "has_expires=True"
e2e_assert_contains "no conflicts after release" "$POST_RELEASE_CHECK" "conflicts_len=0"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
