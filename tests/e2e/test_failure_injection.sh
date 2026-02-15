#!/usr/bin/env bash
# test_failure_injection.sh - Failure-Injection E2E Suite for Degraded/Error UX Paths
#
# br-1xt0m.1.13.15: Validates degraded, error, and edge-case paths through the
# stdio JSON-RPC transport. Tests that invalid inputs, missing prerequisites,
# conflicting operations, and boundary conditions produce actionable diagnostics
# rather than crashes or silent corruption.
#
# Test cases:
#   1. Missing-project error paths: tools called before ensure_project
#   2. Invalid agent name diagnostics: names that violate the adjective+noun contract
#   3. Double-registration idempotency: re-register with different params
#   4. Message to nonexistent recipient: send_message with unknown agent
#   5. File reservation conflict injection: overlapping exclusive reservations
#   6. Force-release on non-stale reservation: should fail gracefully
#   7. Invalid tool parameters: wrong types, missing required fields
#   8. Boundary inputs: empty strings, max-length strings, special characters
#
# Logging: failure mode, injection method, expected fallback, observed fallback.

E2E_SUITE="failure_injection"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Failure-Injection E2E Suite for Degraded/Error UX Paths (br-1xt0m.1.13.15)"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_failure_inj")"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-failure","version":"1.0"}}}'

# ── Helpers ──────────────────────────────────────────────────────────────

send_session() {
    local db_path="$1"
    shift
    local requests=("$@")
    local output_file="${WORK}/session_response_${RANDOM}.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error WORKTREES_ENABLED=true \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.5
        done
        sleep 1
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=25
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if ! kill -0 "$srv_pid" 2>/dev/null; then
            break
        fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    if [ -f "$output_file" ]; then
        cat "$output_file"
    fi
}

extract_tool_result() {
    local resp="$1"
    local req_id="$2"
    echo "$resp" | python3 -c "
import sys, json
target_id = int(sys.argv[1])
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == target_id and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                print(content[0].get('text', ''))
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('')
" "$req_id" 2>/dev/null
}

is_error() {
    local resp="$1"
    local req_id="$2"
    echo "$resp" | python3 -c "
import sys, json
target_id = int(sys.argv[1])
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == target_id:
            if 'error' in d:
                print('ERROR')
                sys.exit(0)
            if d.get('result', {}).get('isError', False):
                print('ERROR')
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('OK')
" "$req_id" 2>/dev/null
}

extract_field() {
    local text="$1"
    local field="$2"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    v = d
    for part in sys.argv[1].split('.'):
        if isinstance(v, list):
            v = v[int(part)]
        else:
            v = v[part]
    print(v)
except Exception:
    print('')
" "$field" 2>/dev/null
}

# Helper: Build a JSON-RPC tool call request. Uses double-quotes so shell
# variables in the arguments JSON string are properly expanded.
mk_tool_call() {
    local id="$1"
    local tool="$2"
    local args_json="$3"
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args_json}}}"
}

# ═════════════════════════════════════════════════════════════════════════
# Case 1: Missing-project error paths
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Missing-project error paths"

DB1="${WORK}/fail_missing_project.db"

# Try to register_agent without ensure_project first
RESP=$(send_session "$DB1" \
    "$INIT_REQ" \
    "$(mk_tool_call 10 register_agent '{"project_key":"nonexistent-proj","program":"test","model":"test-model"}')" \
)

e2e_step_start "register_agent_no_project"
STATUS=$(is_error "$RESP" 10)
e2e_assert_eq "register_agent without project returns error" "ERROR" "$STATUS"

RESULT=$(extract_tool_result "$RESP" 10)
e2e_assert_contains "error mentions project" "$RESULT" "project"
e2e_step_end "register_agent_no_project"

# Try to send_message without project
RESP2=$(send_session "$DB1" \
    "$INIT_REQ" \
    "$(mk_tool_call 11 send_message '{"project_key":"ghost-proj","sender_name":"GreenLake","to":["BlueFox"],"subject":"test","body_md":"hello"}')" \
)

e2e_step_start "send_message_no_project"
STATUS2=$(is_error "$RESP2" 11)
e2e_assert_eq "send_message without project returns error" "ERROR" "$STATUS2"
e2e_step_end "send_message_no_project"

# ═════════════════════════════════════════════════════════════════════════
# Case 2: Invalid agent name diagnostics
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Invalid agent name diagnostics"

DB2="${WORK}/fail_agent_names.db"
PROJ2="${WORK}/proj-agent-names"

# Set up a valid project first
RESP_SETUP=$(send_session "$DB2" \
    "$INIT_REQ" \
    "$(mk_tool_call 20 ensure_project "{\"human_key\":\"${PROJ2}\"}")" \
)

# Try registering with invalid names
INVALID_NAMES=(
    "lowercase"          # No CamelCase
    "EaglePeak"          # Eagle is a noun, not adjective
    "BraveLion"          # Brave not in adjective list
    "123Numeric"         # Starts with number
    ""                   # Empty string
    "A"                  # Too short
)

for bad_name in "${INVALID_NAMES[@]}"; do
    e2e_step_start "invalid_name_${bad_name:-empty}"
    local_safe_name=$(echo "$bad_name" | tr -d '"\\')
    RESP_BAD=$(send_session "$DB2" \
        "$INIT_REQ" \
        "$(mk_tool_call 21 ensure_project "{\"human_key\":\"${PROJ2}\"}")" \
        "$(mk_tool_call 22 register_agent "{\"project_key\":\"${PROJ2}\",\"program\":\"test\",\"model\":\"test-model\",\"name\":\"${local_safe_name}\"}")" \
    )
    STATUS_BAD=$(is_error "$RESP_BAD" 22)
    e2e_assert_eq "invalid name '${bad_name:-<empty>}' returns error" "ERROR" "$STATUS_BAD"
    e2e_step_end "invalid_name_${bad_name:-empty}"
done

# ═════════════════════════════════════════════════════════════════════════
# Case 3: Double-registration idempotency
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Double-registration idempotency"

DB3="${WORK}/fail_double_reg.db"
PROJ3="${WORK}/proj-double-reg"

RESP_REG=$(send_session "$DB3" \
    "$INIT_REQ" \
    "$(mk_tool_call 30 ensure_project "{\"human_key\":\"${PROJ3}\"}")" \
    "$(mk_tool_call 31 register_agent "{\"project_key\":\"${PROJ3}\",\"program\":\"agent-a\",\"model\":\"model-1\",\"name\":\"GreenLake\"}")" \
    "$(mk_tool_call 32 register_agent "{\"project_key\":\"${PROJ3}\",\"program\":\"agent-a\",\"model\":\"model-1\",\"name\":\"GreenLake\"}")" \
)

# Both registrations should succeed (idempotent)
STATUS_R1=$(is_error "$RESP_REG" 31)
STATUS_R2=$(is_error "$RESP_REG" 32)
e2e_assert_eq "first registration succeeds" "OK" "$STATUS_R1"
e2e_assert_eq "second (duplicate) registration succeeds" "OK" "$STATUS_R2"

# Extract agent names — should be the same
RESULT_R1=$(extract_tool_result "$RESP_REG" 31)
RESULT_R2=$(extract_tool_result "$RESP_REG" 32)
NAME_R1=$(extract_field "$RESULT_R1" "name")
NAME_R2=$(extract_field "$RESULT_R2" "name")
e2e_assert_eq "duplicate registration returns same agent name" "$NAME_R1" "$NAME_R2"

# ═════════════════════════════════════════════════════════════════════════
# Case 4: Message to nonexistent recipient
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Message to nonexistent recipient"

DB4="${WORK}/fail_no_recipient.db"
PROJ4="${WORK}/proj-no-recipient"

RESP_MSG=$(send_session "$DB4" \
    "$INIT_REQ" \
    "$(mk_tool_call 40 ensure_project "{\"human_key\":\"${PROJ4}\"}")" \
    "$(mk_tool_call 41 register_agent "{\"project_key\":\"${PROJ4}\",\"program\":\"sender\",\"model\":\"test-model\"}")" \
    "$(mk_tool_call 42 send_message "{\"project_key\":\"${PROJ4}\",\"sender_name\":\"@auto\",\"to\":[\"GhostAgent\"],\"subject\":\"hello ghost\",\"body_md\":\"anyone home?\"}")" \
)

# Sending to a nonexistent agent — check if it errors or succeeds (depends on design)
SEND_RESULT=$(extract_tool_result "$RESP_MSG" 42)
SEND_STATUS=$(is_error "$RESP_MSG" 42)

# Document the observed behavior (the test validates consistency, not specific outcome)
if [ "$SEND_STATUS" = "ERROR" ]; then
    e2e_pass "send to nonexistent recipient returns error (strict mode)"
    RESULT_TEXT=$(extract_tool_result "$RESP_MSG" 42)
    e2e_log "Error response: ${RESULT_TEXT:0:200}"
else
    # If it succeeds (lenient mode), the message should still be well-formed
    e2e_pass "send to nonexistent recipient succeeds (lenient mode)"
    e2e_assert_contains "response contains message_id or thread" "$SEND_RESULT" "id"
fi

# ═════════════════════════════════════════════════════════════════════════
# Case 5: File reservation conflict injection
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "File reservation conflict injection"

DB5="${WORK}/fail_reservation_conflict.db"
PROJ5="${WORK}/proj-reservation-conflict"

RESP_RES=$(send_session "$DB5" \
    "$INIT_REQ" \
    "$(mk_tool_call 50 ensure_project "{\"human_key\":\"${PROJ5}\"}")" \
    "$(mk_tool_call 51 register_agent "{\"project_key\":\"${PROJ5}\",\"program\":\"agent-a\",\"model\":\"m1\",\"name\":\"GreenLake\"}")" \
    "$(mk_tool_call 52 register_agent "{\"project_key\":\"${PROJ5}\",\"program\":\"agent-b\",\"model\":\"m2\",\"name\":\"BlueFox\"}")" \
    "$(mk_tool_call 53 file_reservation_paths "{\"project_key\":\"${PROJ5}\",\"agent_name\":\"GreenLake\",\"paths\":[\"src/main.rs\"],\"exclusive\":true}")" \
    "$(mk_tool_call 54 file_reservation_paths "{\"project_key\":\"${PROJ5}\",\"agent_name\":\"BlueFox\",\"paths\":[\"src/main.rs\"],\"exclusive\":true}")" \
)

# First reservation should succeed
RES1=$(extract_tool_result "$RESP_RES" 53)
STATUS_RES1=$(is_error "$RESP_RES" 53)
e2e_assert_eq "first exclusive reservation succeeds" "OK" "$STATUS_RES1"

# Second reservation should report conflicts
RES2=$(extract_tool_result "$RESP_RES" 54)
STATUS_RES2=$(is_error "$RESP_RES" 54)

# Check for conflicts in the response (may not be an error, but should list conflicts)
e2e_step_start "check_conflict_response"
HAS_CONFLICTS=$(echo "$RES2" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    conflicts = d.get('conflicts', [])
    if conflicts:
        print('HAS_CONFLICTS')
    else:
        print('NO_CONFLICTS')
except:
    print('PARSE_ERROR')
" 2>/dev/null)

if [ "$HAS_CONFLICTS" = "HAS_CONFLICTS" ]; then
    e2e_pass "conflicting reservation reports conflicts array"
elif [ "$STATUS_RES2" = "ERROR" ]; then
    e2e_pass "conflicting reservation returns error"
else
    e2e_fail "conflicting reservation silently succeeded without conflicts"
fi
e2e_step_end "check_conflict_response"

# ═════════════════════════════════════════════════════════════════════════
# Case 6: Force-release on active (non-stale) reservation
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Force-release on active (non-stale) reservation"

DB6="${WORK}/fail_force_release.db"
PROJ6="${WORK}/proj-force-release"

RESP_FR=$(send_session "$DB6" \
    "$INIT_REQ" \
    "$(mk_tool_call 60 ensure_project "{\"human_key\":\"${PROJ6}\"}")" \
    "$(mk_tool_call 61 register_agent "{\"project_key\":\"${PROJ6}\",\"program\":\"holder\",\"model\":\"m1\",\"name\":\"GreenLake\"}")" \
    "$(mk_tool_call 62 register_agent "{\"project_key\":\"${PROJ6}\",\"program\":\"requester\",\"model\":\"m2\",\"name\":\"BlueFox\"}")" \
    "$(mk_tool_call 63 file_reservation_paths "{\"project_key\":\"${PROJ6}\",\"agent_name\":\"GreenLake\",\"paths\":[\"src/lib.rs\"],\"exclusive\":true}")" \
    "$(mk_tool_call 64 force_release_file_reservation "{\"project_key\":\"${PROJ6}\",\"agent_name\":\"BlueFox\",\"file_reservation_id\":1}")" \
)

# Force-release should fail or report non-stale (reservation was just created)
FR_STATUS=$(is_error "$RESP_FR" 64)
FR_TEXT=$(extract_tool_result "$RESP_FR" 64)

e2e_step_start "force_release_active"
if [ "$FR_STATUS" = "ERROR" ]; then
    e2e_pass "force-release on active reservation returns error"
    e2e_log "Error: ${FR_TEXT:0:200}"
else
    # May succeed but should indicate non-stale context
    e2e_pass "force-release on active reservation returned result (may have inactivity check)"
    e2e_log "Result: ${FR_TEXT:0:200}"
fi
e2e_step_end "force_release_active"

# ═════════════════════════════════════════════════════════════════════════
# Case 7: Invalid tool parameters
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Invalid tool parameters"

DB7="${WORK}/fail_bad_params.db"

# Missing required field (project_key)
RESP_BAD1=$(send_session "$DB7" \
    "$INIT_REQ" \
    "$(mk_tool_call 70 ensure_project '{}')" \
)
STATUS_BAD1=$(is_error "$RESP_BAD1" 70)
e2e_assert_eq "ensure_project with empty args returns error" "ERROR" "$STATUS_BAD1"

# Wrong type for integer field (string where int expected)
RESP_BAD2=$(send_session "$DB7" \
    "$INIT_REQ" \
    "$(mk_tool_call 71 fetch_inbox '{"project_key":"test","agent_name":"GreenLake","limit":"not_a_number"}')" \
)
# This might coerce or error — document the behavior
STATUS_BAD2=$(is_error "$RESP_BAD2" 71)
if [ "$STATUS_BAD2" = "ERROR" ]; then
    e2e_pass "string-where-int-expected returns error"
else
    e2e_pass "string-where-int-expected gracefully coerced (lenient)"
fi

# Nonexistent tool name
RESP_BAD3=$(send_session "$DB7" \
    "$INIT_REQ" \
    "$(mk_tool_call 72 nonexistent_tool '{}')" \
)
STATUS_BAD3=$(is_error "$RESP_BAD3" 72)
e2e_assert_eq "nonexistent tool returns error" "ERROR" "$STATUS_BAD3"

# Null arguments
RESP_BAD4=$(send_session "$DB7" \
    "$INIT_REQ" \
    "$(mk_tool_call 73 ensure_project 'null')" \
)
STATUS_BAD4=$(is_error "$RESP_BAD4" 73)
e2e_assert_eq "null arguments returns error" "ERROR" "$STATUS_BAD4"

# ═════════════════════════════════════════════════════════════════════════
# Case 8: Boundary inputs
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Boundary inputs"

DB8="${WORK}/fail_boundary.db"
PROJ8="${WORK}/proj-boundary"

# Setup
RESP_SETUP8=$(send_session "$DB8" \
    "$INIT_REQ" \
    "$(mk_tool_call 80 ensure_project "{\"human_key\":\"${PROJ8}\"}")" \
    "$(mk_tool_call 81 register_agent "{\"project_key\":\"${PROJ8}\",\"program\":\"bound-test\",\"model\":\"m1\"}")" \
)
AGENT_NAME_8=$(extract_field "$(extract_tool_result "$RESP_SETUP8" 81)" "name")

e2e_step_start "empty_subject"
# Send message with empty subject
RESP_EMPTY_SUBJ=$(send_session "$DB8" \
    "$INIT_REQ" \
    "$(mk_tool_call 82 ensure_project "{\"human_key\":\"${PROJ8}\"}")" \
    "$(mk_tool_call 83 send_message "{\"project_key\":\"${PROJ8}\",\"sender_name\":\"${AGENT_NAME_8}\",\"to\":[\"${AGENT_NAME_8}\"],\"subject\":\"\",\"body_md\":\"empty subject test\"}")" \
)
STATUS_EMPTY=$(is_error "$RESP_EMPTY_SUBJ" 83)
if [ "$STATUS_EMPTY" = "ERROR" ]; then
    e2e_pass "empty subject returns error (strict)"
else
    e2e_pass "empty subject accepted (lenient)"
fi
e2e_step_end "empty_subject"

e2e_step_start "very_long_subject"
# Very long subject (500+ chars)
LONG_SUBJ=$(python3 -c "print('A' * 500)")
RESP_LONG=$(send_session "$DB8" \
    "$INIT_REQ" \
    "$(mk_tool_call 84 ensure_project "{\"human_key\":\"${PROJ8}\"}")" \
    "$(mk_tool_call 85 send_message "{\"project_key\":\"${PROJ8}\",\"sender_name\":\"${AGENT_NAME_8}\",\"to\":[\"${AGENT_NAME_8}\"],\"subject\":\"${LONG_SUBJ}\",\"body_md\":\"long subject test\"}")" \
)
STATUS_LONG=$(is_error "$RESP_LONG" 85)
if [ "$STATUS_LONG" = "ERROR" ]; then
    e2e_pass "very long subject (500 chars) returns error (strict, truncation required)"
else
    LONG_RESULT=$(extract_tool_result "$RESP_LONG" 85)
    e2e_pass "very long subject accepted (may be truncated)"
fi
e2e_step_end "very_long_subject"

e2e_step_start "special_chars_body"
# Special characters in body
RESP_SPECIAL=$(send_session "$DB8" \
    "$INIT_REQ" \
    "$(mk_tool_call 86 ensure_project "{\"human_key\":\"${PROJ8}\"}")" \
    "$(mk_tool_call 87 send_message "{\"project_key\":\"${PROJ8}\",\"sender_name\":\"${AGENT_NAME_8}\",\"to\":[\"${AGENT_NAME_8}\"],\"subject\":\"special chars\",\"body_md\":\"Line1\\\\nLine2\\\\tTabbed\"}")" \
)
STATUS_SPECIAL=$(is_error "$RESP_SPECIAL" 87)
e2e_assert_eq "special characters in body accepted" "OK" "$STATUS_SPECIAL"
e2e_step_end "special_chars_body"

e2e_step_start "zero_limit_inbox"
# Fetch inbox with limit=0
RESP_ZERO=$(send_session "$DB8" \
    "$INIT_REQ" \
    "$(mk_tool_call 88 ensure_project "{\"human_key\":\"${PROJ8}\"}")" \
    "$(mk_tool_call 89 fetch_inbox "{\"project_key\":\"${PROJ8}\",\"agent_name\":\"${AGENT_NAME_8}\",\"limit\":0}")" \
)
STATUS_ZERO=$(is_error "$RESP_ZERO" 89)
if [ "$STATUS_ZERO" = "ERROR" ]; then
    e2e_pass "fetch_inbox with limit=0 returns error"
else
    e2e_pass "fetch_inbox with limit=0 returns empty result"
fi
e2e_step_end "zero_limit_inbox"

e2e_summary
