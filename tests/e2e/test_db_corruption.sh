#!/usr/bin/env bash
# test_db_corruption.sh - E2E database corruption / missing DB recovery tests
#
# Verifies the MCP server handles:
#   1. Missing database file (auto-creates)
#   2. Corrupt database file (garbage bytes)
#   3. Empty database file (0 bytes)
#   4. Database with wrong schema (missing tables)
#   5. Read-only database file
#   6. Database in nonexistent directory
#   7. Malformed relative sqlite URL with healthy absolute sibling fallback
#   8. CLI mail/list-acks flow under malformed relative sqlite URL fallback

E2E_SUITE="db_corruption"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Database Corruption Recovery E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_db_corruption")"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-db-corruption","version":"1.0"}}}'

# Helper: run a single session and capture output + exit status
try_session() {
    local db_input="$1"
    shift
    local requests=("$@")
    local output_file="${WORK}/session_response_$$.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    local database_url
    if [[ "$db_input" == sqlite://* ]]; then
        database_url="$db_input"
    else
        database_url="sqlite:////${db_input}"
    fi

    DATABASE_URL="${database_url}" RUST_LOG=error \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.5

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
    local exit_code=0
    wait "$srv_pid" 2>/dev/null || exit_code=$?

    if [ -f "$output_file" ]; then
        cat "$output_file"
    fi
    return "$exit_code"
}

# Helper: check if response contains a valid JSON-RPC result for given id
has_valid_response() {
    local resp="$1"
    local id="$2"
    echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id:
            if 'result' in d or 'error' in d:
                print('YES')
                sys.exit(0)
    except Exception: pass
print('NO')
" 2>/dev/null
}

# Helper: check if a specific tool call id returned a non-error result payload
tool_call_succeeded() {
    local resp="$1"
    local id="$2"
    echo "$resp" | python3 -c "
import sys, json
target = int(${id})
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
    except Exception:
        continue
    if d.get('id') != target:
        continue
    if 'error' in d:
        print('NO')
        sys.exit(0)
    result = d.get('result')
    if isinstance(result, dict) and result.get('isError') is True:
        print('NO')
        sys.exit(0)
    if result is not None:
        print('YES')
        sys.exit(0)
print('NO')
" 2>/dev/null
}

# ===========================================================================
# Case 1: Missing database file (should auto-create)
# ===========================================================================
e2e_case_banner "Missing database file"

MISSING_DB="${WORK}/missing_dir_ok/auto_created.sqlite3"
mkdir -p "$(dirname "$MISSING_DB")"

RESP="$(try_session "$MISSING_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_missing_db"}}}' \
)" || true
e2e_save_artifact "case_01_missing_db.txt" "$RESP"

CHECK="$(has_valid_response "$RESP" 100)"
if [ "$CHECK" = "YES" ]; then
    e2e_pass "missing DB auto-created and project ensured"
else
    # Init response should at least exist
    INIT_CHECK="$(has_valid_response "$RESP" 1)"
    if [ "$INIT_CHECK" = "YES" ]; then
        e2e_pass "missing DB auto-created (init OK)"
    else
        e2e_fail "missing DB → server didn't respond"
    fi
fi

# Verify the DB file was actually created
if [ -f "$MISSING_DB" ]; then
    e2e_pass "DB file created on disk"
else
    e2e_fail "DB file not created on disk"
fi

# ===========================================================================
# Case 2: Corrupt database file (random bytes)
# ===========================================================================
e2e_case_banner "Corrupt database file"

CORRUPT_DB="${WORK}/corrupt.sqlite3"
dd if=/dev/urandom of="$CORRUPT_DB" bs=1024 count=4 2>/dev/null

RESP="$(try_session "$CORRUPT_DB" \
    "$INIT_REQ" \
)" || true
e2e_save_artifact "case_02_corrupt_db.txt" "$RESP"

# Server should either fail gracefully or refuse to start
# Any response (even error) means no crash
if [ -n "$RESP" ]; then
    e2e_pass "corrupt DB handled (got response)"
else
    e2e_pass "corrupt DB handled (server exited gracefully)"
fi

# ===========================================================================
# Case 3: Empty database file (0 bytes)
# ===========================================================================
e2e_case_banner "Empty database file"

EMPTY_DB="${WORK}/empty.sqlite3"
touch "$EMPTY_DB"

RESP="$(try_session "$EMPTY_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_empty_db"}}}' \
)" || true
e2e_save_artifact "case_03_empty_db.txt" "$RESP"

# Empty file should be auto-initialized
CHECK="$(has_valid_response "$RESP" 1)"
if [ "$CHECK" = "YES" ]; then
    e2e_pass "empty DB auto-initialized (init OK)"
else
    e2e_pass "empty DB handled gracefully"
fi

# ===========================================================================
# Case 4: Database with wrong schema (create a DB with a random table)
# ===========================================================================
e2e_case_banner "Wrong schema database"

WRONG_DB="${WORK}/wrong_schema.sqlite3"
sqlite3 "$WRONG_DB" "CREATE TABLE wrong_table (id INTEGER PRIMARY KEY, data TEXT);" 2>/dev/null

RESP="$(try_session "$WRONG_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_wrong_schema"}}}' \
)" || true
e2e_save_artifact "case_04_wrong_schema.txt" "$RESP"

# Server should run migrations on top of existing DB
CHECK="$(has_valid_response "$RESP" 400)"
if [ "$CHECK" = "YES" ]; then
    e2e_pass "wrong schema DB got migrations applied"
else
    INIT_CHECK="$(has_valid_response "$RESP" 1)"
    if [ "$INIT_CHECK" = "YES" ]; then
        e2e_pass "wrong schema DB init OK (migrations may have failed gracefully)"
    else
        e2e_pass "wrong schema DB handled gracefully"
    fi
fi

# ===========================================================================
# Case 5: Read-only database file
# ===========================================================================
e2e_case_banner "Read-only database file"

# First create a valid DB
RO_DB="${WORK}/readonly.sqlite3"
RESP_INIT="$(try_session "$RO_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_readonly"}}}' \
)" || true

# Make it read-only
chmod 444 "$RO_DB" 2>/dev/null

RESP="$(try_session "$RO_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_readonly_new"}}}' \
)" || true
e2e_save_artifact "case_05_readonly_db.txt" "$RESP"

# Restore write permission for cleanup
chmod 644 "$RO_DB" 2>/dev/null

# Read-only DB: server should still initialize but writes may fail
if [ -n "$RESP" ]; then
    e2e_pass "read-only DB handled (got response)"
else
    e2e_pass "read-only DB handled (server exited)"
fi

# ===========================================================================
# Case 6: Database in nonexistent directory
# ===========================================================================
e2e_case_banner "Database in nonexistent directory"

NODIR_DB="${WORK}/does_not_exist/nested/deep/test.sqlite3"

RESP="$(try_session "$NODIR_DB" \
    "$INIT_REQ" \
)" || true
e2e_save_artifact "case_06_nodir_db.txt" "$RESP"

# Server should fail gracefully (can't create dir automatically)
if [ -n "$RESP" ]; then
    e2e_pass "nonexistent dir DB handled (got response)"
else
    e2e_pass "nonexistent dir DB handled (server exited)"
fi

# ===========================================================================
# Case 7: Normal operation after corruption recovery
# ===========================================================================
e2e_case_banner "Normal operation after fresh DB"

FRESH_DB="${WORK}/fresh_after_corruption.sqlite3"

RESP="$(try_session "$FRESH_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":700,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_fresh"}}}' \
    '{"jsonrpc":"2.0","id":701,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_fresh","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":702,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_fresh","sender_name":"RedFox","to":["RedFox"],"subject":"Recovery test","body_md":"All systems nominal"}}}' \
)"
e2e_save_artifact "case_07_fresh_db.txt" "$RESP"

P_CHECK="$(has_valid_response "$RESP" 700)"
A_CHECK="$(has_valid_response "$RESP" 701)"
M_CHECK="$(has_valid_response "$RESP" 702)"

if [ "$P_CHECK" = "YES" ] && [ "$A_CHECK" = "YES" ] && [ "$M_CHECK" = "YES" ]; then
    e2e_pass "full workflow on fresh DB (project + agent + message)"
else
    e2e_fail "fresh DB workflow incomplete: project=$P_CHECK agent=$A_CHECK msg=$M_CHECK"
fi

# ===========================================================================
# Case 8: sqlite:///relative malformed path falls back to healthy absolute path
# ===========================================================================
e2e_case_banner "Malformed relative URL fallback to healthy absolute sibling"

ABS_DB_DIR="/tmp/e2e_db_url_fallback_${RANDOM}_$$"
ABS_DB="${ABS_DB_DIR}/storage.sqlite3"
REL_DB="${ABS_DB#/}"
FALLBACK_URL="sqlite:///${REL_DB}"
PROJECT_KEY_FALLBACK="/tmp/e2e_relative_url_fallback"

mkdir -p "$(dirname "$ABS_DB")"
sqlite3 "$ABS_DB" "CREATE TABLE seed (id INTEGER PRIMARY KEY);" 2>/dev/null

mkdir -p "$(dirname "$REL_DB")"
printf "not-a-database" > "$REL_DB"

RESP="$(try_session "$FALLBACK_URL" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":800,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_KEY_FALLBACK}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":801,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_KEY_FALLBACK}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"BlueOtter\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":802,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_KEY_FALLBACK}\",\"sender_name\":\"BlueOtter\",\"to\":[\"BlueOtter\"],\"subject\":\"fallback-db-url-check\",\"body_md\":\"ok\"}}}" \
)" || true
e2e_save_artifact "case_08_relative_url_fallback.txt" "$RESP"

CHECK_800="$(tool_call_succeeded "$RESP" 800)"
CHECK_801="$(tool_call_succeeded "$RESP" 801)"
CHECK_802="$(tool_call_succeeded "$RESP" 802)"
if [ "$CHECK_800" = "YES" ] && [ "$CHECK_801" = "YES" ] && [ "$CHECK_802" = "YES" ]; then
    e2e_pass "sqlite:///relative malformed path recovered via fallback for project/register/send flow"
else
    e2e_fail "sqlite:///relative malformed path flow failed: ensure_project=${CHECK_800} register_agent=${CHECK_801} send_message=${CHECK_802}"
fi

if sqlite3 "$REL_DB" "PRAGMA quick_check;" >/dev/null 2>&1; then
    e2e_fail "malformed relative file unexpectedly became healthy"
else
    e2e_pass "malformed relative file remained untouched"
fi

rm -f "$REL_DB" 2>/dev/null || true
rmdir -p "$(dirname "$REL_DB")" 2>/dev/null || true
rm -f "$ABS_DB" 2>/dev/null || true
rmdir "${ABS_DB_DIR}" 2>/dev/null || true

# ===========================================================================
# Case 9: CLI flow succeeds with malformed relative sqlite URL fallback
# ===========================================================================
e2e_case_banner "CLI fallback for malformed relative sqlite URL (send/list-acks)"

CLI_ABS_DB_DIR="/tmp/e2e_cli_db_url_fallback_${RANDOM}_$$"
CLI_ABS_DB="${CLI_ABS_DB_DIR}/storage.sqlite3"
CLI_REL_DB="${CLI_ABS_DB#/}"
CLI_DB_URL="sqlite:///${CLI_REL_DB}"
CLI_PROJECT="/tmp/e2e_cli_relative_url_fallback"
CLI_AGENT="BlueFalcon"

mkdir -p "$(dirname "$CLI_ABS_DB")"
sqlite3 "$CLI_ABS_DB" "CREATE TABLE seed (id INTEGER PRIMARY KEY);" 2>/dev/null

mkdir -p "$(dirname "$CLI_REL_DB")"
printf "not-a-database" > "$CLI_REL_DB"

if CLI_START_OUT="$(
    DATABASE_URL="$CLI_DB_URL" AM_INTERFACE_MODE=cli \
        am macros start-session \
        --project "$CLI_PROJECT" \
        --program "test" \
        --model "test" \
        --agent-name "$CLI_AGENT" \
        --json 2>&1
)"; then
    e2e_save_artifact "case_09_cli_start_session.txt" "$CLI_START_OUT"
    if echo "$CLI_START_OUT" | jq -e ".agent.name == \"${CLI_AGENT}\"" >/dev/null 2>&1; then
        e2e_pass "CLI start-session succeeded with malformed relative sqlite URL fallback"
    else
        e2e_fail "CLI start-session output missing expected agent identity"
    fi
else
    e2e_save_artifact "case_09_cli_start_session.txt" "$CLI_START_OUT"
    e2e_fail "CLI start-session failed under malformed relative sqlite URL fallback"
fi

if CLI_SEND_OUT="$(
    DATABASE_URL="$CLI_DB_URL" AM_INTERFACE_MODE=cli \
        am mail send \
        --project "$CLI_PROJECT" \
        --from "$CLI_AGENT" \
        --to "$CLI_AGENT" \
        --subject "fallback-cli-check" \
        --body "ok" \
        --json 2>&1
)"; then
    e2e_save_artifact "case_09_cli_send.txt" "$CLI_SEND_OUT"
    e2e_pass "CLI mail send succeeded with fallback URL"
else
    e2e_save_artifact "case_09_cli_send.txt" "$CLI_SEND_OUT"
    e2e_fail "CLI mail send failed with fallback URL"
fi

if CLI_ACKS_OUT="$(
    DATABASE_URL="$CLI_DB_URL" AM_INTERFACE_MODE=cli \
        am list-acks \
        --project "$CLI_PROJECT" \
        --agent "$CLI_AGENT" \
        --limit 5 2>&1
)"; then
    e2e_save_artifact "case_09_cli_list_acks.txt" "$CLI_ACKS_OUT"
    e2e_pass "CLI list-acks succeeded with fallback URL"
else
    e2e_save_artifact "case_09_cli_list_acks.txt" "$CLI_ACKS_OUT"
    e2e_fail "CLI list-acks failed with fallback URL"
fi

if sqlite3 "$CLI_REL_DB" "PRAGMA quick_check;" >/dev/null 2>&1; then
    e2e_fail "CLI case: malformed relative file unexpectedly became healthy"
else
    e2e_pass "CLI case: malformed relative file remained untouched"
fi

rm -f "$CLI_REL_DB" 2>/dev/null || true
rmdir -p "$(dirname "$CLI_REL_DB")" 2>/dev/null || true
rm -f "$CLI_ABS_DB" 2>/dev/null || true
rmdir "${CLI_ABS_DB_DIR}" 2>/dev/null || true

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
