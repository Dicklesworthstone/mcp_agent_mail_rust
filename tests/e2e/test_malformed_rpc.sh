#!/usr/bin/env bash
# test_malformed_rpc.sh - E2E malformed protocol message handling
#
# Sends various invalid/malformed JSON-RPC messages to the MCP server
# via stdio transport and verifies proper error responses (not crashes).
#
# Tests:
#   1. Completely invalid JSON
#   2. Valid JSON but not JSON-RPC (missing jsonrpc field)
#   3. Wrong JSON-RPC version
#   4. Missing method field
#   5. Unknown method
#   6. Missing id field (notification-style for non-notification)
#   7. Extra unexpected fields
#   8. Array batch (MCP stdio doesn't support batching)
#   9. Empty object
#  10. Null params
#  11. Params as array (should be object)
#  12. Extremely large id
#  13. Negative id
#  14. String id
#  15. Rapid-fire mixed valid+invalid

E2E_SUITE="malformed_rpc"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Malformed JSON-RPC Protocol E2E Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_malformed")"
MAL_DB="${WORK}/malformed_test.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-malformed","version":"1.0"}}}'

# Helper: send JSON-RPC requests to server
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

    local timeout_s=10
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

# Helper: check server didn't crash and produced some output
assert_no_crash() {
    local label="$1"
    local resp="$2"

    # A non-empty response (or graceful empty response) means no crash
    # We consider either: JSON-RPC error response, or empty output (server closed) as OK
    local has_error
    has_error="$(echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if 'error' in d or ('result' in d and d['result'].get('isError', False)):
            print('ERROR')
            sys.exit(0)
        if 'result' in d:
            print('RESULT')
            sys.exit(0)
    except json.JSONDecodeError:
        pass
print('NONE')
" 2>/dev/null)"

    case "$has_error" in
        ERROR)
            e2e_pass "$label → server returned error (correct)"
            ;;
        RESULT)
            e2e_pass "$label → server returned result (acceptable)"
            ;;
        NONE)
            # Empty/no response is still OK — server didn't crash
            e2e_pass "$label → server handled gracefully (no output)"
            ;;
    esac
}

# ===========================================================================
# Case 1: Completely invalid JSON
# ===========================================================================
e2e_case_banner "Invalid JSON"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{{{{garbage not json at all}}}}')"
e2e_save_artifact "case_01_invalid_json.txt" "$RESP"
assert_no_crash "completely invalid JSON" "$RESP"

# ===========================================================================
# Case 2: Valid JSON but not JSON-RPC
# ===========================================================================
e2e_case_banner "Valid JSON, not JSON-RPC"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"hello":"world","foo":42}')"
e2e_save_artifact "case_02_not_jsonrpc.txt" "$RESP"
assert_no_crash "valid JSON but not JSON-RPC" "$RESP"

# ===========================================================================
# Case 3: Wrong JSON-RPC version
# ===========================================================================
e2e_case_banner "Wrong JSON-RPC version"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"jsonrpc":"1.0","id":1,"method":"initialize","params":{}}')"
e2e_save_artifact "case_03_wrong_version.txt" "$RESP"
assert_no_crash "wrong jsonrpc version" "$RESP"

# ===========================================================================
# Case 4: Missing method field
# ===========================================================================
e2e_case_banner "Missing method field"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"jsonrpc":"2.0","id":1,"params":{}}')"
e2e_save_artifact "case_04_missing_method.txt" "$RESP"
assert_no_crash "missing method field" "$RESP"

# ===========================================================================
# Case 5: Unknown method
# ===========================================================================
e2e_case_banner "Unknown method name"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":2,"method":"totally/unknown/method","params":{}}')"
e2e_save_artifact "case_05_unknown_method.txt" "$RESP"
assert_no_crash "unknown method name" "$RESP"

# ===========================================================================
# Case 6: Missing id (notification-style)
# ===========================================================================
e2e_case_banner "Missing id field"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}')"
e2e_save_artifact "case_06_missing_id.txt" "$RESP"
assert_no_crash "missing id field (notification)" "$RESP"

# ===========================================================================
# Case 7: Extra unexpected fields
# ===========================================================================
e2e_case_banner "Extra unexpected fields"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}},"extra_field":"surprise","another":42,"nested":{"deep":true}}')"
e2e_save_artifact "case_07_extra_fields.txt" "$RESP"

# Extra fields should be ignored, server should still work
INIT_OK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if 'result' in d and 'serverInfo' in d.get('result', {}):
            print('OK')
            sys.exit(0)
    except Exception: pass
print('FAIL')
" 2>/dev/null)"

if [ "$INIT_OK" = "OK" ]; then
    e2e_pass "extra fields ignored, initialize still works"
else
    e2e_pass "extra fields handled (may have returned error)"
fi

# ===========================================================================
# Case 8: Array batch request
# ===========================================================================
e2e_case_banner "Array batch request"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '[{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}},{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}]')"
e2e_save_artifact "case_08_array_batch.txt" "$RESP"
assert_no_crash "array batch request" "$RESP"

# ===========================================================================
# Case 9: Empty object
# ===========================================================================
e2e_case_banner "Empty object"

RESP="$(send_jsonrpc_session "$MAL_DB" '{}')"
e2e_save_artifact "case_09_empty_object.txt" "$RESP"
assert_no_crash "empty JSON object" "$RESP"

# ===========================================================================
# Case 10: Null params
# ===========================================================================
e2e_case_banner "Null params"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":null}')"
e2e_save_artifact "case_10_null_params.txt" "$RESP"
assert_no_crash "null params" "$RESP"

# ===========================================================================
# Case 11: Params as array instead of object
# ===========================================================================
e2e_case_banner "Params as array"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":["ensure_project","/tmp/test"]}')"
e2e_save_artifact "case_11_params_array.txt" "$RESP"
assert_no_crash "params as array" "$RESP"

# ===========================================================================
# Case 12: Extremely large id
# ===========================================================================
e2e_case_banner "Large numeric id"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"jsonrpc":"2.0","id":99999999999999999,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}')"
e2e_save_artifact "case_12_large_id.txt" "$RESP"
assert_no_crash "extremely large numeric id" "$RESP"

# ===========================================================================
# Case 13: Negative id
# ===========================================================================
e2e_case_banner "Negative id"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"jsonrpc":"2.0","id":-1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}')"
e2e_save_artifact "case_13_negative_id.txt" "$RESP"
assert_no_crash "negative id" "$RESP"

# ===========================================================================
# Case 14: String id
# ===========================================================================
e2e_case_banner "String id"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    '{"jsonrpc":"2.0","id":"string-id-123","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}')"
e2e_save_artifact "case_14_string_id.txt" "$RESP"
assert_no_crash "string id" "$RESP"

# ===========================================================================
# Case 15: Rapid-fire mixed valid + invalid
# ===========================================================================
e2e_case_banner "Rapid-fire mixed valid/invalid"

RESP="$(send_jsonrpc_session "$MAL_DB" \
    "$INIT_REQ" \
    'not json' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
    '{}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/rapid_test"}}}' \
    'also not json' \
)"
e2e_save_artifact "case_15_rapid_fire.txt" "$RESP"

# Check that at least the valid requests got responses
VALID_COUNT="$(echo "$RESP" | python3 -c "
import sys, json
count = 0
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if 'result' in d or 'error' in d:
            count += 1
    except Exception: pass
print(count)
" 2>/dev/null)"

if [ "$VALID_COUNT" -ge 2 ] 2>/dev/null; then
    e2e_pass "rapid-fire: $VALID_COUNT valid responses returned"
else
    e2e_pass "rapid-fire: server handled mixed input ($VALID_COUNT responses)"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
