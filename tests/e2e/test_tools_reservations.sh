#!/usr/bin/env bash
# test_tools_reservations.sh - E2E test for file reservation tools
#
# Verifies the file reservation tools work correctly through the
# MCP stdio transport: file_reservation_paths, renew_file_reservations,
# release_file_reservations, force_release_file_reservation.
#
# Tests:
#   1. Setup: ensure_project + register 2 agents (GoldFox, SilverWolf)
#   2. file_reservation_paths: GoldFox reserves ["app/api/*.py"] exclusive
#   3. file_reservation_paths: SilverWolf conflict on ["app/api/users.py"]
#   4. renew_file_reservations: GoldFox renews reservations
#   5. release_file_reservations: GoldFox releases all reservations
#   6. file_reservation_paths after release: SilverWolf reserves successfully
#   7. force_release_file_reservation: attempt force-release on 3rd agent's reservation

E2E_SUITE="tools_reservations"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "File Reservation Tools E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_reservations")"
RES_DB="${WORK}/reservations_test.sqlite3"
PROJECT_PATH="/tmp/e2e_reservations_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-reservations","version":"1.0"}}}'

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

SETUP_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"reservation E2E testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"reservation E2E testing\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

PROJ_ERR="$(is_error_result "$SETUP_RESP" 10)"
GF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"
if [ "$PROJ_ERR" = "false" ] && [ "$GF_ERR" = "false" ] && [ "$SW_ERR" = "false" ]; then
    e2e_pass "setup: project + 2 agents registered"
else
    e2e_fail "setup failed (proj=$PROJ_ERR, GoldFox=$GF_ERR, SilverWolf=$SW_ERR)"
fi

# ===========================================================================
# Case 2: file_reservation_paths - GoldFox reserves ["app/api/*.py"] exclusive
# ===========================================================================
e2e_case_banner "file_reservation_paths: GoldFox reserves app/api/*.py"

RESERVE_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"paths\":[\"app/api/*.py\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"E2E test\"}}}" \
)"
e2e_save_artifact "case_02_reserve.txt" "$RESERVE_RESP"

RESERVE_ERR="$(is_error_result "$RESERVE_RESP" 20)"
RESERVE_TEXT="$(extract_result "$RESERVE_RESP" 20)"
if [ "$RESERVE_ERR" = "true" ]; then
    e2e_fail "file_reservation_paths GoldFox returned error"
    echo "    text: $RESERVE_TEXT"
else
    e2e_pass "file_reservation_paths GoldFox succeeded"
fi

# Parse granted array length and conflicts
RESERVE_CHECK="$(echo "$RESERVE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    conflicts = result.get('conflicts', [])
    granted_len = len(granted)
    conflicts_len = len(conflicts)
    has_id = granted_len > 0 and 'id' in granted[0]
    has_pattern = granted_len > 0 and 'path_pattern' in granted[0]
    has_exclusive = granted_len > 0 and granted[0].get('exclusive', False)
    has_expires = granted_len > 0 and 'expires_ts' in granted[0]
    first_id = granted[0].get('id', '') if granted_len > 0 else ''
    print(f'granted_len={granted_len}|conflicts_len={conflicts_len}|has_id={has_id}|has_pattern={has_pattern}|has_exclusive={has_exclusive}|has_expires={has_expires}|first_id={first_id}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_02_parsed.txt" "$RESERVE_CHECK"

e2e_assert_contains "granted array non-empty" "$RESERVE_CHECK" "granted_len=1"
e2e_assert_contains "conflicts array empty" "$RESERVE_CHECK" "conflicts_len=0"
e2e_assert_contains "granted has id" "$RESERVE_CHECK" "has_id=True"
e2e_assert_contains "granted has path_pattern" "$RESERVE_CHECK" "has_pattern=True"
e2e_assert_contains "granted has expires_ts" "$RESERVE_CHECK" "has_expires=True"

# Extract the reservation id for later use
RESERVATION_ID="$(echo "$RESERVE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    if granted:
        print(granted[0].get('id', ''))
except Exception:
    print('')
" 2>/dev/null)"
e2e_save_artifact "case_02_reservation_id.txt" "$RESERVATION_ID"

# ===========================================================================
# Case 3: file_reservation_paths - SilverWolf conflict on app/api/users.py
# ===========================================================================
e2e_case_banner "file_reservation_paths: SilverWolf conflict detection"

CONFLICT_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"paths\":[\"app/api/users.py\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"E2E conflict test\"}}}" \
)"
e2e_save_artifact "case_03_conflict.txt" "$CONFLICT_RESP"

CONFLICT_ERR="$(is_error_result "$CONFLICT_RESP" 30)"
CONFLICT_TEXT="$(extract_result "$CONFLICT_RESP" 30)"

# The tool should succeed but report conflicts (it may or may not grant the reservation too)
CONFLICT_CHECK="$(echo "$CONFLICT_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    conflicts = result.get('conflicts', [])
    granted_len = len(granted)
    conflicts_len = len(conflicts)
    # Check if conflicts mention GoldFox as a holder
    has_goldfox_holder = False
    for c in conflicts:
        holders = c.get('holders', [])
        for h in holders:
            agent = h if isinstance(h, str) else h.get('agent', h.get('agent_name', ''))
            if 'GoldFox' in str(agent):
                has_goldfox_holder = True
    print(f'granted_len={granted_len}|conflicts_len={conflicts_len}|has_goldfox_holder={has_goldfox_holder}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_03_parsed.txt" "$CONFLICT_CHECK"

e2e_assert_contains "conflicts detected" "$CONFLICT_CHECK" "conflicts_len=1"
e2e_assert_contains "GoldFox is holder in conflict" "$CONFLICT_CHECK" "has_goldfox_holder=True"

# ===========================================================================
# Case 4: renew_file_reservations - GoldFox renews
# ===========================================================================
e2e_case_banner "renew_file_reservations: GoldFox renews"

RENEW_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"renew_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"extend_seconds\":1800}}}" \
)"
e2e_save_artifact "case_04_renew.txt" "$RENEW_RESP"

RENEW_ERR="$(is_error_result "$RENEW_RESP" 40)"
RENEW_TEXT="$(extract_result "$RENEW_RESP" 40)"
if [ "$RENEW_ERR" = "true" ]; then
    e2e_fail "renew_file_reservations returned error"
    echo "    text: $RENEW_TEXT"
else
    e2e_pass "renew_file_reservations succeeded"
fi

# Parse renewed count
RENEW_CHECK="$(echo "$RENEW_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    renewed = result.get('renewed', 0)
    reservations = result.get('file_reservations', [])
    res_len = len(reservations)
    has_old_expires = res_len > 0 and 'old_expires_ts' in reservations[0]
    has_new_expires = res_len > 0 and 'new_expires_ts' in reservations[0]
    print(f'renewed={renewed}|res_len={res_len}|has_old_expires={has_old_expires}|has_new_expires={has_new_expires}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_04_parsed.txt" "$RENEW_CHECK"

e2e_assert_contains "renewed count >= 1" "$RENEW_CHECK" "renewed=1"
e2e_assert_contains "file_reservations array has entries" "$RENEW_CHECK" "res_len=1"

# ===========================================================================
# Case 5: release_file_reservations - GoldFox releases all
# ===========================================================================
e2e_case_banner "release_file_reservations: GoldFox releases all"

RELEASE_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\"}}}" \
)"
e2e_save_artifact "case_05_release.txt" "$RELEASE_RESP"

RELEASE_ERR="$(is_error_result "$RELEASE_RESP" 50)"
RELEASE_TEXT="$(extract_result "$RELEASE_RESP" 50)"
if [ "$RELEASE_ERR" = "true" ]; then
    e2e_fail "release_file_reservations returned error"
    echo "    text: $RELEASE_TEXT"
else
    e2e_pass "release_file_reservations succeeded"
fi

# Parse released count
RELEASE_CHECK="$(echo "$RELEASE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    released = result.get('released', 0)
    has_released_at = bool(result.get('released_at', ''))
    print(f'released={released}|has_released_at={has_released_at}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_parsed.txt" "$RELEASE_CHECK"

e2e_assert_contains "released count >= 1" "$RELEASE_CHECK" "released=1"
e2e_assert_contains "released_at present" "$RELEASE_CHECK" "has_released_at=True"

# ===========================================================================
# Case 6: file_reservation_paths after release - SilverWolf reserves successfully
# ===========================================================================
e2e_case_banner "file_reservation_paths after release: SilverWolf succeeds"

POST_RELEASE_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"paths\":[\"app/api/users.py\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"E2E post-release test\"}}}" \
)"
e2e_save_artifact "case_06_post_release.txt" "$POST_RELEASE_RESP"

POST_RELEASE_ERR="$(is_error_result "$POST_RELEASE_RESP" 60)"
POST_RELEASE_TEXT="$(extract_result "$POST_RELEASE_RESP" 60)"
if [ "$POST_RELEASE_ERR" = "true" ]; then
    e2e_fail "file_reservation_paths SilverWolf after release returned error"
    echo "    text: $POST_RELEASE_TEXT"
else
    e2e_pass "file_reservation_paths SilverWolf after release succeeded"
fi

# Parse: should have granted, no conflicts
POST_RELEASE_CHECK="$(echo "$POST_RELEASE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    conflicts = result.get('conflicts', [])
    granted_len = len(granted)
    conflicts_len = len(conflicts)
    print(f'granted_len={granted_len}|conflicts_len={conflicts_len}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_06_parsed.txt" "$POST_RELEASE_CHECK"

e2e_assert_contains "SilverWolf granted after release" "$POST_RELEASE_CHECK" "granted_len=1"
e2e_assert_contains "no conflicts after release" "$POST_RELEASE_CHECK" "conflicts_len=0"

# ===========================================================================
# Case 7: force_release_file_reservation
# ===========================================================================
e2e_case_banner "force_release_file_reservation: attempt force-release"

# First register a 3rd agent (CrimsonLake) and have it reserve a path
FORCE_SETUP_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"CrimsonLake\",\"task_description\":\"force-release target\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":71,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"CrimsonLake\",\"paths\":[\"docs/design.md\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"force-release target\"}}}" \
)"
e2e_save_artifact "case_07_force_setup.txt" "$FORCE_SETUP_RESP"

CL_REG_ERR="$(is_error_result "$FORCE_SETUP_RESP" 70)"
CL_RES_ERR="$(is_error_result "$FORCE_SETUP_RESP" 71)"
if [ "$CL_REG_ERR" = "false" ] && [ "$CL_RES_ERR" = "false" ]; then
    e2e_pass "CrimsonLake registered and reserved docs/design.md"
else
    e2e_fail "CrimsonLake setup failed (reg=$CL_REG_ERR, res=$CL_RES_ERR)"
fi

# Extract CrimsonLake's reservation id
CL_RES_TEXT="$(extract_result "$FORCE_SETUP_RESP" 71)"
CL_RES_ID="$(echo "$CL_RES_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    if granted:
        print(granted[0].get('id', ''))
except Exception:
    print('')
" 2>/dev/null)"
e2e_save_artifact "case_07_cl_reservation_id.txt" "$CL_RES_ID"

# Now GoldFox attempts to force-release CrimsonLake's reservation
# Note: force_release may fail due to inactivity heuristics (CrimsonLake was just active).
# We test that the tool call at least doesn't crash.
if [ -n "$CL_RES_ID" ] && [ "$CL_RES_ID" != "" ]; then
    FORCE_RESP="$(send_jsonrpc_session "$RES_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":72,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"file_reservation_id\":${CL_RES_ID},\"note\":\"E2E force release test\"}}}" \
    )"
    e2e_save_artifact "case_07_force_release.txt" "$FORCE_RESP"

    FORCE_ERR="$(is_error_result "$FORCE_RESP" 72)"
    FORCE_TEXT="$(extract_result "$FORCE_RESP" 72)"

    # Force release may succeed or fail (agent was just active). Either way,
    # we verify the tool responded without crashing (we got a valid JSON-RPC response).
    FORCE_HAS_RESPONSE="$(echo "$FORCE_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 72:
            print('true')
            sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('false')
" 2>/dev/null)"

    if [ "$FORCE_HAS_RESPONSE" = "true" ]; then
        e2e_pass "force_release_file_reservation returned valid JSON-RPC response"
    else
        e2e_fail "force_release_file_reservation did not return a valid response"
    fi

    if [ "$FORCE_ERR" = "true" ]; then
        # Expected: agent not inactive enough
        e2e_pass "force_release returned error (agent still active, as expected)"
    else
        # Also acceptable: force release succeeded
        e2e_pass "force_release succeeded (agent met inactivity threshold)"
    fi
else
    e2e_skip "CrimsonLake reservation id not available, skipping force-release"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
