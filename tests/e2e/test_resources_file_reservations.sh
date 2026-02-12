#!/usr/bin/env bash
# test_resources_file_reservations.sh - E2E: File reservations MCP resource endpoint
#
# Verifies the resource://file_reservations/{slug} resource reading lifecycle
# through the MCP stdio transport.
#
# Tests:
#   1. Setup: ensure_project + register 2 agents (GoldFox, SilverWolf)
#   2. Create file reservations for both agents via file_reservation_paths tool
#   3. Read resource://file_reservations/{slug} - verify active reservations
#   4. Verify reservation fields: id, agent, path_pattern, exclusive, expires_ts, reason
#   5. Release one agent's reservations, re-read resource, verify count decreased
#   6. Release remaining reservations, verify empty list
#   7. Error case: file_reservations for nonexistent project

E2E_SUITE="resources_file_reservations"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "File Reservations Resources E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_res_file_reservations")"
FR_DB="${WORK}/file_reservations_test.sqlite3"
PROJECT_PATH="/tmp/e2e_file_reservations_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-resources-file-reservations","version":"1.0"}}}'

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

# Extract result text from a tools/call response (result.content[0].text)
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

# Extract text from a resources/read response (result.contents[0].text)
extract_resource_text() {
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
            # Resources use 'contents' (plural), not 'content'
            contents = d['result'].get('contents', [])
            if contents:
                print(contents[0].get('text', ''))
                sys.exit(0)
            # Fall back to 'content' just in case
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

# Compute the expected project slug from the project path
PROJECT_SLUG="$(echo "$PROJECT_PATH" | python3 -c "
import sys, re
raw = sys.stdin.read().strip()
s = raw.lower()
out = []
prev_dash = False
for ch in s:
    if ch.isalnum():
        out.append(ch)
        prev_dash = False
    elif not prev_dash:
        out.append('-')
        prev_dash = True
result = ''.join(out).strip('-')
print(result if result else 'project')
" 2>/dev/null)"

e2e_log "Project path: $PROJECT_PATH"
e2e_log "Project slug: $PROJECT_SLUG"

# ===========================================================================
# Case 1: Setup -- ensure_project + register 2 agents (GoldFox, SilverWolf)
# ===========================================================================
e2e_case_banner "Setup: project + two agents (GoldFox, SilverWolf)"

SETUP_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"file reservation resource E2E testing\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"file reservation resource E2E testing\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

EP_ERR="$(is_error_result "$SETUP_RESP" 10)"
GF_ERR="$(is_error_result "$SETUP_RESP" 11)"
SW_ERR="$(is_error_result "$SETUP_RESP" 12)"

if [ "$EP_ERR" = "false" ]; then
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
# Case 2: Create file reservations for both agents
# ===========================================================================
e2e_case_banner "file_reservation_paths: GoldFox reserves src/*.rs, SilverWolf reserves tests/*.rs"

RESERVE_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"paths\":[\"src/*.rs\",\"config/settings.yaml\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"refactoring modules\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\",\"paths\":[\"tests/*.rs\"],\"ttl_seconds\":7200,\"exclusive\":true,\"reason\":\"writing tests\"}}}" \
)"
e2e_save_artifact "case_02_reserve.txt" "$RESERVE_RESP"

GF_RES_ERR="$(is_error_result "$RESERVE_RESP" 20)"
SW_RES_ERR="$(is_error_result "$RESERVE_RESP" 21)"

if [ "$GF_RES_ERR" = "false" ]; then
    e2e_pass "GoldFox file_reservation_paths succeeded"
else
    e2e_fail "GoldFox file_reservation_paths returned error"
fi

if [ "$SW_RES_ERR" = "false" ]; then
    e2e_pass "SilverWolf file_reservation_paths succeeded"
else
    e2e_fail "SilverWolf file_reservation_paths returned error"
fi

# Verify granted counts
GF_RES_TEXT="$(extract_result "$RESERVE_RESP" 20)"
SW_RES_TEXT="$(extract_result "$RESERVE_RESP" 21)"
e2e_save_artifact "case_02_goldfox_reserve.txt" "$GF_RES_TEXT"
e2e_save_artifact "case_02_silverwolf_reserve.txt" "$SW_RES_TEXT"

GF_GRANTED="$(echo "$GF_RES_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(len(d.get('granted', [])))
except Exception:
    print('0')
" 2>/dev/null)"

SW_GRANTED="$(echo "$SW_RES_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(len(d.get('granted', [])))
except Exception:
    print('0')
" 2>/dev/null)"

e2e_assert_eq "GoldFox granted 2 reservations" "2" "$GF_GRANTED"
e2e_assert_eq "SilverWolf granted 1 reservation" "1" "$SW_GRANTED"

# ===========================================================================
# Case 3: Read resource://file_reservations/{slug} - verify active reservations
# ===========================================================================
e2e_case_banner "resource://file_reservations/${PROJECT_SLUG} - all active reservations"

FR_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://file_reservations/${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_03_file_reservations.txt" "$FR_RESP"

FR_ERR="$(is_error_result "$FR_RESP" 30)"
if [ "$FR_ERR" = "true" ]; then
    e2e_fail "resource://file_reservations returned error"
else
    e2e_pass "resource://file_reservations succeeded"
fi

FR_TEXT="$(extract_resource_text "$FR_RESP" 30)"
e2e_save_artifact "case_03_file_reservations_text.txt" "$FR_TEXT"

# Parse the reservations array
FR_CHECK="$(echo "$FR_TEXT" | python3 -c "
import sys, json
try:
    arr = json.loads(sys.stdin.read())
    if not isinstance(arr, list):
        print('PARSE_ERROR: expected array')
        sys.exit(0)
    count = len(arr)
    agents = sorted(set(r.get('agent', '') for r in arr))
    paths = sorted(r.get('path_pattern', '') for r in arr)
    print(f'count={count}')
    print(f'agents={agents}')
    print(f'paths={paths}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_03_parsed.txt" "$FR_CHECK"

e2e_assert_contains "file_reservations has 3 entries" "$FR_CHECK" "count=3"
e2e_assert_contains "file_reservations includes GoldFox" "$FR_CHECK" "GoldFox"
e2e_assert_contains "file_reservations includes SilverWolf" "$FR_CHECK" "SilverWolf"
e2e_assert_contains "file_reservations includes src/*.rs" "$FR_CHECK" "src/*.rs"
e2e_assert_contains "file_reservations includes config/settings.yaml" "$FR_CHECK" "config/settings.yaml"
e2e_assert_contains "file_reservations includes tests/*.rs" "$FR_CHECK" "tests/*.rs"

# ===========================================================================
# Case 4: Verify reservation fields have expected structure
# ===========================================================================
e2e_case_banner "Verify reservation entry fields"

FR_FIELDS="$(echo "$FR_TEXT" | python3 -c "
import sys, json
try:
    arr = json.loads(sys.stdin.read())
    if not isinstance(arr, list) or len(arr) == 0:
        print('PARSE_ERROR: empty array')
        sys.exit(0)
    # Check the first reservation entry for required fields
    entry = arr[0]
    has_id = 'id' in entry and isinstance(entry['id'], int)
    has_agent = 'agent' in entry and isinstance(entry['agent'], str) and len(entry['agent']) > 0
    has_path_pattern = 'path_pattern' in entry and isinstance(entry['path_pattern'], str) and len(entry['path_pattern']) > 0
    has_exclusive = 'exclusive' in entry and isinstance(entry['exclusive'], bool)
    has_expires_ts = 'expires_ts' in entry and entry['expires_ts'] is not None
    has_reason = 'reason' in entry and isinstance(entry['reason'], str)
    has_stale = 'stale' in entry and isinstance(entry['stale'], bool)
    has_stale_reasons = 'stale_reasons' in entry and isinstance(entry['stale_reasons'], list)
    print(f'has_id={has_id}')
    print(f'has_agent={has_agent}')
    print(f'has_path_pattern={has_path_pattern}')
    print(f'has_exclusive={has_exclusive}')
    print(f'has_expires_ts={has_expires_ts}')
    print(f'has_reason={has_reason}')
    print(f'has_stale={has_stale}')
    print(f'has_stale_reasons={has_stale_reasons}')
    # Check that the first entry's exclusive flag is True (we requested exclusive=true)
    print(f'exclusive_val={entry.get(\"exclusive\", None)}')
    # Check reason text
    reasons_found = [r.get('reason', '') for r in arr]
    print(f'reasons_found={reasons_found}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_04_fields.txt" "$FR_FIELDS"

e2e_assert_contains "entry has id field" "$FR_FIELDS" "has_id=True"
e2e_assert_contains "entry has agent field" "$FR_FIELDS" "has_agent=True"
e2e_assert_contains "entry has path_pattern field" "$FR_FIELDS" "has_path_pattern=True"
e2e_assert_contains "entry has exclusive field" "$FR_FIELDS" "has_exclusive=True"
e2e_assert_contains "entry has expires_ts field" "$FR_FIELDS" "has_expires_ts=True"
e2e_assert_contains "entry has reason field" "$FR_FIELDS" "has_reason=True"
e2e_assert_contains "entry has stale field" "$FR_FIELDS" "has_stale=True"
e2e_assert_contains "entry has stale_reasons field" "$FR_FIELDS" "has_stale_reasons=True"
e2e_assert_contains "exclusive is True" "$FR_FIELDS" "exclusive_val=True"
e2e_assert_contains "reason includes refactoring modules" "$FR_FIELDS" "refactoring modules"

# ===========================================================================
# Case 5: Release GoldFox's reservations, re-read resource, verify count decreased
# ===========================================================================
e2e_case_banner "Release GoldFox reservations, verify count drops to 1"

RELEASE_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://file_reservations/${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_05_release_and_read.txt" "$RELEASE_RESP"

RELEASE_ERR="$(is_error_result "$RELEASE_RESP" 50)"
if [ "$RELEASE_ERR" = "false" ]; then
    e2e_pass "release_file_reservations for GoldFox succeeded"
else
    e2e_fail "release_file_reservations for GoldFox returned error"
fi

# Verify release response
RELEASE_TEXT="$(extract_result "$RELEASE_RESP" 50)"
e2e_save_artifact "case_05_release_text.txt" "$RELEASE_TEXT"

RELEASE_COUNT="$(parse_json_field "$RELEASE_TEXT" "released")"
e2e_assert_eq "GoldFox released 2 reservations" "2" "$RELEASE_COUNT"

# Re-read resource and check count
FR_AFTER_TEXT="$(extract_resource_text "$RELEASE_RESP" 51)"
e2e_save_artifact "case_05_file_reservations_after.txt" "$FR_AFTER_TEXT"

FR_AFTER_CHECK="$(echo "$FR_AFTER_TEXT" | python3 -c "
import sys, json
try:
    arr = json.loads(sys.stdin.read())
    if not isinstance(arr, list):
        print('PARSE_ERROR: expected array')
        sys.exit(0)
    count = len(arr)
    agents = sorted(set(r.get('agent', '') for r in arr))
    paths = sorted(r.get('path_pattern', '') for r in arr)
    print(f'count={count}')
    print(f'agents={agents}')
    print(f'paths={paths}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_05_parsed.txt" "$FR_AFTER_CHECK"

e2e_assert_contains "after release: 1 reservation remains" "$FR_AFTER_CHECK" "count=1"
e2e_assert_contains "after release: only SilverWolf remains" "$FR_AFTER_CHECK" "agents=['SilverWolf']"
e2e_assert_contains "after release: only tests/*.rs remains" "$FR_AFTER_CHECK" "tests/*.rs"
e2e_assert_not_contains "after release: GoldFox gone" "$FR_AFTER_CHECK" "GoldFox"

# ===========================================================================
# Case 6: Release remaining reservations, verify empty list
# ===========================================================================
e2e_case_banner "Release SilverWolf reservations, verify empty list"

RELEASE2_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"SilverWolf\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://file_reservations/${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_06_release_all.txt" "$RELEASE2_RESP"

RELEASE2_ERR="$(is_error_result "$RELEASE2_RESP" 60)"
if [ "$RELEASE2_ERR" = "false" ]; then
    e2e_pass "release_file_reservations for SilverWolf succeeded"
else
    e2e_fail "release_file_reservations for SilverWolf returned error"
fi

RELEASE2_TEXT="$(extract_result "$RELEASE2_RESP" 60)"
RELEASE2_COUNT="$(parse_json_field "$RELEASE2_TEXT" "released")"
e2e_assert_eq "SilverWolf released 1 reservation" "1" "$RELEASE2_COUNT"

# Re-read resource and verify empty
FR_EMPTY_TEXT="$(extract_resource_text "$RELEASE2_RESP" 61)"
e2e_save_artifact "case_06_file_reservations_empty.txt" "$FR_EMPTY_TEXT"

FR_EMPTY_CHECK="$(echo "$FR_EMPTY_TEXT" | python3 -c "
import sys, json
try:
    arr = json.loads(sys.stdin.read())
    if not isinstance(arr, list):
        print('PARSE_ERROR: expected array')
        sys.exit(0)
    count = len(arr)
    print(f'count={count}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_06_parsed.txt" "$FR_EMPTY_CHECK"

e2e_assert_contains "all released: 0 reservations" "$FR_EMPTY_CHECK" "count=0"

# ===========================================================================
# Case 7: Error case -- file_reservations for nonexistent project
# ===========================================================================
e2e_case_banner "file_reservations for nonexistent project returns error"

NONEXIST_RESP="$(send_jsonrpc_session "$FR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":70,"method":"resources/read","params":{"uri":"resource://file_reservations/nonexistent-project-slug-xyz"}}' \
)"
e2e_save_artifact "case_07_nonexistent.txt" "$NONEXIST_RESP"

NONEXIST_ERR="$(is_error_result "$NONEXIST_RESP" 70)"
if [ "$NONEXIST_ERR" = "true" ]; then
    e2e_pass "file_reservations for nonexistent project correctly returned error"
else
    # Some implementations may return an empty array instead of an error.
    # Check if the response is an empty array, which is also acceptable.
    NONEXIST_TEXT="$(extract_resource_text "$NONEXIST_RESP" 70)"
    NONEXIST_CHECK="$(echo "$NONEXIST_TEXT" | python3 -c "
import sys, json
try:
    arr = json.loads(sys.stdin.read())
    if isinstance(arr, list) and len(arr) == 0:
        print('empty_array')
    else:
        print('unexpected')
except Exception:
    print('parse_error')
" 2>/dev/null)"
    if [ "$NONEXIST_CHECK" = "empty_array" ]; then
        e2e_pass "file_reservations for nonexistent project returned empty array (acceptable)"
    else
        e2e_fail "file_reservations for nonexistent project should have returned error or empty array"
    fi
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
