#!/usr/bin/env bash
# test_force_release_signals.sh - E2E: Force-release multi-signal heuristic aggregation
#
# Verifies (br-3h13.10.4):
# 1. Recent agent/mail activity prevents release and preserves the lease
# 2. Four stale signals authorize exactly one real release by another agent
# 3. Force-release of an already-released reservation is an idempotent no-op
# 4. A nonexistent reservation returns NOT_FOUND
# 5. notify_previous=true creates a real holder notification
# 6. notify_previous=false releases without sending holder mail
# 7. The holder can force-release its actually expired lease
# 8. Detailed note text survives in the notification body
#
# Target: 10+ assertions

E2E_SUITE="force_release_signals"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Force-Release Multi-Signal Heuristic E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_force_release")"
FR_DB="${WORK}/force_release_test.sqlite3"
TEST_STORAGE_ROOT="${WORK}/storage"
PROJECT_PATH="${WORK}/project"
mkdir -p "$TEST_STORAGE_ROOT"
mkdir -p "$PROJECT_PATH"/{stale_files,active_files,notify_test,self_release,note_test}
# Real tracked paths provide complete filesystem/Git evidence. Old commit and
# file timestamps represent inactivity without weakening production thresholds.
for directory in stale_files active_files notify_test self_release note_test; do
    printf 'reservation activity fixture\n' >> "$PROJECT_PATH/$directory/lease.rs"
done
git -C "$PROJECT_PATH" init --quiet
git -C "$PROJECT_PATH" config user.name "Force Release E2E"
git -C "$PROJECT_PATH" config user.email "force-release@example.invalid"
git -C "$PROJECT_PATH" add .
GIT_AUTHOR_DATE="2000-01-01T00:00:00Z" GIT_COMMITTER_DATE="2000-01-01T00:00:00Z" \
    git -C "$PROJECT_PATH" commit --quiet -m "Seed inactive reservation paths"
for directory in stale_files active_files notify_test self_release note_test; do
    touch -t 200001010000.00 "$PROJECT_PATH/$directory/lease.rs"
done

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-force-release","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    E2E_STDIO_TIMEOUT_SECONDS=100 STORAGE_ROOT="$TEST_STORAGE_ROOT" RUST_LOG=error \
        e2e_stdio_session "$@"
}

seed_inactive_holder() {
    local holder="$1"
    local changed
    changed="$(sqlite3 "$FR_DB" "UPDATE agents SET inception_ts=946684800000000, last_active_ts=946684800000000 WHERE name='${holder}'; SELECT changes();")"
    e2e_assert_eq "one private $holder heartbeat aged" "1" "$changed"
    e2e_save_artifact "seed_${holder}_activity.txt" \
        "$(sqlite3 "$FR_DB" "SELECT id, name, inception_ts, last_active_ts FROM agents WHERE name='${holder}';")"
}

extract_result() {
    local response="$1"
    local req_id="$2"
    python3 -c "
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
" <<< "$response" 2>/dev/null
}

is_error_result() {
    local response="$1"
    local req_id="$2"
    python3 -c "
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
" <<< "$response" 2>/dev/null
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
    check="$(python3 -c "
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
" <<< "$resp" 2>/dev/null)"

    case "$check" in
        OK) e2e_pass "$label" ;;
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label -> error: $check" ;;
        NO_MATCH) e2e_fail "$label -> no response for id=$id" ;;
    esac
}

# ===========================================================================
# Setup: Create project and agents.
# ===========================================================================
STALE_AGENT="SilverPine"
ACTIVE_AGENT="BlueLake"
RELEASER_AGENT="RedStone"
NOTIFY_AGENT="GoldLake"
NOTE_AGENT="GreenCastle"

e2e_case_banner "Setup: project + agents (${STALE_AGENT}, ${ACTIVE_AGENT}, ${RELEASER_AGENT})"

SETUP_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"${STALE_AGENT}\",\"task_description\":\"force-release E2E (will become stale)\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"${ACTIVE_AGENT}\",\"task_description\":\"force-release E2E (stays active)\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"${RELEASER_AGENT}\",\"task_description\":\"force-release E2E (requests force-release)\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"${NOTIFY_AGENT}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":15,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"${NOTE_AGENT}\"}}}" \
)"
e2e_save_artifact "case_00_setup.txt" "$SETUP_RESP"

assert_ok "ensure_project" "$SETUP_RESP" 10
assert_ok "register ${STALE_AGENT}" "$SETUP_RESP" 11
assert_ok "register ${ACTIVE_AGENT}" "$SETUP_RESP" 12
assert_ok "register ${RELEASER_AGENT}" "$SETUP_RESP" 13
assert_ok "register ${NOTIFY_AGENT}" "$SETUP_RESP" 14
assert_ok "register ${NOTE_AGENT}" "$SETUP_RESP" 15

# ===========================================================================
# Case 1: stale agent creates a reservation
# ===========================================================================
e2e_case_banner "${STALE_AGENT} creates a reservation"

STALE_RES_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${STALE_AGENT}\",\"paths\":[\"stale_files/*.rs\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"${STALE_AGENT} reservation for force-release test\"}}}" \
)"
e2e_save_artifact "case_01_stale_reserve.txt" "$STALE_RES_RESP"

STALE_RES_ERR="$(is_error_result "$STALE_RES_RESP" 20)"
STALE_RES_TEXT="$(extract_result "$STALE_RES_RESP" 20)"

if [ "$STALE_RES_ERR" = "false" ]; then
    e2e_pass "${STALE_AGENT} reservation created"
else
    e2e_fail "${STALE_AGENT} reservation failed"
fi

# Extract reservation ID
STALE_RES_ID="$(python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    if granted:
        print(granted[0].get('id', ''))
except Exception:
    print('')
" <<< "$STALE_RES_TEXT" 2>/dev/null)"

e2e_save_artifact "case_01_stale_res_id.txt" "$STALE_RES_ID"

if [ -n "$STALE_RES_ID" ] && [ "$STALE_RES_ID" != "" ]; then
    e2e_pass "${STALE_AGENT} reservation ID obtained: $STALE_RES_ID"
else
    e2e_fail "${STALE_AGENT} reservation ID not found"
fi

# ===========================================================================
# Case 2: active agent creates a reservation (with recent activity)
# ===========================================================================
e2e_case_banner "${ACTIVE_AGENT} creates a reservation with activity"

# The active agent sends a message to show activity.
ACTIVE_RES_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${ACTIVE_AGENT}\",\"paths\":[\"active_files/*.rs\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"${ACTIVE_AGENT} reservation\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"${ACTIVE_AGENT}\",\"to\":[\"${RELEASER_AGENT}\"],\"subject\":\"I am active\",\"body_md\":\"Recent mail activity for staleness check.\"}}}" \
)"
e2e_save_artifact "case_02_active_reserve.txt" "$ACTIVE_RES_RESP"

assert_ok "${ACTIVE_AGENT} reservation created" "$ACTIVE_RES_RESP" 30
assert_ok "${ACTIVE_AGENT} sent message (activity)" "$ACTIVE_RES_RESP" 31

ACTIVE_RES_TEXT="$(extract_result "$ACTIVE_RES_RESP" 30)"
ACTIVE_RES_ID="$(python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    if granted:
        print(granted[0].get('id', ''))
except Exception:
    print('')
" <<< "$ACTIVE_RES_TEXT" 2>/dev/null)"

e2e_save_artifact "case_02_active_res_id.txt" "$ACTIVE_RES_ID"

if [ -n "$ACTIVE_RES_ID" ] && [ "$ACTIVE_RES_ID" != "" ]; then
    e2e_pass "${ACTIVE_AGENT} reservation ID obtained: $ACTIVE_RES_ID"
else
    e2e_fail "${ACTIVE_AGENT} reservation ID not found"
fi

# ===========================================================================
# Case 3: Force-release on active agent's reservation (should fail - recent activity)
# ===========================================================================
e2e_case_banner "Force-release ${ACTIVE_AGENT} reservation (expect fail - recent activity)"

if [ -n "$ACTIVE_RES_ID" ] && [ "$ACTIVE_RES_ID" != "" ]; then
    FORCE_ACTIVE_RESP="$(send_jsonrpc_session "$FR_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RELEASER_AGENT}\",\"file_reservation_id\":${ACTIVE_RES_ID},\"note\":\"Attempting force release on active agent\"}}}" \
    )"
    e2e_save_artifact "case_03_force_active.txt" "$FORCE_ACTIVE_RESP"

    FORCE_ACTIVE_ERR="$(is_error_result "$FORCE_ACTIVE_RESP" 40)"
    FORCE_ACTIVE_TEXT="$(extract_result "$FORCE_ACTIVE_RESP" 40)"

    if [ "$FORCE_ACTIVE_ERR" = "true" ]; then
        e2e_pass "force-release rejected: ${ACTIVE_AGENT} has recent activity"
        e2e_assert_contains "active reservation refusal names the activity policy" "$FORCE_ACTIVE_RESP" "RESERVATION_ACTIVE"
    else
        e2e_fail "force-release must refuse an active reservation"
    fi
    e2e_assert_eq "active reservation remains unreleased" "1" \
        "$(sqlite3 "$FR_DB" "SELECT COUNT(*) FROM file_reservations WHERE id=${ACTIVE_RES_ID} AND released_ts IS NULL;")"
else
    e2e_fail "${ACTIVE_AGENT} reservation ID not available for force-release test"
fi

# ===========================================================================
# Case 4: Force-release on stale agent's reservation (should succeed)
# ===========================================================================
e2e_case_banner "Force-release ${STALE_AGENT} reservation (expect success)"

if [ -n "$STALE_RES_ID" ] && [ "$STALE_RES_ID" != "" ]; then
    seed_inactive_holder "$STALE_AGENT"
    FORCE_STALE_RESP="$(send_jsonrpc_session "$FR_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RELEASER_AGENT}\",\"file_reservation_id\":${STALE_RES_ID},\"note\":\"Force releasing stale reservation\",\"notify_previous\":true}}}" \
    )"
    e2e_save_artifact "case_04_force_stale.txt" "$FORCE_STALE_RESP"

    FORCE_STALE_ERR="$(is_error_result "$FORCE_STALE_RESP" 50)"
    FORCE_STALE_TEXT="$(extract_result "$FORCE_STALE_RESP" 50)"

    if [ "$FORCE_STALE_ERR" = "false" ]; then
        e2e_pass "force-release ${STALE_AGENT} succeeded"

        RELEASED="$(parse_json_field "$FORCE_STALE_TEXT" "released")"
        e2e_assert_eq "one stale reservation was released" "1" "$RELEASED"
        e2e_assert_eq "released reservation retains its holder" "$STALE_AGENT" \
            "$(parse_json_field "$FORCE_STALE_TEXT" "reservation.agent")"
        e2e_assert_eq "previous holder was notified" "True" \
            "$(parse_json_field "$FORCE_STALE_TEXT" "reservation.notified")"
        FORCE_SIGNALS_CHECK="$(echo "$FORCE_STALE_TEXT" | python3 -c '
from datetime import datetime, timezone
import json, sys
reservation = json.load(sys.stdin)["reservation"]
now = datetime.now(timezone.utc)
for field in ["last_agent_activity_ts", "last_filesystem_activity_ts", "last_git_activity_ts"]:
    value = reservation.get(field)
    old = value is not None and (now - datetime.fromisoformat(value.replace("Z", "+00:00"))).total_seconds() > 1800
    print(f"{field}_old={old}")
value = reservation.get("last_mail_activity_ts")
mail_stale = value is None or (now - datetime.fromisoformat(value.replace("Z", "+00:00"))).total_seconds() > 1800
print(f"mail_stale={mail_stale}")
')"
        e2e_save_artifact "case_04_stale_signals.txt" "$FORCE_SIGNALS_CHECK"
        for field in last_agent_activity_ts last_filesystem_activity_ts last_git_activity_ts; do
            e2e_assert_contains "stale release observed old $field" "$FORCE_SIGNALS_CHECK" "${field}_old=True"
        done
        e2e_assert_contains "stale release observed no recent mail" "$FORCE_SIGNALS_CHECK" "mail_stale=True"
    else
        e2e_fail "force-release must release a reservation with four stale signals: $FORCE_STALE_TEXT"
    fi
else
    e2e_fail "${STALE_AGENT} reservation ID not available for force-release test"
fi

# ===========================================================================
# Case 5: Force-release on already-released reservation (no-op)
# ===========================================================================
e2e_case_banner "Force-release already-released reservation (no-op)"

if [ -n "$STALE_RES_ID" ] && [ "$STALE_RES_ID" != "" ]; then
    FORCE_AGAIN_RESP="$(send_jsonrpc_session "$FR_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RELEASER_AGENT}\",\"file_reservation_id\":${STALE_RES_ID},\"note\":\"Attempting to force-release already-released\"}}}" \
    )"
    e2e_save_artifact "case_05_force_again.txt" "$FORCE_AGAIN_RESP"

    FORCE_AGAIN_ERR="$(is_error_result "$FORCE_AGAIN_RESP" 60)"
    FORCE_AGAIN_TEXT="$(extract_result "$FORCE_AGAIN_RESP" 60)"

    if [ "$FORCE_AGAIN_ERR" = "true" ]; then
        e2e_fail "already-released reservation must return an idempotent no-op"
    else
        e2e_assert_eq "already-released reservation releases no second row" "0" \
            "$(parse_json_field "$FORCE_AGAIN_TEXT" "released")"
        e2e_assert_eq "already-released reservation is identified" "True" \
            "$(parse_json_field "$FORCE_AGAIN_TEXT" "already_released")"
    fi
else
    e2e_fail "${STALE_AGENT} reservation ID not available for no-op test"
fi

# ===========================================================================
# Case 6: Force-release with nonexistent reservation ID
# ===========================================================================
e2e_case_banner "Force-release nonexistent reservation ID"

FORCE_NONEXIST_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RELEASER_AGENT}\",\"file_reservation_id\":999999,\"note\":\"Testing nonexistent reservation\"}}}" \
)"
e2e_save_artifact "case_06_force_nonexist.txt" "$FORCE_NONEXIST_RESP"

FORCE_NONEXIST_ERR="$(is_error_result "$FORCE_NONEXIST_RESP" 70)"
FORCE_NONEXIST_TEXT="$(extract_result "$FORCE_NONEXIST_RESP" 70)"

if [ "$FORCE_NONEXIST_ERR" = "true" ]; then
    e2e_pass "force-release on nonexistent ID correctly returned error"
    e2e_assert_contains "nonexistent reservation names NOT_FOUND" "$FORCE_NONEXIST_RESP" "NOT_FOUND"
else
    e2e_fail "nonexistent reservation must return NOT_FOUND"
fi

# ===========================================================================
# Case 7: Force-release with notify_previous=false
# ===========================================================================
e2e_case_banner "Force-release with notify_previous=false"

# Create another reservation for this test
NOTIFY_RES_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":75,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${NOTIFY_AGENT}\",\"paths\":[\"notify_test/*.rs\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"Notify test\"}}}" \
)"
e2e_save_artifact "case_07_notify_reserve.txt" "$NOTIFY_RES_RESP"

NOTIFY_RES_TEXT="$(extract_result "$NOTIFY_RES_RESP" 75)"
NOTIFY_RES_ID="$(python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    if granted:
        print(granted[0].get('id', ''))
except Exception:
    print('')
" <<< "$NOTIFY_RES_TEXT" 2>/dev/null)"

if [ -n "$NOTIFY_RES_ID" ] && [ "$NOTIFY_RES_ID" != "" ]; then
    seed_inactive_holder "$NOTIFY_AGENT"
    FORCE_NONOTIFY_RESP="$(send_jsonrpc_session "$FR_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RELEASER_AGENT}\",\"file_reservation_id\":${NOTIFY_RES_ID},\"note\":\"Silent force release\",\"notify_previous\":false}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":81,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${NOTIFY_AGENT}\",\"include_bodies\":true}}}" \
    )"
    e2e_save_artifact "case_07_force_nonotify.txt" "$FORCE_NONOTIFY_RESP"

    FORCE_NONOTIFY_ERR="$(is_error_result "$FORCE_NONOTIFY_RESP" 80)"

    if [ "$FORCE_NONOTIFY_ERR" = "false" ]; then
        e2e_pass "force-release with notify_previous=false succeeded"
        FORCE_NONOTIFY_TEXT="$(extract_result "$FORCE_NONOTIFY_RESP" 80)"
        e2e_assert_eq "silent force release releases one row" "1" \
            "$(parse_json_field "$FORCE_NONOTIFY_TEXT" "released")"
        e2e_assert_eq "silent force release reports no notification" "False" \
            "$(parse_json_field "$FORCE_NONOTIFY_TEXT" "reservation.notified")"
        assert_ok "silent previous holder inbox remains readable" "$FORCE_NONOTIFY_RESP" 81
        e2e_assert_eq "silent previous holder receives no mail" "[]" \
            "$(extract_result "$FORCE_NONOTIFY_RESP" 81)"
    else
        e2e_fail "silent force release must release the inactive holder's lease"
    fi
else
    e2e_fail "Notify test reservation ID not available"
fi

# ===========================================================================
# Case 8: Verify notification message sent to previous holder
# ===========================================================================
e2e_case_banner "Check notifications in ${STALE_AGENT} inbox"

INBOX_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${STALE_AGENT}\",\"include_bodies\":true}}}" \
)"
e2e_save_artifact "case_08_inbox.txt" "$INBOX_RESP"

assert_ok "fetch_inbox ${STALE_AGENT} succeeded" "$INBOX_RESP" 90

INBOX_TEXT="$(extract_result "$INBOX_RESP" 90)"

# Check if there's a force-release notification
NOTIFICATION_CHECK="$(python3 -c "
import sys, json
try:
    msgs = json.loads(sys.stdin.read())
    if not isinstance(msgs, list):
        print('not_list')
        sys.exit(0)

    force_release_msgs = [m for m in msgs if 'force' in m.get('subject', '').lower() or 'release' in m.get('subject', '').lower() or 'reservation' in m.get('body_md', '').lower()]
    print(f'total={len(msgs)},force_release={len(force_release_msgs)}')
except Exception:
    print('error')
" <<< "$INBOX_TEXT" 2>/dev/null)"

e2e_save_artifact "case_08_notification_check.txt" "$NOTIFICATION_CHECK"

if echo "$NOTIFICATION_CHECK" | grep -q "force_release=[1-9]"; then
    e2e_pass "force-release notification found in ${STALE_AGENT} inbox"
else
    e2e_fail "force-release notification must appear in ${STALE_AGENT} inbox"
fi

# ===========================================================================
# Case 9: Force-release as the reservation holder (self-release)
# ===========================================================================
e2e_case_banner "Force-release an expired lease as its reservation holder"

# Create a reservation for the active agent.
SELF_RES_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":95,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${ACTIVE_AGENT}\",\"paths\":[\"self_release/*.rs\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"Self-release test\"}}}" \
)"
e2e_save_artifact "case_09_self_reserve.txt" "$SELF_RES_RESP"

SELF_RES_TEXT="$(extract_result "$SELF_RES_RESP" 95)"
SELF_RES_ID="$(python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    if granted:
        print(granted[0].get('id', ''))
except Exception:
    print('')
" <<< "$SELF_RES_TEXT" 2>/dev/null)"

if [ -n "$SELF_RES_ID" ] && [ "$SELF_RES_ID" != "" ]; then
    # Seed an actually expired private lease; expiry authorizes release even
    # while this holder is active. This does not claim inactivity authorization.
    SELF_SEEDED="$(sqlite3 "$FR_DB" "UPDATE file_reservations SET created_ts=946684800000000, expires_ts=946684860000000 WHERE id=${SELF_RES_ID}; SELECT changes();")"
    e2e_assert_eq "one private own lease expired" "1" "$SELF_SEEDED"
    SELF_FORCE_RESP="$(send_jsonrpc_session "$FR_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${ACTIVE_AGENT}\",\"file_reservation_id\":${SELF_RES_ID},\"note\":\"Self force-release\"}}}" \
    )"
    e2e_save_artifact "case_09_self_force.txt" "$SELF_FORCE_RESP"

    SELF_FORCE_ERR="$(is_error_result "$SELF_FORCE_RESP" 100)"

    if [ "$SELF_FORCE_ERR" = "false" ]; then
        e2e_pass "force-release as reservation holder succeeded"
        e2e_assert_eq "expired own lease releases one row" "1" \
            "$(parse_json_field "$(extract_result "$SELF_FORCE_RESP" 100)" "released")"
    else
        e2e_fail "force-release must release the holder's expired lease"
    fi
else
    e2e_fail "Self-release reservation ID not available"
fi

# ===========================================================================
# Case 10: Force-release with detailed note field
# ===========================================================================
e2e_case_banner "Force-release with detailed note field"

# Create another reservation
NOTE_RES_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":105,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${NOTE_AGENT}\",\"paths\":[\"note_test/*.rs\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"Note test\"}}}" \
)"
e2e_save_artifact "case_10_note_reserve.txt" "$NOTE_RES_RESP"

NOTE_RES_TEXT="$(extract_result "$NOTE_RES_RESP" 105)"
NOTE_RES_ID="$(python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    granted = result.get('granted', [])
    if granted:
        print(granted[0].get('id', ''))
except Exception:
    print('')
" <<< "$NOTE_RES_TEXT" 2>/dev/null)"

if [ -n "$NOTE_RES_ID" ] && [ "$NOTE_RES_ID" != "" ]; then
    seed_inactive_holder "$NOTE_AGENT"
    DETAILED_NOTE="Agent ${NOTE_AGENT} has been unresponsive for 2 hours. Last seen: 2026-02-12T03:00:00Z. No git commits. No mail activity. Force-releasing to unblock br-1234."

    # URL-encode the note for JSON
    ENCODED_NOTE="$(python3 -c "import json; print(json.dumps('$DETAILED_NOTE'))" 2>/dev/null | tr -d '"')"

    NOTE_FORCE_RESP="$(send_jsonrpc_session "$FR_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":110,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${RELEASER_AGENT}\",\"file_reservation_id\":${NOTE_RES_ID},\"note\":\"${ENCODED_NOTE}\",\"notify_previous\":true}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":111,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"${NOTE_AGENT}\",\"include_bodies\":true}}}" \
    )"
    e2e_save_artifact "case_10_note_force.txt" "$NOTE_FORCE_RESP"

    NOTE_FORCE_ERR="$(is_error_result "$NOTE_FORCE_RESP" 110)"

    if [ "$NOTE_FORCE_ERR" = "false" ]; then
        e2e_pass "force-release with detailed note succeeded"
        e2e_assert_eq "detailed-note force release releases one row" "1" \
            "$(parse_json_field "$(extract_result "$NOTE_FORCE_RESP" 110)" "released")"
        assert_ok "detailed-note holder inbox is readable" "$NOTE_FORCE_RESP" 111
        e2e_assert_contains "notification preserves the detailed note" \
            "$(extract_result "$NOTE_FORCE_RESP" 111)" "$DETAILED_NOTE"
    else
        e2e_fail "detailed-note force release must release its inactive holder's lease"
    fi
else
    e2e_fail "Note test reservation ID not available"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
