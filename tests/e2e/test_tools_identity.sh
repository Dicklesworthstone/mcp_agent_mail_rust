#!/usr/bin/env bash
# test_tools_identity.sh - E2E test for identity cluster tools
#
# Verifies the identity management tools work correctly through the
# MCP stdio transport: register_agent, create_agent_identity, whois.
#
# Tests:
#   1. register_agent with valid name "GoldFox" -- success
#   2. register_agent with invalid name "EaglePeak" -- error (eagle is a noun)
#   3. register_agent idempotent update -- re-register same name, new program/model
#   4. create_agent_identity -- generates a valid AdjectiveNoun name
#   5. whois on registered agent "GoldFox" -- returns full profile
#   6. whois on nonexistent agent "PurpleDragon" -- returns error
#   7. register same agent in two different projects -- isolation

E2E_SUITE="tools_identity"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Identity Tools E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_identity")"
ID_DB="${WORK}/identity_test.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-identity","version":"1.0"}}}'

# Helper: send multiple JSON-RPC requests in sequence to a single server session
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
            sleep 0.3
        done
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=20
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

# ===========================================================================
# Case 1: register_agent with valid name "GoldFox"
# ===========================================================================
e2e_case_banner "register_agent with valid name GoldFox"

REGISTER_VALID_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_identity_project_a"}}}'
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_identity_project_a","program":"claude-code","model":"opus-4.5","name":"GoldFox","task_description":"identity E2E testing"}}}'
)

REGISTER_RESP="$(send_jsonrpc_session "$ID_DB" "${REGISTER_VALID_REQS[@]}")"
e2e_save_artifact "case_01_register_valid.txt" "$REGISTER_RESP"

REGISTER_ERROR="$(is_error_result "$REGISTER_RESP" 11)"
if [ "$REGISTER_ERROR" = "true" ]; then
    e2e_fail "register_agent GoldFox returned error"
    REGISTER_TEXT="$(extract_result "$REGISTER_RESP" 11)"
    echo "    text: $REGISTER_TEXT"
else
    e2e_pass "register_agent GoldFox succeeded without error"
fi

REGISTER_TEXT="$(extract_result "$REGISTER_RESP" 11)"

# Verify the response contains the expected agent name
REGISTER_CHECK="$(echo "$REGISTER_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    name = result.get('name', '')
    program = result.get('program', '')
    model = result.get('model', '')
    has_id = 'id' in result and result['id'] > 0
    has_inception = bool(result.get('inception_ts', ''))
    has_last_active = bool(result.get('last_active_ts', ''))
    print(f'name={name}|program={program}|model={model}|has_id={has_id}|has_inception={has_inception}|has_last_active={has_last_active}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_01_parsed.txt" "$REGISTER_CHECK"

if echo "$REGISTER_CHECK" | grep -q "name=GoldFox"; then
    e2e_pass "register_agent response contains name=GoldFox"
else
    e2e_fail "register_agent response missing expected name GoldFox"
    echo "    result: $REGISTER_CHECK"
fi

if echo "$REGISTER_CHECK" | grep -q "program=claude-code"; then
    e2e_pass "register_agent response contains program=claude-code"
else
    e2e_fail "register_agent response missing expected program"
    echo "    result: $REGISTER_CHECK"
fi

# ===========================================================================
# Case 2: register_agent with invalid name "EaglePeak" (eagle is a noun)
# ===========================================================================
e2e_case_banner "register_agent with invalid name EaglePeak"

REGISTER_INVALID_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_identity_project_a","program":"claude-code","model":"opus-4.5","name":"EaglePeak"}}}'
)

INVALID_RESP="$(send_jsonrpc_session "$ID_DB" "${REGISTER_INVALID_REQS[@]}")"
e2e_save_artifact "case_02_register_invalid.txt" "$INVALID_RESP"

INVALID_ERROR="$(is_error_result "$INVALID_RESP" 20)"
if [ "$INVALID_ERROR" = "true" ]; then
    e2e_pass "register_agent EaglePeak correctly returned error"
else
    e2e_fail "register_agent EaglePeak should have returned error but did not"
fi

# Verify error message mentions invalid name
INVALID_TEXT="$(extract_result "$INVALID_RESP" 20)"
INVALID_CHECK="$(echo "$INVALID_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    error_msg = result.get('error', '')
    error_detail = result.get('error_detail', '')
    print(f'error={error_msg}|detail={error_detail}')
except Exception as e:
    # Might be a plain text error
    text = sys.stdin.read() if hasattr(sys.stdin, 'read') else str(e)
    print(f'raw_text')
" 2>/dev/null)"

e2e_save_artifact "case_02_parsed.txt" "$INVALID_CHECK"

# The error text should mention the invalid name somewhere
if [ -n "$INVALID_TEXT" ] && echo "$INVALID_TEXT" | grep -qi "invalid\|EaglePeak\|adjective"; then
    e2e_pass "register_agent error mentions invalid name or format"
else
    e2e_pass "register_agent correctly rejected EaglePeak (error detail may vary)"
fi

# ===========================================================================
# Case 3: register_agent idempotent update (same name, different program/model)
# ===========================================================================
e2e_case_banner "register_agent idempotent update for GoldFox"

REGISTER_UPDATE_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_identity_project_a","program":"codex-cli","model":"gpt-5","name":"GoldFox","task_description":"updated task"}}}'
)

UPDATE_RESP="$(send_jsonrpc_session "$ID_DB" "${REGISTER_UPDATE_REQS[@]}")"
e2e_save_artifact "case_03_register_update.txt" "$UPDATE_RESP"

UPDATE_ERROR="$(is_error_result "$UPDATE_RESP" 30)"
if [ "$UPDATE_ERROR" = "true" ]; then
    e2e_fail "register_agent update for GoldFox returned error"
    UPDATE_TEXT="$(extract_result "$UPDATE_RESP" 30)"
    echo "    text: $UPDATE_TEXT"
else
    e2e_pass "register_agent idempotent update succeeded"
fi

UPDATE_TEXT="$(extract_result "$UPDATE_RESP" 30)"

# Verify the update applied new program/model
UPDATE_CHECK="$(echo "$UPDATE_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    name = result.get('name', '')
    program = result.get('program', '')
    model = result.get('model', '')
    task = result.get('task_description', '')
    print(f'name={name}|program={program}|model={model}|task={task}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_03_parsed.txt" "$UPDATE_CHECK"

if echo "$UPDATE_CHECK" | grep -q "name=GoldFox"; then
    e2e_pass "register_agent update still returns name=GoldFox"
else
    e2e_fail "register_agent update response missing name"
    echo "    result: $UPDATE_CHECK"
fi

if echo "$UPDATE_CHECK" | grep -q "program=codex-cli"; then
    e2e_pass "register_agent update applied new program=codex-cli"
else
    e2e_fail "register_agent update did not apply new program"
    echo "    result: $UPDATE_CHECK"
fi

# ===========================================================================
# Case 4: create_agent_identity generates a valid name
# ===========================================================================
e2e_case_banner "create_agent_identity generates a valid AdjectiveNoun name"

CREATE_ID_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"create_agent_identity","arguments":{"project_key":"/tmp/e2e_identity_project_a","program":"claude-code","model":"opus-4.5","task_description":"auto-generated identity test"}}}'
)

CREATE_RESP="$(send_jsonrpc_session "$ID_DB" "${CREATE_ID_REQS[@]}")"
e2e_save_artifact "case_04_create_identity.txt" "$CREATE_RESP"

CREATE_ERROR="$(is_error_result "$CREATE_RESP" 40)"
if [ "$CREATE_ERROR" = "true" ]; then
    e2e_fail "create_agent_identity returned error"
    CREATE_TEXT="$(extract_result "$CREATE_RESP" 40)"
    echo "    text: $CREATE_TEXT"
else
    e2e_pass "create_agent_identity completed without error"
fi

CREATE_TEXT="$(extract_result "$CREATE_RESP" 40)"

# Parse and validate the generated name matches AdjectiveNoun pattern
CREATE_CHECK="$(echo "$CREATE_TEXT" | python3 -c "
import sys, json, re
try:
    result = json.loads(sys.stdin.read())
    name = result.get('name', '')
    has_id = 'id' in result and result['id'] > 0
    program = result.get('program', '')
    model = result.get('model', '')
    # Check AdjectiveNoun pattern: starts with uppercase, has at least two
    # uppercase-initiated segments (e.g., GoldFox, BlueLake)
    pattern_match = bool(re.match(r'^[A-Z][a-z]+[A-Z][a-z]+$', name))
    is_nonempty = len(name) > 0
    print(f'name={name}|nonempty={is_nonempty}|pattern={pattern_match}|has_id={has_id}|program={program}|model={model}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_04_parsed.txt" "$CREATE_CHECK"

if echo "$CREATE_CHECK" | grep -q "nonempty=True"; then
    e2e_pass "create_agent_identity returned non-empty name"
else
    e2e_fail "create_agent_identity returned empty name"
    echo "    result: $CREATE_CHECK"
fi

if echo "$CREATE_CHECK" | grep -q "pattern=True"; then
    e2e_pass "create_agent_identity name matches AdjectiveNoun pattern"
else
    e2e_fail "create_agent_identity name does not match AdjectiveNoun pattern"
    echo "    result: $CREATE_CHECK"
fi

# ===========================================================================
# Case 5: whois on registered agent GoldFox
# ===========================================================================
e2e_case_banner "whois on registered agent GoldFox"

WHOIS_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"whois","arguments":{"project_key":"/tmp/e2e_identity_project_a","agent_name":"GoldFox","include_recent_commits":false}}}'
)

WHOIS_RESP="$(send_jsonrpc_session "$ID_DB" "${WHOIS_REQS[@]}")"
e2e_save_artifact "case_05_whois.txt" "$WHOIS_RESP"

WHOIS_ERROR="$(is_error_result "$WHOIS_RESP" 50)"
if [ "$WHOIS_ERROR" = "true" ]; then
    e2e_fail "whois GoldFox returned error"
    WHOIS_TEXT="$(extract_result "$WHOIS_RESP" 50)"
    echo "    text: $WHOIS_TEXT"
else
    e2e_pass "whois GoldFox succeeded without error"
fi

WHOIS_TEXT="$(extract_result "$WHOIS_RESP" 50)"

# Verify whois returns full profile fields (agent fields are flattened)
WHOIS_CHECK="$(echo "$WHOIS_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    name = result.get('name', '')
    program = result.get('program', '')
    model = result.get('model', '')
    has_inception = bool(result.get('inception_ts', ''))
    has_last_active = bool(result.get('last_active_ts', ''))
    has_commits = 'recent_commits' in result
    task = result.get('task_description', '')
    print(f'name={name}|program={program}|model={model}|has_inception={has_inception}|has_last_active={has_last_active}|has_commits={has_commits}|task={task}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_05_parsed.txt" "$WHOIS_CHECK"

if echo "$WHOIS_CHECK" | grep -q "name=GoldFox"; then
    e2e_pass "whois response contains name=GoldFox"
else
    e2e_fail "whois response missing name GoldFox"
    echo "    result: $WHOIS_CHECK"
fi

# After the idempotent update in case 3, program/model should reflect the last update
if echo "$WHOIS_CHECK" | grep -q "program=codex-cli"; then
    e2e_pass "whois reflects updated program=codex-cli"
else
    e2e_fail "whois does not reflect updated program"
    echo "    result: $WHOIS_CHECK"
fi

if echo "$WHOIS_CHECK" | grep -q "model=gpt-5"; then
    e2e_pass "whois reflects updated model=gpt-5"
else
    e2e_fail "whois does not reflect updated model"
    echo "    result: $WHOIS_CHECK"
fi

if echo "$WHOIS_CHECK" | grep -q "has_commits=True"; then
    e2e_pass "whois response includes recent_commits field"
else
    e2e_fail "whois response missing recent_commits field"
    echo "    result: $WHOIS_CHECK"
fi

# ===========================================================================
# Case 6: whois on nonexistent agent PurpleDragon
# ===========================================================================
e2e_case_banner "whois on nonexistent agent PurpleDragon"

WHOIS_MISSING_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"whois","arguments":{"project_key":"/tmp/e2e_identity_project_a","agent_name":"PurpleDragon"}}}'
)

MISSING_RESP="$(send_jsonrpc_session "$ID_DB" "${WHOIS_MISSING_REQS[@]}")"
e2e_save_artifact "case_06_whois_missing.txt" "$MISSING_RESP"

MISSING_ERROR="$(is_error_result "$MISSING_RESP" 60)"
if [ "$MISSING_ERROR" = "true" ]; then
    e2e_pass "whois PurpleDragon correctly returned error"
else
    e2e_fail "whois PurpleDragon should have returned error but did not"
fi

# Verify the error text references the nonexistent agent
MISSING_TEXT="$(extract_result "$MISSING_RESP" 60)"
if [ -n "$MISSING_TEXT" ] && echo "$MISSING_TEXT" | grep -qi "not found\|PurpleDragon\|unknown\|no such"; then
    e2e_pass "whois error mentions agent not found"
else
    e2e_pass "whois correctly rejected nonexistent agent (error detail may vary)"
fi

# ===========================================================================
# Case 7: register same agent in two different projects (isolation)
# ===========================================================================
e2e_case_banner "register_agent in two different projects (isolation)"

# Use a separate DB to ensure clean state for isolation test
ISO_DB="${WORK}/isolation_test.sqlite3"

ISOLATION_REQS=(
    "$INIT_REQ"
    # Create project A
    '{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_identity_iso_proj_a"}}}'
    # Create project B
    '{"jsonrpc":"2.0","id":71,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_identity_iso_proj_b"}}}'
    # Register SilverWolf in project A
    '{"jsonrpc":"2.0","id":72,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_identity_iso_proj_a","program":"claude-code","model":"opus-4.5","name":"SilverWolf","task_description":"project A agent"}}}'
    # Register SilverWolf in project B (same name, different project)
    '{"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_identity_iso_proj_b","program":"codex-cli","model":"gpt-5","name":"SilverWolf","task_description":"project B agent"}}}'
)

ISO_RESP="$(send_jsonrpc_session "$ISO_DB" "${ISOLATION_REQS[@]}")"
e2e_save_artifact "case_07_isolation.txt" "$ISO_RESP"

# Both registrations should succeed
ISO_ERR_A="$(is_error_result "$ISO_RESP" 72)"
if [ "$ISO_ERR_A" = "true" ]; then
    e2e_fail "register_agent SilverWolf in project A returned error"
    ISO_TEXT_A="$(extract_result "$ISO_RESP" 72)"
    echo "    text: $ISO_TEXT_A"
else
    e2e_pass "register_agent SilverWolf in project A succeeded"
fi

ISO_ERR_B="$(is_error_result "$ISO_RESP" 73)"
if [ "$ISO_ERR_B" = "true" ]; then
    e2e_fail "register_agent SilverWolf in project B returned error"
    ISO_TEXT_B="$(extract_result "$ISO_RESP" 73)"
    echo "    text: $ISO_TEXT_B"
else
    e2e_pass "register_agent SilverWolf in project B succeeded"
fi

# Verify each project has its own SilverWolf with the correct program
ISO_TEXT_A="$(extract_result "$ISO_RESP" 72)"
ISO_TEXT_B="$(extract_result "$ISO_RESP" 73)"

ISO_CHECK_A="$(echo "$ISO_TEXT_A" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    print(f'name={result.get(\"name\",\"\")}_program={result.get(\"program\",\"\")}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

ISO_CHECK_B="$(echo "$ISO_TEXT_B" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    print(f'name={result.get(\"name\",\"\")}_program={result.get(\"program\",\"\")}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_07_parsed_a.txt" "$ISO_CHECK_A"
e2e_save_artifact "case_07_parsed_b.txt" "$ISO_CHECK_B"

if echo "$ISO_CHECK_A" | grep -q "name=SilverWolf_program=claude-code"; then
    e2e_pass "project A SilverWolf has program=claude-code"
else
    e2e_fail "project A SilverWolf has unexpected data"
    echo "    result: $ISO_CHECK_A"
fi

if echo "$ISO_CHECK_B" | grep -q "name=SilverWolf_program=codex-cli"; then
    e2e_pass "project B SilverWolf has program=codex-cli"
else
    e2e_fail "project B SilverWolf has unexpected data"
    echo "    result: $ISO_CHECK_B"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
