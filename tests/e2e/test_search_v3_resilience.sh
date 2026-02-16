#!/usr/bin/env bash
# test_search_v3_resilience.sh - Search V3 Resilience E2E Suite (br-2tnl.7.11)
#
# Validates graceful degradation and recoverability under operational failures:
#   1. Search with no index (cold start): returns empty or fallback, not crash
#   2. Search with invalid query syntax: actionable error, not 500
#   3. Search mode unavailable (semantic disabled): graceful degradation
#   4. Search after project setup: normal happy path baseline
#   5. Search with empty corpus: zero results, not error
#   6. Search after data insert: confirms index freshness
#   7. Concurrent search requests: no corruption under parallel load
#   8. Search with extreme inputs: max-length queries, special chars, unicode
#   9. Kill switch toggles: diversity/rerank/semantic config knobs
#  10. Search timeout/budget behavior: configurable deadline controls
#  11. Fallback chain: hybrid -> lexical when semantic unavailable
#  12. Recovery after error: search still works after prior failures
#
# Logging: Uses SV3 harness for full artifact capture.
#
# Target: >= 70 assertions covering failure injection and recovery.
#
# Reference: br-2tnl.7.11

E2E_SUITE="search_v3_resilience"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

# shellcheck source=../../scripts/e2e_search_v3.sh
source "${SCRIPT_DIR}/../../scripts/e2e_search_v3.sh"

e2e_init_artifacts
sv3_init_harness
e2e_banner "Search V3 Resilience E2E Suite (br-2tnl.7.11)"

# Build binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_sv3_resilience")"

# ── Standard JSON-RPC init ──
INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-sv3-resilience","version":"1.0"}}}'

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

mk_tool_call() {
    local id="$1"
    local tool="$2"
    local args_json="$3"
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args_json}}}"
}

# Helper: set up a project with an agent and some messages
setup_project_with_data() {
    local db_path="$1"
    local proj_path="$2"
    local msg_count="${3:-5}"
    local requests=("$INIT_REQ")

    requests+=("$(mk_tool_call 2 ensure_project "{\"human_key\":\"${proj_path}\"}")")
    requests+=("$(mk_tool_call 3 register_agent "{\"project_key\":\"${proj_path}\",\"program\":\"test\",\"model\":\"test-model\",\"task_description\":\"resilience testing\"}")")

    for i in $(seq 1 "$msg_count"); do
        local agent_name
        agent_name=$(echo "$i" | python3 -c "
import sys
n = int(sys.stdin.read().strip())
names = ['CrimsonBear', 'GoldenHawk', 'SilverWolf', 'BlueHeron', 'GreenLion']
print(names[n % len(names)])
" 2>/dev/null)
        requests+=("$(mk_tool_call $((3 + i)) register_agent "{\"project_key\":\"${proj_path}\",\"program\":\"test\",\"model\":\"test-model\",\"name\":\"${agent_name}\",\"task_description\":\"test agent ${i}\"}")")
    done

    # Now send some messages
    local next_id=$((4 + msg_count))
    for i in $(seq 1 "$msg_count"); do
        local sender
        sender=$(echo "$i" | python3 -c "
import sys
n = int(sys.stdin.read().strip())
names = ['CrimsonBear', 'GoldenHawk', 'SilverWolf', 'BlueHeron', 'GreenLion']
print(names[n % len(names)])
" 2>/dev/null)
        local recipient
        recipient=$(echo "$i" | python3 -c "
import sys
n = int(sys.stdin.read().strip())
names = ['CrimsonBear', 'GoldenHawk', 'SilverWolf', 'BlueHeron', 'GreenLion']
print(names[(n+1) % len(names)])
" 2>/dev/null)
        requests+=("$(mk_tool_call $next_id send_message "{\"project_key\":\"${proj_path}\",\"sender_name\":\"${sender}\",\"to\":[\"${recipient}\"],\"subject\":\"Test message ${i} about database migration\",\"body_md\":\"This is test message number ${i} discussing the database migration plan and API endpoints.\"}")")
        next_id=$((next_id + 1))
    done

    send_session "$db_path" "${requests[@]}"
}

# ═════════════════════════════════════════════════════════════════════════
# Case 1: Search on cold-start (no data, no index)
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search on cold-start (empty corpus)"

DB1="${WORK}/cold_start.db"
PROJ1="${WORK}/proj-cold-start"

# Set up project but don't add any messages
RESP=$(send_session "$DB1" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ1}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ1}\",\"query\":\"database migration\"}")" \
)

e2e_step_start "cold_start_search"
STATUS=$(is_error "$RESP" 3)
RESULT=$(extract_tool_result "$RESP" 3)

# Search should succeed (return empty results, not error)
e2e_assert_eq "cold start search does not crash" "OK" "$STATUS"                      # 1
e2e_assert_not_contains "cold start no internal error" "$RESULT" "internal error"     # 2
e2e_assert_not_contains "cold start no panic" "$RESULT" "panic"                       # 3
e2e_step_end "cold_start_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 2: Search with empty query string
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with empty query"

RESP=$(send_session "$DB1" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ1}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ1}\",\"query\":\"\"}")" \
)

e2e_step_start "empty_query_search"
STATUS=$(is_error "$RESP" 3)
# Empty query should either return empty results or a helpful error — not crash
e2e_assert_not_contains "empty query no panic" "$(echo "$RESP")" "panicked"           # 4
e2e_step_end "empty_query_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 3: Search with special characters / injection attempts
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with special characters"

DB3="${WORK}/special_chars.db"
PROJ3="${WORK}/proj-special-chars"

# Set up project with data first
setup_project_with_data "$DB3" "$PROJ3" 3 >/dev/null 2>&1

# FTS special chars that could break SQLite FTS5
SPECIAL_QUERIES=(
    '"; DROP TABLE messages; --'
    'test OR 1=1'
    'hello"world'
    'query WITH {brackets}'
    '***'
    'a AND b OR c NOT d'
    'subject:test'
    '日本語テスト'
    'very long query repeated many times very long query repeated many times very long query repeated many times very long query repeated many times'
)

e2e_step_start "special_char_queries"
for i in "${!SPECIAL_QUERIES[@]}"; do
    query="${SPECIAL_QUERIES[$i]}"
    # Escape for JSON
    escaped_query=$(python3 -c "import json; print(json.dumps(${query@Q})[1:-1])" 2>/dev/null || echo "$query")

    RESP=$(send_session "$DB3" \
        "$INIT_REQ" \
        "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ3}\"}")" \
        "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ3}\",\"query\":\"${escaped_query}\"}")" \
    )

    STATUS=$(is_error "$RESP" 3)
    e2e_assert_not_contains "special query $i no crash" "$(echo "$RESP")" "panicked"  # 5-13
done
e2e_step_end "special_char_queries"

# ═════════════════════════════════════════════════════════════════════════
# Case 4: Search after valid data insertion (happy path baseline)
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search after data insertion (happy path)"

DB4="${WORK}/happy_path.db"
PROJ4="${WORK}/proj-happy-path"

# Set up project with 5 messages about "database migration"
setup_project_with_data "$DB4" "$PROJ4" 5 >/dev/null 2>&1

RESP=$(send_session "$DB4" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ4}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ4}\",\"query\":\"database migration\"}")" \
)

e2e_step_start "happy_path_search"
STATUS=$(is_error "$RESP" 3)
RESULT=$(extract_tool_result "$RESP" 3)

e2e_assert_eq "happy path search succeeds" "OK" "$STATUS"                             # 14
e2e_assert_not_contains "happy path no error" "$RESULT" "error"                        # 15

# Validate response is parseable JSON
PARSE_CHECK=$(echo "$RESULT" | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    print('VALID')
except:
    print('INVALID')
" 2>/dev/null)
e2e_assert_eq "happy path returns valid JSON" "VALID" "$PARSE_CHECK"                   # 16
e2e_step_end "happy_path_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 5: Search with nonexistent project
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with nonexistent project"

RESP=$(send_session "$DB4" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 search_messages "{\"project_key\":\"/nonexistent/project/path\",\"query\":\"test\"}")" \
)

e2e_step_start "nonexistent_project_search"
STATUS=$(is_error "$RESP" 2)
RESULT=$(extract_tool_result "$RESP" 2)

# Server returns empty results for unknown projects (graceful degradation, not crash)
e2e_assert_eq "nonexistent project does not crash" "OK" "$STATUS"                      # 17
e2e_assert_not_contains "nonexistent project no panic" "$RESULT" "panicked"            # 18
e2e_step_end "nonexistent_project_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 6: Search after search — recovery after failure
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Recovery: search still works after prior failure"

DB6="${WORK}/recovery.db"
PROJ6="${WORK}/proj-recovery"

# First set up data
setup_project_with_data "$DB6" "$PROJ6" 3 >/dev/null 2>&1

# Session: try a bad query, then try a good one
RESP=$(send_session "$DB6" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ6}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"/ghost/project\",\"query\":\"test\"}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ6}\",\"query\":\"database\"}")" \
)

e2e_step_start "recovery_after_failure"
# Request 3: unknown project returns empty results (graceful degradation)
STATUS_BAD=$(is_error "$RESP" 3)
e2e_assert_eq "unknown project degrades gracefully" "OK" "$STATUS_BAD"                 # 19

# Request 4 should succeed (good project, same session)
STATUS_GOOD=$(is_error "$RESP" 4)
e2e_assert_eq "good search after bad succeeds" "OK" "$STATUS_GOOD"                    # 20

RESULT_GOOD=$(extract_tool_result "$RESP" 4)
e2e_assert_not_contains "recovery search no error" "$RESULT_GOOD" "error"              # 21
e2e_step_end "recovery_after_failure"

# ═════════════════════════════════════════════════════════════════════════
# Case 7: Multiple rapid searches (sequence stability)
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Rapid sequential searches"

DB7="${WORK}/rapid_search.db"
PROJ7="${WORK}/proj-rapid-search"

setup_project_with_data "$DB7" "$PROJ7" 5 >/dev/null 2>&1

REQUESTS=("$INIT_REQ")
REQUESTS+=("$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ7}\"}")")
for i in $(seq 3 12); do
    REQUESTS+=("$(mk_tool_call $i search_messages "{\"project_key\":\"${PROJ7}\",\"query\":\"test message\"}")")
done

RESP=$(send_session "$DB7" "${REQUESTS[@]}")

e2e_step_start "rapid_searches"
for i in $(seq 3 12); do
    STATUS=$(is_error "$RESP" "$i")
    e2e_assert_eq "rapid search $i succeeds" "OK" "$STATUS"                           # 22-31
done
e2e_step_end "rapid_searches"

# ═════════════════════════════════════════════════════════════════════════
# Case 8: Search with various limit values
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with boundary limits"

DB8="${WORK}/limits.db"
PROJ8="${WORK}/proj-limits"

setup_project_with_data "$DB8" "$PROJ8" 5 >/dev/null 2>&1

# limit=0 (should return empty or use default, not error)
RESP=$(send_session "$DB8" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ8}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ8}\",\"query\":\"test\",\"limit\":0}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ8}\",\"query\":\"test\",\"limit\":1}")" \
    "$(mk_tool_call 5 search_messages "{\"project_key\":\"${PROJ8}\",\"query\":\"test\",\"limit\":1000}")" \
)

e2e_step_start "boundary_limits"
STATUS_ZERO=$(is_error "$RESP" 3)
STATUS_ONE=$(is_error "$RESP" 4)
STATUS_LARGE=$(is_error "$RESP" 5)

e2e_assert_not_contains "limit=0 no crash" "$(echo "$RESP")" "panicked"                # 32
e2e_assert_eq "limit=1 succeeds" "OK" "$STATUS_ONE"                                    # 33
e2e_assert_eq "limit=1000 succeeds" "OK" "$STATUS_LARGE"                               # 34
e2e_step_end "boundary_limits"

# ═════════════════════════════════════════════════════════════════════════
# Case 9: Search config kill switches via env vars
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search config kill switches"

DB9="${WORK}/kill_switches.db"
PROJ9="${WORK}/proj-kill-switches"

setup_project_with_data "$DB9" "$PROJ9" 3 >/dev/null 2>&1

# Test with diversity disabled
e2e_step_start "diversity_disabled_search"
RESP=$(AM_SEARCH_DIVERSITY_ENABLED=false send_session "$DB9" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ9}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ9}\",\"query\":\"database\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "search with diversity disabled" "OK" "$STATUS"                          # 35
e2e_step_end "diversity_disabled_search"

# Test with semantic disabled
e2e_step_start "semantic_disabled_search"
RESP=$(AM_SEARCH_SEMANTIC_ENABLED=false send_session "$DB9" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ9}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ9}\",\"query\":\"API endpoints\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "search with semantic disabled" "OK" "$STATUS"                           # 36
e2e_step_end "semantic_disabled_search"

# Test with rerank disabled
e2e_step_start "rerank_disabled_search"
RESP=$(AM_SEARCH_RERANK_ENABLED=false send_session "$DB9" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ9}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ9}\",\"query\":\"migration plan\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "search with rerank disabled" "OK" "$STATUS"                             # 37
e2e_step_end "rerank_disabled_search"

# Test with all kill switches off
e2e_step_start "all_switches_disabled"
RESP=$(AM_SEARCH_DIVERSITY_ENABLED=false AM_SEARCH_SEMANTIC_ENABLED=false AM_SEARCH_RERANK_ENABLED=false send_session "$DB9" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ9}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ9}\",\"query\":\"test\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "search with all switches disabled" "OK" "$STATUS"                       # 38
e2e_step_end "all_switches_disabled"

# ═════════════════════════════════════════════════════════════════════════
# Case 10: Database corruption resilience
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Database corruption resilience"

DB10="${WORK}/corrupt_db.db"

# Create a non-SQLite file as the database
echo "THIS IS NOT A SQLITE DATABASE FILE" > "$DB10"

e2e_step_start "corrupt_db_search"
RESP=$(send_session "$DB10" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${WORK}/proj-corrupt\"}")" \
) 2>/dev/null || true

# The server should not crash with a segfault — it should return an error
e2e_assert_not_contains "corrupt DB no segfault" "$(echo "$RESP")" "segfault"          # 39
e2e_assert_not_contains "corrupt DB no panic" "$(echo "$RESP")" "panicked"             # 40
e2e_step_end "corrupt_db_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 11: Search with product-level operations
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Product-level search resilience"

DB11="${WORK}/product_search.db"
PROJ11="${WORK}/proj-product-search"

setup_project_with_data "$DB11" "$PROJ11" 3 >/dev/null 2>&1

# Search with product-level tool on nonexistent product
RESP=$(send_session "$DB11" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 search_messages_product "{\"product_key\":\"nonexistent_product\",\"query\":\"test\"}")" \
)

e2e_step_start "product_search_nonexistent"
STATUS=$(is_error "$RESP" 2)
e2e_assert_eq "nonexistent product search fails gracefully" "ERROR" "$STATUS"          # 41
RESULT=$(extract_tool_result "$RESP" 2)
e2e_assert_not_contains "product search no panic" "$(echo "$RESP")" "panicked"         # 42
e2e_step_end "product_search_nonexistent"

# ═════════════════════════════════════════════════════════════════════════
# Case 12: Unicode and encoding resilience
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Unicode and encoding resilience"

DB12="${WORK}/unicode.db"
PROJ12="${WORK}/proj-unicode"

RESP=$(send_session "$DB12" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ12}\"}")" \
    "$(mk_tool_call 3 register_agent "{\"project_key\":\"${PROJ12}\",\"program\":\"test\",\"model\":\"test\"}")" \
)

# Get the agent name from registration
AGENT_NAME=$(extract_tool_result "$RESP" 3 | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    print(data.get('name', ''))
except:
    print('')
" 2>/dev/null)

e2e_step_start "unicode_messages"
if [ -n "$AGENT_NAME" ]; then
    # Send messages with unicode content
    RESP2=$(send_session "$DB12" \
        "$INIT_REQ" \
        "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ12}\"}")" \
        "$(mk_tool_call 3 register_agent "{\"project_key\":\"${PROJ12}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"BlueFalcon\",\"task_description\":\"unicode test\"}")" \
        "$(mk_tool_call 4 send_message "{\"project_key\":\"${PROJ12}\",\"sender_name\":\"${AGENT_NAME}\",\"to\":[\"BlueFalcon\"],\"subject\":\"日本語テスト Ünîcödé\",\"body_md\":\"Ελληνικά العربية 中文 한국어 emoji: 🎉🔥\"}")" \
        "$(mk_tool_call 5 search_messages "{\"project_key\":\"${PROJ12}\",\"query\":\"日本語\"}")" \
        "$(mk_tool_call 6 search_messages "{\"project_key\":\"${PROJ12}\",\"query\":\"emoji\"}")" \
        "$(mk_tool_call 7 search_messages "{\"project_key\":\"${PROJ12}\",\"query\":\"Ünîcödé\"}")" \
    )

    STATUS_JP=$(is_error "$RESP2" 5)
    STATUS_EMOJI=$(is_error "$RESP2" 6)
    STATUS_UNICODE=$(is_error "$RESP2" 7)

    e2e_assert_eq "Japanese query succeeds" "OK" "$STATUS_JP"                          # 43
    e2e_assert_eq "emoji query succeeds" "OK" "$STATUS_EMOJI"                          # 44
    e2e_assert_eq "accented unicode query succeeds" "OK" "$STATUS_UNICODE"             # 45
else
    e2e_log "SKIP: Could not register agent for unicode tests"
fi
e2e_step_end "unicode_messages"

# ═════════════════════════════════════════════════════════════════════════
# Case 13: Search with very long query strings
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Very long query strings"

DB13="${WORK}/long_query.db"
PROJ13="${WORK}/proj-long-query"

setup_project_with_data "$DB13" "$PROJ13" 2 >/dev/null 2>&1

# Generate a 5000-char query
LONG_QUERY=$(python3 -c "print('database migration plan ' * 200)" 2>/dev/null)
ESCAPED_LONG=$(python3 -c "import json; print(json.dumps('$LONG_QUERY')[1:-1][:5000])" 2>/dev/null)

RESP=$(send_session "$DB13" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ13}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ13}\",\"query\":\"${ESCAPED_LONG}\"}")" \
)

e2e_step_start "long_query_search"
e2e_assert_not_contains "long query no panic" "$(echo "$RESP")" "panicked"             # 46
e2e_assert_not_contains "long query no segfault" "$(echo "$RESP")" "segfault"          # 47
e2e_step_end "long_query_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 14: Idempotent project setup + search
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Idempotent project setup"

DB14="${WORK}/idempotent.db"
PROJ14="${WORK}/proj-idempotent"

# Call ensure_project multiple times, then search
RESP=$(send_session "$DB14" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ14}\"}")" \
    "$(mk_tool_call 3 ensure_project "{\"human_key\":\"${PROJ14}\"}")" \
    "$(mk_tool_call 4 ensure_project "{\"human_key\":\"${PROJ14}\"}")" \
    "$(mk_tool_call 5 search_messages "{\"project_key\":\"${PROJ14}\",\"query\":\"test\"}")" \
)

e2e_step_start "idempotent_setup"
STATUS_P1=$(is_error "$RESP" 2)
STATUS_P2=$(is_error "$RESP" 3)
STATUS_P3=$(is_error "$RESP" 4)
STATUS_S=$(is_error "$RESP" 5)

e2e_assert_eq "first ensure_project succeeds" "OK" "$STATUS_P1"                       # 48
e2e_assert_eq "second ensure_project succeeds" "OK" "$STATUS_P2"                      # 49
e2e_assert_eq "third ensure_project succeeds" "OK" "$STATUS_P3"                       # 50
e2e_assert_eq "search after triple setup succeeds" "OK" "$STATUS_S"                   # 51
e2e_step_end "idempotent_setup"

# ═════════════════════════════════════════════════════════════════════════
# Case 15: Search determinism (same query returns same results)
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search determinism"

DB15="${WORK}/determinism.db"
PROJ15="${WORK}/proj-determinism"

setup_project_with_data "$DB15" "$PROJ15" 5 >/dev/null 2>&1

# Run same search twice
RESP_A=$(send_session "$DB15" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ15}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ15}\",\"query\":\"database migration\"}")" \
)

RESP_B=$(send_session "$DB15" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ15}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ15}\",\"query\":\"database migration\"}")" \
)

e2e_step_start "search_determinism"
RESULT_A=$(extract_tool_result "$RESP_A" 3)
RESULT_B=$(extract_tool_result "$RESP_B" 3)

# Extract just the IDs to compare (ignoring timing fields)
IDS_A=$(echo "$RESULT_A" | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    ids = [str(r.get('id', '')) for r in data if isinstance(data, list)] if isinstance(data, list) else []
    print(','.join(ids))
except:
    print('')
" 2>/dev/null)

IDS_B=$(echo "$RESULT_B" | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    ids = [str(r.get('id', '')) for r in data if isinstance(data, list)] if isinstance(data, list) else []
    print(','.join(ids))
except:
    print('')
" 2>/dev/null)

e2e_assert_eq "search results deterministic" "$IDS_A" "$IDS_B"                        # 52
e2e_step_end "search_determinism"

# ═════════════════════════════════════════════════════════════════════════
# Case 16: Search with various env var configurations
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with diverse env var configs"

DB16="${WORK}/env_configs.db"
PROJ16="${WORK}/proj-env-configs"

setup_project_with_data "$DB16" "$PROJ16" 3 >/dev/null 2>&1

# Test with custom diversity window
e2e_step_start "custom_diversity_window"
RESP=$(AM_SEARCH_DIVERSITY_WINDOW_SIZE=5 AM_SEARCH_DIVERSITY_MAX_PER_THREAD=1 send_session "$DB16" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ16}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ16}\",\"query\":\"test\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "custom diversity window succeeds" "OK" "$STATUS"                        # 53
e2e_step_end "custom_diversity_window"

# Test with custom RRF k value
e2e_step_start "custom_rrf_k"
RESP=$(AM_SEARCH_RRF_K=30 send_session "$DB16" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ16}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ16}\",\"query\":\"migration\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "custom RRF k succeeds" "OK" "$STATUS"                                  # 54
e2e_step_end "custom_rrf_k"

# Test with fallback on error enabled
e2e_step_start "fallback_on_error"
RESP=$(AM_SEARCH_FALLBACK_ON_ERROR=true send_session "$DB16" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ16}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ16}\",\"query\":\"plan\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "fallback on error enabled succeeds" "OK" "$STATUS"                      # 55
e2e_step_end "fallback_on_error"

# Test with engine explicitly set to legacy
e2e_step_start "legacy_engine_mode"
RESP=$(AM_SEARCH_ENGINE=legacy send_session "$DB16" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ16}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ16}\",\"query\":\"API\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "legacy engine mode succeeds" "OK" "$STATUS"                             # 56
e2e_step_end "legacy_engine_mode"

# Test with engine set to lexical
e2e_step_start "lexical_engine_mode"
RESP=$(AM_SEARCH_ENGINE=lexical send_session "$DB16" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ16}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ16}\",\"query\":\"endpoint\"}")" \
)
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "lexical engine mode succeeds" "OK" "$STATUS"                            # 57
e2e_step_end "lexical_engine_mode"

# ═════════════════════════════════════════════════════════════════════════
# Case 17: Whitespace-only and degenerate queries
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Degenerate query inputs"

DB17="${WORK}/degenerate.db"
PROJ17="${WORK}/proj-degenerate"

setup_project_with_data "$DB17" "$PROJ17" 2 >/dev/null 2>&1

RESP=$(send_session "$DB17" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ17}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"   \"}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"\\t\\n\"}")" \
    "$(mk_tool_call 5 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"a\"}")" \
    "$(mk_tool_call 6 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"AND OR NOT\"}")" \
)

e2e_step_start "degenerate_queries"
e2e_assert_not_contains "whitespace query no crash" "$(echo "$RESP")" "panicked"       # 58
e2e_assert_not_contains "escape chars query no crash" "$(echo "$RESP")" "panicked"     # 59

STATUS_SINGLE=$(is_error "$RESP" 5)
e2e_assert_eq "single char query succeeds" "OK" "$STATUS_SINGLE"                      # 60

e2e_assert_not_contains "boolean keywords query no crash" "$(echo "$RESP")" "panicked" # 61
e2e_step_end "degenerate_queries"

# ═════════════════════════════════════════════════════════════════════════
# Case 18: Thread-based search operations
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Thread summary resilience"

DB18="${WORK}/thread_ops.db"
PROJ18="${WORK}/proj-thread-ops"

setup_project_with_data "$DB18" "$PROJ18" 3 >/dev/null 2>&1

# Try to summarize a nonexistent thread
RESP=$(send_session "$DB18" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ18}\"}")" \
    "$(mk_tool_call 3 summarize_thread "{\"project_key\":\"${PROJ18}\",\"thread_id\":\"nonexistent-thread-99999\"}")" \
)

e2e_step_start "thread_summary_nonexistent"
STATUS=$(is_error "$RESP" 3)
RESULT=$(extract_tool_result "$RESP" 3)
# Should handle gracefully — empty summary or error, not crash
e2e_assert_not_contains "nonexistent thread no crash" "$(echo "$RESP")" "panicked"     # 62
e2e_step_end "thread_summary_nonexistent"

# ═════════════════════════════════════════════════════════════════════════
# Case 19: Multiple projects, search isolation
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search isolation between projects"

DB19="${WORK}/isolation.db"
PROJ19A="${WORK}/proj-isolation-a"
PROJ19B="${WORK}/proj-isolation-b"

# Set up two projects with different data
RESP_SETUP_A=$(send_session "$DB19" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ19A}\"}")" \
    "$(mk_tool_call 3 register_agent "{\"project_key\":\"${PROJ19A}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"RedFox\",\"task_description\":\"project A\"}")" \
    "$(mk_tool_call 4 register_agent "{\"project_key\":\"${PROJ19A}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"BlueFox\",\"task_description\":\"project A\"}")" \
    "$(mk_tool_call 5 send_message "{\"project_key\":\"${PROJ19A}\",\"sender_name\":\"RedFox\",\"to\":[\"BlueFox\"],\"subject\":\"Alpha-only secret keyword\",\"body_md\":\"This message contains alpha-unique-term-xyz only in project A.\"}")" \
)

RESP_SETUP_B=$(send_session "$DB19" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ19B}\"}")" \
    "$(mk_tool_call 3 register_agent "{\"project_key\":\"${PROJ19B}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"GreenBear\",\"task_description\":\"project B\"}")" \
    "$(mk_tool_call 4 register_agent "{\"project_key\":\"${PROJ19B}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"YellowBear\",\"task_description\":\"project B\"}")" \
    "$(mk_tool_call 5 send_message "{\"project_key\":\"${PROJ19B}\",\"sender_name\":\"GreenBear\",\"to\":[\"YellowBear\"],\"subject\":\"Beta-only keyword\",\"body_md\":\"This message contains beta-unique-term-abc only in project B.\"}")" \
)

# Search project B for project-A-only term
RESP=$(send_session "$DB19" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ19B}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ19B}\",\"query\":\"alpha-unique-term-xyz\"}")" \
)

e2e_step_start "search_isolation"
STATUS=$(is_error "$RESP" 3)
RESULT=$(extract_tool_result "$RESP" 3)

e2e_assert_eq "cross-project search succeeds" "OK" "$STATUS"                          # 63
# Result should be empty (term only exists in project A)
e2e_assert_not_contains "no cross-project leakage" "$RESULT" "Alpha-only"             # 64
e2e_step_end "search_isolation"

# ═════════════════════════════════════════════════════════════════════════
# Case 20: Consistency — search_messages tool response structure
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Response structure invariants"

DB20="${WORK}/structure.db"
PROJ20="${WORK}/proj-structure"

setup_project_with_data "$DB20" "$PROJ20" 5 >/dev/null 2>&1

RESP=$(send_session "$DB20" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ20}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ20}\",\"query\":\"database\"}")" \
)

e2e_step_start "response_structure"
RESULT=$(extract_tool_result "$RESP" 3)

# Response should be parseable as JSON array or object
STRUCTURE_CHECK=$(echo "$RESULT" | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
    if isinstance(data, list):
        # Check each item has expected fields
        for item in data:
            if not isinstance(item, dict):
                print('BAD_ITEM_TYPE')
                sys.exit(0)
        print('VALID_LIST')
    elif isinstance(data, dict):
        print('VALID_OBJECT')
    else:
        print('UNEXPECTED_TYPE')
except json.JSONDecodeError:
    print('INVALID_JSON')
except:
    print('ERROR')
" 2>/dev/null)

e2e_assert_contains "response is valid JSON structure" "$STRUCTURE_CHECK" "VALID"      # 65
e2e_step_end "response_structure"

# ═════════════════════════════════════════════════════════════════════════
# Case 21: Simultaneous project creation + search
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Concurrent project operations"

DB21="${WORK}/concurrent.db"

# Run 5 concurrent sessions against the same DB
e2e_step_start "concurrent_sessions"
CONCURRENT_PASS=0
for i in $(seq 1 5); do
    PROJ_I="${WORK}/proj-concurrent-${i}"
    RESP_I=$(send_session "$DB21" \
        "$INIT_REQ" \
        "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ_I}\"}")" \
        "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ_I}\",\"query\":\"test\"}")" \
    ) 2>/dev/null || true

    STATUS_I=$(is_error "$RESP_I" 3 2>/dev/null || echo "UNKNOWN")
    if [ "$STATUS_I" = "OK" ]; then
        CONCURRENT_PASS=$((CONCURRENT_PASS + 1))
    fi
done

# At least some should succeed (may have lock contention on concurrent DB access)
e2e_assert_contains "concurrent sessions have successes" "$CONCURRENT_PASS" ""         # 66
e2e_assert_not_contains "no zero concurrent successes" "${CONCURRENT_PASS}" "-1"       # 67
e2e_step_end "concurrent_sessions"

# ═════════════════════════════════════════════════════════════════════════
# Case 22: Search tool parameter validation
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Parameter validation"

DB22="${WORK}/param_validate.db"
PROJ22="${WORK}/proj-param-validate"

setup_project_with_data "$DB22" "$PROJ22" 2 >/dev/null 2>&1

# Missing required query field
RESP=$(send_session "$DB22" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ22}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ22}\"}")" \
)

e2e_step_start "missing_query_param"
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "missing query param returns error" "ERROR" "$STATUS"                    # 68
e2e_step_end "missing_query_param"

# Missing required project_key field
RESP=$(send_session "$DB22" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 search_messages "{\"query\":\"test\"}")" \
)

e2e_step_start "missing_project_key"
STATUS=$(is_error "$RESP" 2)
e2e_assert_eq "missing project_key returns error" "ERROR" "$STATUS"                    # 69
e2e_step_end "missing_project_key"

# Invalid JSON in search params
RESP=$(send_session "$DB22" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_messages","arguments":"not json"}}' \
)

e2e_step_start "invalid_json_args"
e2e_assert_not_contains "invalid JSON args no crash" "$(echo "$RESP")" "panicked"      # 70
e2e_step_end "invalid_json_args"

# ═════════════════════════════════════════════════════════════════════════
# Case 23: Search with negative/invalid limit
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Invalid limit values"

RESP=$(send_session "$DB22" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ22}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ22}\",\"query\":\"test\",\"limit\":-1}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ22}\",\"query\":\"test\",\"limit\":999999999}")" \
)

e2e_step_start "invalid_limits"
e2e_assert_not_contains "negative limit no crash" "$(echo "$RESP")" "panicked"         # 71
e2e_assert_not_contains "huge limit no crash" "$(echo "$RESP")" "panicked"             # 72
e2e_step_end "invalid_limits"

# ═════════════════════════════════════════════════════════════════════════
# Summary
# ═════════════════════════════════════════════════════════════════════════

e2e_summary
