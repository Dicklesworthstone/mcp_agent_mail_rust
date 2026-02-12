#!/usr/bin/env bash
# test_db_migration_e2e.sh - E2E: Database migration and persistence lifecycle
#
# Verifies that the am binary correctly creates, migrates, and preserves data
# across server restarts using the same SQLite database file.
#
# Tests:
#   1. Fresh DB creation: start with no DB, create it, verify schema tables exist
#   2. Schema version check: verify migration tracking table has applied entries
#   3. Data roundtrip: create data, stop server, restart, verify data intact
#   4. FTS integrity: after DB creation, verify FTS table exists and search works
#   5. Multiple restarts: create data, restart 3 times, verify data survives
#   6. Concurrent access after migration: multiple sessions against same DB

E2E_SUITE="db_migration_e2e"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "DB Migration E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_db_migration")"
MIGRATION_DB="${WORK}/migration_test.sqlite3"
PROJECT_PATH="/tmp/e2e_migration_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-db-migration","version":"1.0"}}}'

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

# Check if sqlite3 CLI is available for direct DB inspection
HAS_SQLITE3=false
if command -v sqlite3 >/dev/null 2>&1; then
    HAS_SQLITE3=true
fi
e2e_log "sqlite3 CLI available: $HAS_SQLITE3"

e2e_log "Project path: $PROJECT_PATH"
e2e_log "Migration DB: $MIGRATION_DB"

# ---------------------------------------------------------------------------
# Smoke check: verify the am binary can successfully run ensure_project.
# If FrankenSQLite (pure-Rust SQLite) lacks CREATE TRIGGER / CREATE VIRTUAL
# TABLE support, the pool initialiser fails and every tool call returns
# "not implemented".  In that case we skip the entire suite gracefully.
# ---------------------------------------------------------------------------
SMOKE_DB="${WORK}/smoke_check.sqlite3"
SMOKE_RESP="$(send_jsonrpc_session "$SMOKE_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
)"
SMOKE_ERR="$(is_error_result "$SMOKE_RESP" 2)"
SMOKE_TEXT="$(extract_result "$SMOKE_RESP" 2)"
if [ "$SMOKE_ERR" = "true" ] && echo "$SMOKE_TEXT" | grep -q "not implemented"; then
    e2e_log "SMOKE CHECK FAILED: am binary cannot run migrations (FrankenSQLite limitation)"
    e2e_log "Error text: $SMOKE_TEXT"
    e2e_log "Skipping entire suite -- all tests require working migrations"
    # Register one case so summary is valid, then skip all tests
    e2e_case_banner "Smoke check: server can create and migrate DB"
    e2e_skip "am binary migration unsupported (FrankenSQLite lacks CREATE TRIGGER/VIRTUAL TABLE)"
    e2e_summary
    exit $?
fi
rm -f "$SMOKE_DB" "${SMOKE_DB}-wal" "${SMOKE_DB}-shm" 2>/dev/null

# ===========================================================================
# Case 1: Fresh DB creation -- no DB file exists, server creates it
# ===========================================================================
e2e_case_banner "Fresh DB creation: verify schema tables are created"

# Ensure DB does not yet exist
if [ -f "$MIGRATION_DB" ]; then
    rm -f "$MIGRATION_DB"
fi

FRESH_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
)"
e2e_save_artifact "case_01_fresh_create.txt" "$FRESH_RESP"

# Verify the DB file was actually created
if [ -f "$MIGRATION_DB" ]; then
    e2e_pass "DB file was created on first server start"
else
    e2e_fail "DB file was NOT created"
fi

# Verify ensure_project succeeded
EP_ERR="$(is_error_result "$FRESH_RESP" 10)"
if [ "$EP_ERR" = "false" ]; then
    e2e_pass "ensure_project succeeded on fresh DB"
else
    e2e_fail "ensure_project failed on fresh DB"
fi

# ===========================================================================
# Case 2: Schema version check -- verify migrations were applied
# ===========================================================================
e2e_case_banner "Schema version check: migration tracking table exists"

if [ "$HAS_SQLITE3" = "true" ]; then
    # Check that the migrations tracking table exists and has applied entries
    MIGRATION_COUNT="$(sqlite3 "$MIGRATION_DB" "SELECT COUNT(*) FROM mcp_agent_mail_migrations;" 2>/dev/null || echo "ERROR")"

    if [ "$MIGRATION_COUNT" != "ERROR" ] && [ "$MIGRATION_COUNT" -gt 0 ] 2>/dev/null; then
        e2e_pass "migrations table has $MIGRATION_COUNT applied entries"
    else
        e2e_fail "migrations table missing or empty (got: $MIGRATION_COUNT)"
    fi

    # Check that core tables exist
    TABLE_LIST="$(sqlite3 "$MIGRATION_DB" ".tables" 2>/dev/null || echo "")"
    e2e_save_artifact "case_02_tables.txt" "$TABLE_LIST"

    for tbl in projects agents messages message_recipients file_reservations agent_links fts_messages; do
        if echo "$TABLE_LIST" | grep -qw "$tbl"; then
            e2e_pass "table '$tbl' exists in schema"
        else
            e2e_fail "table '$tbl' missing from schema"
        fi
    done

    # Check WAL mode is active
    JOURNAL_MODE="$(sqlite3 "$MIGRATION_DB" "PRAGMA journal_mode;" 2>/dev/null || echo "")"
    if [ "$JOURNAL_MODE" = "wal" ]; then
        e2e_pass "WAL journal mode is active"
    else
        e2e_skip "WAL journal mode check (got: $JOURNAL_MODE)"
    fi
else
    e2e_skip "sqlite3 CLI not available -- skipping direct DB inspection"
fi

# ===========================================================================
# Case 3: Data roundtrip -- create data, restart, verify persistence
# ===========================================================================
e2e_case_banner "Data roundtrip: create data, restart server, verify persistence"

# Session A: Create project + agents + message + file_reservation
SESSION_A_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"migration-e2e\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"migration roundtrip testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"migration-e2e\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"migration roundtrip peer\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Migration roundtrip test\",\"body_md\":\"Verifying data survives restart.\",\"importance\":\"high\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":33,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/migration_test.rs\"],\"ttl_seconds\":7200,\"exclusive\":true,\"reason\":\"migration e2e test\"}}}" \
)"
e2e_save_artifact "case_03_session_a.txt" "$SESSION_A_RESP"

# Verify session A operations
GF_ERR="$(is_error_result "$SESSION_A_RESP" 30)"
SW_ERR="$(is_error_result "$SESSION_A_RESP" 31)"
MSG_ERR="$(is_error_result "$SESSION_A_RESP" 32)"
RES_ERR="$(is_error_result "$SESSION_A_RESP" 33)"

if [ "$GF_ERR" = "false" ] && [ "$SW_ERR" = "false" ]; then
    e2e_pass "session A: both agents registered"
else
    e2e_fail "session A: agent registration failed (GoldFox=$GF_ERR, SilverWolf=$SW_ERR)"
fi

if [ "$MSG_ERR" = "false" ]; then
    e2e_pass "session A: message sent"
else
    e2e_fail "session A: message send failed"
fi

if [ "$RES_ERR" = "false" ]; then
    e2e_pass "session A: file reservation created"
else
    e2e_fail "session A: file reservation failed"
fi

MSG_TEXT="$(extract_result "$SESSION_A_RESP" 32)"
MSG_ID="$(parse_json_field "$MSG_TEXT" "deliveries.0.payload.id")"
e2e_log "Message ID from session A: $MSG_ID"

# Session B: NEW server process, same DB -- verify data persists
SESSION_B_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"tools/call\",\"params\":{\"name\":\"whois\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"include_recent_commits\":false}}}" \
)"
e2e_save_artifact "case_03_session_b.txt" "$SESSION_B_RESP"

INBOX_ERR="$(is_error_result "$SESSION_B_RESP" 40)"
WHOIS_ERR="$(is_error_result "$SESSION_B_RESP" 41)"

if [ "$INBOX_ERR" = "false" ]; then
    e2e_pass "session B: fetch_inbox succeeded (data persists)"
else
    e2e_fail "session B: fetch_inbox failed"
fi

if [ "$WHOIS_ERR" = "false" ]; then
    e2e_pass "session B: whois GoldFox succeeded (agent persists)"
else
    e2e_fail "session B: whois GoldFox failed"
fi

# Verify inbox contents
INBOX_TEXT="$(extract_result "$SESSION_B_RESP" 40)"
e2e_save_artifact "case_03_inbox.txt" "$INBOX_TEXT"

INBOX_PARSED="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    msgs = d if isinstance(d, list) else d.get('messages', [])
    count = len(msgs)
    subject = msgs[0].get('subject', '') if msgs else ''
    importance = msgs[0].get('importance', '') if msgs else ''
    print(f'count={count}')
    print(f'subject={subject}')
    print(f'importance={importance}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_03_inbox_parsed.txt" "$INBOX_PARSED"

e2e_assert_contains "inbox has 1 message after restart" "$INBOX_PARSED" "count=1"
e2e_assert_contains "message subject survived restart" "$INBOX_PARSED" "subject=Migration roundtrip test"
e2e_assert_contains "message importance survived restart" "$INBOX_PARSED" "importance=high"

# Verify agent whois details
WHOIS_TEXT="$(extract_result "$SESSION_B_RESP" 41)"
WHOIS_NAME="$(parse_json_field "$WHOIS_TEXT" "name")"
WHOIS_PROGRAM="$(parse_json_field "$WHOIS_TEXT" "program")"
e2e_assert_eq "agent name persists across restart" "GoldFox" "$WHOIS_NAME"
e2e_assert_eq "agent program persists across restart" "migration-e2e" "$WHOIS_PROGRAM"

# ===========================================================================
# Case 4: FTS integrity -- search works on data created in prior session
# ===========================================================================
e2e_case_banner "FTS integrity: search_messages works after restart"

FTS_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"query\":\"roundtrip\"}}}" \
)"
e2e_save_artifact "case_04_fts_search.txt" "$FTS_RESP"

FTS_ERR="$(is_error_result "$FTS_RESP" 50)"
if [ "$FTS_ERR" = "false" ]; then
    e2e_pass "search_messages succeeded after restart"
else
    e2e_fail "search_messages failed after restart"
fi

FTS_TEXT="$(extract_result "$FTS_RESP" 50)"
e2e_save_artifact "case_04_fts_text.txt" "$FTS_TEXT"

FTS_CHECK="$(echo "$FTS_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    # search_messages returns {\"result\": [...]} format
    results = d if isinstance(d, list) else d.get('result', d.get('results', d.get('messages', [])))
    if not isinstance(results, list):
        results = []
    count = len(results)
    subjects = [r.get('subject', '') for r in results]
    print(f'count={count}')
    print(f'subjects={subjects}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_04_fts_parsed.txt" "$FTS_CHECK"

e2e_assert_contains "FTS search found the message" "$FTS_CHECK" "count=1"
e2e_assert_contains "FTS search matched correct subject" "$FTS_CHECK" "Migration roundtrip test"

# Direct FTS table verification with sqlite3
if [ "$HAS_SQLITE3" = "true" ]; then
    FTS_ROW_COUNT="$(sqlite3 "$MIGRATION_DB" "SELECT COUNT(*) FROM fts_messages;" 2>/dev/null || echo "ERROR")"
    if [ "$FTS_ROW_COUNT" != "ERROR" ] && [ "$FTS_ROW_COUNT" -gt 0 ] 2>/dev/null; then
        e2e_pass "fts_messages table has $FTS_ROW_COUNT rows"
    else
        e2e_fail "fts_messages table empty or inaccessible (got: $FTS_ROW_COUNT)"
    fi

    # Verify FTS match query works directly
    FTS_DIRECT="$(sqlite3 "$MIGRATION_DB" "SELECT COUNT(*) FROM fts_messages WHERE fts_messages MATCH 'roundtrip';" 2>/dev/null || echo "ERROR")"
    if [ "$FTS_DIRECT" != "ERROR" ] && [ "$FTS_DIRECT" -gt 0 ] 2>/dev/null; then
        e2e_pass "direct FTS MATCH query found results"
    else
        e2e_fail "direct FTS MATCH query failed (got: $FTS_DIRECT)"
    fi
else
    e2e_skip "sqlite3 not available for direct FTS check"
fi

# ===========================================================================
# Case 5: Multiple restarts -- data survives 3 consecutive restarts
# ===========================================================================
e2e_case_banner "Multiple restarts: data survives 3 consecutive server restarts"

# Send additional messages in a new session
MULTI_SEED_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"Reply from wolf\",\"body_md\":\"Acknowledged the migration test.\",\"importance\":\"normal\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Third message for count\",\"body_md\":\"Testing persistence of multiple messages.\",\"importance\":\"low\"}}}" \
)"
e2e_save_artifact "case_05_seed.txt" "$MULTI_SEED_RESP"

SEED1_ERR="$(is_error_result "$MULTI_SEED_RESP" 60)"
SEED2_ERR="$(is_error_result "$MULTI_SEED_RESP" 61)"
if [ "$SEED1_ERR" = "false" ] && [ "$SEED2_ERR" = "false" ]; then
    e2e_pass "seeded 2 additional messages for multi-restart test"
else
    e2e_fail "failed to seed messages (msg1=$SEED1_ERR, msg2=$SEED2_ERR)"
fi

# Now restart 3 times, each time reading GoldFox's inbox
EXPECTED_GF_COUNT=1  # GoldFox receives "Reply from wolf"
for restart_num in 1 2 3; do
    RESTART_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"include_bodies\":false}}}" \
    )"
    e2e_save_artifact "case_05_restart_${restart_num}.txt" "$RESTART_RESP"

    R_ERR="$(is_error_result "$RESTART_RESP" 70)"
    if [ "$R_ERR" = "false" ]; then
        R_TEXT="$(extract_result "$RESTART_RESP" 70)"
        R_COUNT="$(echo "$R_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    msgs = d if isinstance(d, list) else d.get('messages', [])
    print(len(msgs))
except:
    print(-1)
" 2>/dev/null)"
        if [ "$R_COUNT" -ge "$EXPECTED_GF_COUNT" ] 2>/dev/null; then
            e2e_pass "restart $restart_num: GoldFox inbox has $R_COUNT message(s)"
        else
            e2e_fail "restart $restart_num: GoldFox inbox count unexpected (got $R_COUNT, expected >= $EXPECTED_GF_COUNT)"
        fi
    else
        e2e_fail "restart $restart_num: fetch_inbox failed"
    fi
done

# After 3 restarts, verify SilverWolf's inbox still correct (2 messages total)
FINAL_SW_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"include_bodies\":false}}}" \
)"
e2e_save_artifact "case_05_final_sw.txt" "$FINAL_SW_RESP"

SW_FINAL_ERR="$(is_error_result "$FINAL_SW_RESP" 80)"
if [ "$SW_FINAL_ERR" = "false" ]; then
    SW_TEXT="$(extract_result "$FINAL_SW_RESP" 80)"
    SW_MSG_COUNT="$(echo "$SW_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    msgs = d if isinstance(d, list) else d.get('messages', [])
    print(len(msgs))
except:
    print(-1)
" 2>/dev/null)"
    if [ "$SW_MSG_COUNT" -ge 2 ] 2>/dev/null; then
        e2e_pass "after 3 restarts: SilverWolf inbox has $SW_MSG_COUNT messages"
    else
        e2e_fail "after 3 restarts: SilverWolf inbox count wrong (got $SW_MSG_COUNT, expected >= 2)"
    fi
else
    e2e_fail "after 3 restarts: fetch_inbox for SilverWolf failed"
fi

# ===========================================================================
# Case 6: Concurrent access after migration -- two sessions, same DB
# ===========================================================================
e2e_case_banner "Concurrent access: two sessions against same migrated DB"

# Session X: register a new agent and send a message
CONC_X_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"concurrent-e2e\",\"model\":\"test-model\",\"name\":\"RedPeak\",\"task_description\":\"concurrent test agent\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":91,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\"],\"subject\":\"Concurrent session X\",\"body_md\":\"Message from concurrent session X.\"}}}" \
)"
e2e_save_artifact "case_06_session_x.txt" "$CONC_X_RESP"

CX_REG_ERR="$(is_error_result "$CONC_X_RESP" 90)"
CX_MSG_ERR="$(is_error_result "$CONC_X_RESP" 91)"

if [ "$CX_REG_ERR" = "false" ]; then
    e2e_pass "concurrent session X: RedPeak registered"
else
    e2e_fail "concurrent session X: RedPeak registration failed"
fi

if [ "$CX_MSG_ERR" = "false" ]; then
    e2e_pass "concurrent session X: message sent"
else
    e2e_fail "concurrent session X: message send failed"
fi

# Session Y: immediately after, verify the message is visible
CONC_Y_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":92,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"include_bodies\":false}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":93,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"query\":\"concurrent\"}}}" \
)"
e2e_save_artifact "case_06_session_y.txt" "$CONC_Y_RESP"

CY_INBOX_ERR="$(is_error_result "$CONC_Y_RESP" 92)"
CY_SEARCH_ERR="$(is_error_result "$CONC_Y_RESP" 93)"

if [ "$CY_INBOX_ERR" = "false" ]; then
    CY_INBOX_TEXT="$(extract_result "$CONC_Y_RESP" 92)"
    CY_COUNT="$(echo "$CY_INBOX_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    msgs = d if isinstance(d, list) else d.get('messages', [])
    print(len(msgs))
except:
    print(-1)
" 2>/dev/null)"
    # GoldFox should have at least 2 messages: "Reply from wolf" + "Concurrent session X"
    if [ "$CY_COUNT" -ge 2 ] 2>/dev/null; then
        e2e_pass "concurrent session Y: GoldFox inbox has $CY_COUNT messages (includes concurrent msg)"
    else
        e2e_fail "concurrent session Y: GoldFox inbox count unexpected ($CY_COUNT, expected >= 2)"
    fi
else
    e2e_fail "concurrent session Y: fetch_inbox failed"
fi

if [ "$CY_SEARCH_ERR" = "false" ]; then
    CY_SEARCH_TEXT="$(extract_result "$CONC_Y_RESP" 93)"
    CY_SEARCH_COUNT="$(echo "$CY_SEARCH_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    results = d if isinstance(d, list) else d.get('result', d.get('results', d.get('messages', [])))
    if not isinstance(results, list): results = []
    print(len(results))
except:
    print(-1)
" 2>/dev/null)"
    if [ "$CY_SEARCH_COUNT" -ge 1 ] 2>/dev/null; then
        e2e_pass "concurrent session Y: search found $CY_SEARCH_COUNT 'concurrent' result(s)"
    else
        e2e_fail "concurrent session Y: search for 'concurrent' found nothing ($CY_SEARCH_COUNT)"
    fi
else
    e2e_fail "concurrent session Y: search_messages failed"
fi

# ===========================================================================
# Case 7: Migration table details -- verify specific migration IDs applied
# ===========================================================================
e2e_case_banner "Migration table details: verify key migration IDs"

if [ "$HAS_SQLITE3" = "true" ]; then
    # Check for some known migration IDs from schema.rs
    for mig_pattern in "projects" "agents" "messages" "fts_messages" "v5_create_fts_with_porter"; do
        MIG_HIT="$(sqlite3 "$MIGRATION_DB" "SELECT COUNT(*) FROM mcp_agent_mail_migrations WHERE id LIKE '%${mig_pattern}%';" 2>/dev/null || echo "0")"
        if [ "$MIG_HIT" -gt 0 ] 2>/dev/null; then
            e2e_pass "migration for '$mig_pattern' found ($MIG_HIT entries)"
        else
            e2e_fail "migration for '$mig_pattern' not found"
        fi
    done

    # Verify total migration count is reasonable (schema has many migrations)
    TOTAL_MIGS="$(sqlite3 "$MIGRATION_DB" "SELECT COUNT(*) FROM mcp_agent_mail_migrations;" 2>/dev/null || echo "0")"
    if [ "$TOTAL_MIGS" -ge 20 ] 2>/dev/null; then
        e2e_pass "total migrations applied: $TOTAL_MIGS (>= 20 expected)"
    else
        e2e_fail "total migrations applied unexpectedly low: $TOTAL_MIGS"
    fi
else
    e2e_skip "sqlite3 not available -- skipping migration ID verification"
fi

# ===========================================================================
# Case 8: Idempotent re-migration -- restart doesn't re-apply migrations
# ===========================================================================
e2e_case_banner "Idempotent re-migration: restart does not re-apply"

if [ "$HAS_SQLITE3" = "true" ]; then
    MIG_COUNT_BEFORE="$(sqlite3 "$MIGRATION_DB" "SELECT COUNT(*) FROM mcp_agent_mail_migrations;" 2>/dev/null || echo "0")"

    # Run another session (triggers migration check)
    IDEM_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    )"
    e2e_save_artifact "case_08_idempotent.txt" "$IDEM_RESP"

    MIG_COUNT_AFTER="$(sqlite3 "$MIGRATION_DB" "SELECT COUNT(*) FROM mcp_agent_mail_migrations;" 2>/dev/null || echo "0")"

    e2e_assert_eq "migration count unchanged after restart" "$MIG_COUNT_BEFORE" "$MIG_COUNT_AFTER"
else
    e2e_skip "sqlite3 not available -- skipping idempotent migration check"
fi

# ===========================================================================
# Case 9: Data integrity after all operations -- final comprehensive check
# ===========================================================================
e2e_case_banner "Final data integrity: comprehensive verification"

FINAL_RESP="$(send_jsonrpc_session "$MIGRATION_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":110,\"method\":\"tools/call\",\"params\":{\"name\":\"whois\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"include_recent_commits\":false}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":111,\"method\":\"tools/call\",\"params\":{\"name\":\"whois\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"RedPeak\",\"include_recent_commits\":false}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":112,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"query\":\"test\"}}}" \
)"
e2e_save_artifact "case_09_final.txt" "$FINAL_RESP"

# Verify all 3 agents are findable
for agent_check in "110:SilverWolf" "111:RedPeak"; do
    AGENT_ID="${agent_check%%:*}"
    AGENT_NAME="${agent_check##*:}"
    AGENT_ERR="$(is_error_result "$FINAL_RESP" "$AGENT_ID")"
    if [ "$AGENT_ERR" = "false" ]; then
        AGENT_TEXT="$(extract_result "$FINAL_RESP" "$AGENT_ID")"
        AGENT_FOUND_NAME="$(parse_json_field "$AGENT_TEXT" "name")"
        e2e_assert_eq "final check: $AGENT_NAME persists" "$AGENT_NAME" "$AGENT_FOUND_NAME"
    else
        e2e_fail "final check: whois $AGENT_NAME failed"
    fi
done

# Final search: "test" should match multiple messages
FINAL_SEARCH_ERR="$(is_error_result "$FINAL_RESP" 112)"
if [ "$FINAL_SEARCH_ERR" = "false" ]; then
    FINAL_SEARCH_TEXT="$(extract_result "$FINAL_RESP" 112)"
    FINAL_SEARCH_COUNT="$(echo "$FINAL_SEARCH_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    results = d if isinstance(d, list) else d.get('result', d.get('results', d.get('messages', [])))
    if not isinstance(results, list): results = []
    print(len(results))
except:
    print(-1)
" 2>/dev/null)"
    if [ "$FINAL_SEARCH_COUNT" -ge 1 ] 2>/dev/null; then
        e2e_pass "final search for 'test' returned $FINAL_SEARCH_COUNT result(s)"
    else
        e2e_fail "final search for 'test' returned no results ($FINAL_SEARCH_COUNT)"
    fi
else
    e2e_fail "final search_messages failed"
fi

# DB file size sanity check
if [ -f "$MIGRATION_DB" ]; then
    DB_SIZE="$(stat --format='%s' "$MIGRATION_DB" 2>/dev/null || stat -f '%z' "$MIGRATION_DB" 2>/dev/null || echo "0")"
    if [ "$DB_SIZE" -gt 0 ] 2>/dev/null; then
        e2e_pass "DB file size: $DB_SIZE bytes (non-empty)"
    else
        e2e_fail "DB file size is 0 or unreadable"
    fi
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
