#!/usr/bin/env bash
# test_wrong_order_misuse.sh - E2E: Common Pitfalls from AGENTS.md
#
# Verifies that the server properly handles common misuse patterns --
# things that agents/users might try that are wrong or out of order.
# The test confirms proper error responses (not crashes).
#
# Tests:
#   1.  Send message before registering agent -- expect error
#   2.  Register agent before ensure_project -- auto-ensures project (succeeds)
#   3.  Reply to nonexistent message -- expect error
#   4.  Double register same agent -- should succeed (upsert behavior)
#   5.  Send message to an invalid name and an unregistered recipient
#   6.  Acknowledge nonexistent message -- expect error
#   7.  File reservation before project exists -- expect error
#   8.  Release nonexistent reservation -- should succeed (idempotent)
#   9.  Fetch inbox for unregistered agent -- expect error
#   10. Search in nonexistent project -- empty result or project not found
#   11. Initialize twice -- should handle gracefully

E2E_SUITE="wrong_order_misuse"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Wrong Order / Misuse E2E Test Suite"

AM_BIN="$(e2e_ensure_binary am | tail -n 1)"
e2e_log "am binary: ${AM_BIN}"

# Temp workspace
WORK="$(e2e_mktemp "e2e_wrong_order")"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-wrong-order","version":"1.0"}}}'

# Unique project paths per test group
PROJECT_PATH_A="/tmp/e2e_wrong_order_a_$$"
PROJECT_PATH_B="/tmp/e2e_wrong_order_b_$$"
NONEXISTENT_PROJECT="/tmp/e2e_nonexistent_project_$$"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"; shift
    local archive="${db_path%.sqlite3}.archive"
    mkdir -p "$archive"
    # The original polling loop waited 25 half-second ticks. Keep the
    # response deadline and five-second EOF drain inside that budget.
    E2E_STDIO_BINARY="$AM_BIN" E2E_STDIO_TIMEOUT_SECONDS=7.5 STORAGE_ROOT="$archive" \
        SEARCH_V3_INDEX_DIR="$archive/search_index" AM_ALLOW_EPHEMERAL_PROJECT_ROOTS=1 \
        RUST_LOG=error e2e_stdio_session "$db_path" "$@"
}

read_reply() {
    printf '%s\n' "$1" | python3 -c '
import json
import sys

request_id, kind = int(sys.argv[1]), sys.argv[2]
matching = [frame for frame in map(json.loads, sys.stdin) if frame.get("id") == request_id]
if len(matching) != 1:
    sys.exit(f"Expected one reply for request {request_id}, got {len(matching)}")
reply, = matching
if kind == "rpc":
    print(json.dumps(reply))
    sys.exit(0)
result = reply.get("result")
if "error" in reply or not isinstance(result, dict):
    sys.exit(f"Expected tool payload for request {request_id}, got a protocol error or invalid result")
is_error = result.get("isError", False)
if not isinstance(is_error, bool) or (kind != "tool" and is_error != (kind == "tool-error")):
    sys.exit(f"Unexpected tool status for request {request_id}")
payload = json.loads(result["content"][0]["text"])
if is_error and (not isinstance(payload, dict) or not isinstance(payload.get("error"), dict)):
    sys.exit(f"Missing structured tool error for request {request_id}")
print(json.dumps({"isError": is_error, "payload": payload} if kind == "tool" else payload))
' "$2" "$3"
}

parse_json_field() {
    printf '%s\n' "$1" | python3 -c '
import json
import sys

value = json.load(sys.stdin)
for field in sys.argv[1].split("."):
    value = value[int(field)] if isinstance(value, list) else value[field]
print(value)
' "$2"
}

assert_tool_error() {
    local response="$1" request_id="$2" error_type="$3" field="$4" expected="$5" label="$6"
    local payload actual_type actual_value
    if payload="$(read_reply "$response" "$request_id" tool-error)" &&
        actual_type="$(parse_json_field "$payload" error.type)" &&
        actual_value="$(parse_json_field "$payload" "error.data.$field")" &&
        [ "$actual_type" = "$error_type" ] && [ "$actual_value" = "$expected" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label (expected ${error_type} with ${field}=${expected})"
    fi
}

# ===========================================================================
# Case 1: Send message before registering agent -- expect error
# ===========================================================================
e2e_case_banner "send_message before register_agent"

DB_1="${WORK}/case01.sqlite3"

RESP_1="$(send_jsonrpc_session "$DB_1" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Hello\",\"body_md\":\"Test message\"}}}" \
)"
e2e_save_artifact "case_01_send_before_register.txt" "$RESP_1"

EP_1="$(read_reply "$RESP_1" 10 tool-ok)"
e2e_assert_eq "setup created the requested project" "$PROJECT_PATH_A" "$(parse_json_field "$EP_1" human_key)"
assert_tool_error "$RESP_1" 11 NOT_FOUND agent_name GoldFox "send_message identifies the unregistered sender"

# ===========================================================================
# Case 2: Register agent before ensure_project -- auto-ensures project
# ===========================================================================
e2e_case_banner "register_agent before ensure_project (auto-ensure)"

DB_2="${WORK}/case02.sqlite3"

RESP_2="$(send_jsonrpc_session "$DB_2" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${NONEXISTENT_PROJECT}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"testing\"}}}" \
)"
e2e_save_artifact "case_02_register_before_project.txt" "$RESP_2"

REG_2="$(read_reply "$RESP_2" 20 tool-ok)"
e2e_assert_eq "auto-registered agent has correct name" "GoldFox" "$(parse_json_field "$REG_2" name)"
if [ "$(parse_json_field "$REG_2" project_id)" -gt 0 ]; then
    e2e_pass "register_agent allocated a project"
else
    e2e_fail "register_agent did not allocate a project"
fi

# ===========================================================================
# Case 3: Reply to nonexistent message -- expect error
# ===========================================================================
e2e_case_banner "reply_message to nonexistent message_id"

DB_3="${WORK}/case03.sqlite3"

RESP_3="$(send_jsonrpc_session "$DB_3" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"message_id\":99999,\"sender_name\":\"GoldFox\",\"body_md\":\"Reply to nothing\"}}}" \
)"
e2e_save_artifact "case_03_reply_nonexistent.txt" "$RESP_3"

read_reply "$RESP_3" 30 tool-ok >/dev/null
read_reply "$RESP_3" 31 tool-ok >/dev/null
assert_tool_error "$RESP_3" 32 NOT_FOUND identifier 99999 "reply_message identifies the missing message"

# ===========================================================================
# Case 4: Double register same agent -- should succeed (upsert)
# ===========================================================================
e2e_case_banner "double register_agent (upsert)"

DB_4="${WORK}/case04.sqlite3"

RESP_4="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_B}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"program\":\"e2e-test\",\"model\":\"test-model-v1\",\"name\":\"GoldFox\",\"task_description\":\"first registration\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"program\":\"e2e-test\",\"model\":\"test-model-v2\",\"name\":\"GoldFox\",\"task_description\":\"second registration\"}}}" \
)"
e2e_save_artifact "case_04_double_register.txt" "$RESP_4"

read_reply "$RESP_4" 40 tool-ok >/dev/null
REG1_4="$(read_reply "$RESP_4" 41 tool-ok)"
REG2_4="$(read_reply "$RESP_4" 42 tool-ok)"
AGENT_ID_4="$(parse_json_field "$REG1_4" id)"
e2e_assert_eq "first registration used the original model" "test-model-v1" "$(parse_json_field "$REG1_4" model)"
e2e_assert_eq "upsert reused the original agent ID" "$AGENT_ID_4" "$(parse_json_field "$REG2_4" id)"
e2e_assert_eq "upsert persisted the new model" "test-model-v2" "$(parse_json_field "$REG2_4" model)"
e2e_assert_eq "upsert preserved agent name" "GoldFox" "$(parse_json_field "$REG2_4" name)"

# ===========================================================================
# Case 5: Invalid name and unregistered recipient are distinct errors
# ===========================================================================
e2e_case_banner "send_message to nonexistent recipient"

# Reuse DB_4 with GoldFox registered. Default auto-registration validates
# the name before creating a recipient. The strict session refuses a valid
# unregistered name without creating an identity or delivering mail.
RESP_5="$(MESSAGING_AUTO_REGISTER_RECIPIENTS=1 send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"sender_name\":\"GoldFox\",\"to\":[\"NonExistentAgent123\"],\"subject\":\"Hello ghost\",\"body_md\":\"You do not exist\"}}}" \
)"
e2e_save_artifact "case_05_nonexistent_recipient.txt" "$RESP_5"
assert_tool_error "$RESP_5" 50 INVALID_ARGUMENT field name "send_message rejects the malformed recipient name"

STRICT_5="$(MESSAGING_AUTO_REGISTER_RECIPIENTS=0 send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"sender_name\":\"GoldFox\",\"to\":[\"BlueLake\"],\"subject\":\"Hello ghost\",\"body_md\":\"You do not exist\"}}}" \
)"
e2e_save_artifact "case_05_strict_recipient.txt" "$STRICT_5"
assert_tool_error "$STRICT_5" 51 RECIPIENT_NOT_FOUND unknown_local.0 BlueLake "send_message identifies the valid but unregistered recipient"

# ===========================================================================
# Case 6: Acknowledge nonexistent message -- expect error
# ===========================================================================
e2e_case_banner "acknowledge_message with bad message_id"

# Reuse DB_4 which has project + GoldFox registered
RESP_6="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"agent_name\":\"GoldFox\",\"message_id\":99999}}}" \
)"
e2e_save_artifact "case_06_ack_nonexistent.txt" "$RESP_6"

assert_tool_error "$RESP_6" 60 NOT_FOUND identifier "${AGENT_ID_4}:99999" "acknowledge_message identifies the missing recipient record"

# ===========================================================================
# Case 7: File reservation before project exists -- expect error
# ===========================================================================
e2e_case_banner "file_reservation_paths before ensure_project"

DB_7="${WORK}/case07.sqlite3"

RESP_7="$(send_jsonrpc_session "$DB_7" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${NONEXISTENT_PROJECT}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/*.rs\"],\"ttl_seconds\":3600,\"exclusive\":true}}}" \
)"
e2e_save_artifact "case_07_reservation_no_project.txt" "$RESP_7"

assert_tool_error "$RESP_7" 70 NOT_FOUND agent_name GoldFox "file_reservation_paths refuses the unregistered claimant in a new project"

# ===========================================================================
# Case 8: Release nonexistent reservation -- should succeed (idempotent)
# ===========================================================================
e2e_case_banner "release_file_reservations (idempotent, nothing to release)"

# Reuse DB_4 which has project + GoldFox registered
RESP_8="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"agent_name\":\"GoldFox\"}}}" \
)"
e2e_save_artifact "case_08_release_nonexistent.txt" "$RESP_8"

REL_8="$(read_reply "$RESP_8" 80 tool-ok)"
e2e_assert_eq "release without reservations succeeded with count zero" "0" "$(parse_json_field "$REL_8" released)"
e2e_assert_eq "release returned no reservation IDs" "[]" "$(parse_json_field "$REL_8" released_ids)"

# ===========================================================================
# Case 9: Fetch inbox for unregistered agent -- expect error
# ===========================================================================
e2e_case_banner "fetch_inbox for unregistered agent"

# Reuse DB_4 which has project but only GoldFox registered
RESP_9="$(send_jsonrpc_session "$DB_4" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"agent_name\":\"PurpleDragon\"}}}" \
)"
e2e_save_artifact "case_09_inbox_unregistered.txt" "$RESP_9"

assert_tool_error "$RESP_9" 90 NOT_FOUND agent_name PurpleDragon "fetch_inbox identifies the unregistered agent"

# ===========================================================================
# Case 10: Search in nonexistent project -- empty result or project not found
# ===========================================================================
e2e_case_banner "search_messages in nonexistent project"

DB_10="${WORK}/case10.sqlite3"

RESP_10="$(send_jsonrpc_session "$DB_10" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${NONEXISTENT_PROJECT}\",\"query\":\"hello\"}}}" \
)"
e2e_save_artifact "case_10_search_no_project.txt" "$RESP_10"

SEARCH_10="$(read_reply "$RESP_10" 100 tool)"
if [ "$(parse_json_field "$SEARCH_10" isError)" = "True" ]; then
    assert_tool_error "$RESP_10" 100 NOT_FOUND identifier "$NONEXISTENT_PROJECT" "search reports the missing project"
else
    SEARCH_ROWS_10="$(printf '%s\n' "$SEARCH_10" | python3 -c '
import json
import sys

payload = json.load(sys.stdin)["payload"]
rows = payload.get("result") if isinstance(payload, dict) else payload
print(json.dumps(rows))
')"
    e2e_assert_eq "search returned an empty result list for the missing project" "[]" "$SEARCH_ROWS_10"
fi

# ===========================================================================
# Case 11: Initialize twice -- should handle gracefully
# ===========================================================================
e2e_case_banner "double initialize request"

DB_11="${WORK}/case11.sqlite3"

INIT_REQ_2='{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-wrong-order-2nd","version":"1.0"}}}'

RESP_11="$(send_jsonrpc_session "$DB_11" \
    "$INIT_REQ" \
    "$INIT_REQ_2" \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    "{\"jsonrpc\":\"2.0\",\"id\":110,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}" \
)"
e2e_save_artifact "case_11_double_init.txt" "$RESP_11"

INIT_11="$(read_reply "$RESP_11" 1 rpc)"
e2e_assert_eq "first initialize negotiated the protocol" "2024-11-05" "$(parse_json_field "$INIT_11" result.protocolVersion)"
DUPLICATE_11="$(read_reply "$RESP_11" 2 rpc)"
e2e_assert_eq "duplicate before readiness got the handshake error code" "-32600" "$(parse_json_field "$DUPLICATE_11" error.code)"
e2e_assert_contains "duplicate before readiness was refused" "$(parse_json_field "$DUPLICATE_11" error.message)" "notifications/initialized"
EP_11="$(read_reply "$RESP_11" 110 tool-ok)"
e2e_assert_eq "ensure_project succeeds after readiness notification" "$PROJECT_PATH_A" "$(parse_json_field "$EP_11" human_key)"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
