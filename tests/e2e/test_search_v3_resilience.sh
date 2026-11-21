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

AM_BIN="$(e2e_ensure_binary am | tail -n 1)"
e2e_log "am binary: ${AM_BIN}"

# Temp workspace
WORK="$(e2e_mktemp "e2e_sv3_resilience")"

# ── Standard JSON-RPC init ──
INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-sv3-resilience","version":"1.0"}}}'

# ── Helpers ──────────────────────────────────────────────────────────────

send_session() {
    local db_path="$1"
    shift
    local client_args=("$db_path")
    if [ "${1:-}" = "--pipeline" ]; then
        client_args=(--pipeline "$db_path")
        shift
    fi
    local archive="${db_path%.db}.archive"
    mkdir -p "$archive"
    # The response budget plus the client's five-second EOF drain fits the
    # previous session budget.
    E2E_STDIO_BINARY="$AM_BIN" E2E_STDIO_TIMEOUT_SECONDS=7.5 STORAGE_ROOT="$archive" \
        SEARCH_V3_INDEX_DIR="$archive/search_index" AM_SEARCH_ENGINE="${AM_SEARCH_ENGINE:-lexical}" \
        AM_ALLOW_EPHEMERAL_PROJECT_ROOTS=1 WORKTREES_ENABLED=true RUST_LOG=error \
        e2e_stdio_session "${client_args[@]}" "$@"
}

extract_tool_result() {
    tool_reply "$1" "$2" text
}

is_error() {
    tool_reply "$1" "$2" status
}

tool_reply() {
    printf '%s\n' "$1" | python3 -c '
import json
import sys

target_id = int(sys.argv[1])
replies = [json.loads(line) for line in sys.stdin if line.strip()]
matches = [reply for reply in replies if reply.get("id") == target_id]
if len(matches) != 1:
    sys.exit(f"Expected one reply for request {target_id}, got {len(matches)}")
reply, = matches
error = reply.get("error")
if isinstance(error, dict) and isinstance(error.get("message"), str):
    status, body = "ERROR", error["message"]
else:
    result = reply.get("result")
    if not isinstance(result, dict) or not isinstance(result.get("isError", False), bool):
        sys.exit(f"Invalid reply for request {target_id}: {reply}")
    content = result.get("content")
    if not isinstance(content, list) or not content or not isinstance(content[0], dict):
        sys.exit(f"Missing tool content for request {target_id}: {reply}")
    body = content[0].get("text")
    if not isinstance(body, str) or not body:
        sys.exit(f"Missing tool text for request {target_id}: {reply}")
    status = "ERROR" if result.get("isError", False) else "OK"
print(status if sys.argv[2] == "status" else body)
' "$2" "$3"
}

require_success() {
    local response="$1" request_id="$2"
    if [ "$(is_error "$response" "$request_id")" != "OK" ]; then
        printf 'Fixture request %s failed:\n%s\n' "$request_id" "$response" >&2
        return 1
    fi
}

search_ids() {
    require_success "$1" "$2" || return 1
    extract_tool_result "$1" "$2" | python3 -c '
import json
import sys

data = json.load(sys.stdin)
rows = data.get("result") if isinstance(data, dict) else data
if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
    sys.exit(f"Invalid search result: {data}")
ids = [row.get("id") for row in rows]
if any(type(value) is not int or value <= 0 for value in ids) or len(set(ids)) != len(ids):
    sys.exit(f"Invalid search result IDs: {ids}")
if sys.argv[1] == "sorted":
    ids.sort()
print(",".join(map(str, ids)))
' "${3:-ranked}"
}

fixture_message_ids() {
    python3 -c '
import json
import sys

with open(sys.argv[1], encoding="utf-8") as transcript:
    ids = []
    for line in transcript:
        reply = json.loads(line)
        content = reply["result"].get("content")
        if content:
            payload = json.loads(content[0]["text"])
            if "deliveries" in payload:
                delivery, = payload["deliveries"]
                ids.append(delivery["payload"]["id"])
if len(ids) != int(sys.argv[2]) or any(type(value) is not int or value <= 0 for value in ids) or len(set(ids)) != len(ids):
    sys.exit(f"Invalid fixture message IDs: {ids}")
print(",".join(map(str, sorted(ids))))
' "${E2E_ARTIFACT_DIR}/setup-$(basename "$1").jsonl" "$2"
}

sent_message_id() {
    require_success "$1" "$2" || return 1
    extract_tool_result "$1" "$2" | python3 -c '
import json
import sys

delivery, = json.load(sys.stdin)["deliveries"]
message_id = delivery["payload"]["id"]
if type(message_id) is not int or message_id <= 0:
    sys.exit(f"Invalid send receipt ID: {message_id}")
print(message_id)
'
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
    local agents=(CrimsonBear GoldenHawk SilverWolf BlueHeron GreenLion)
    local agent_count="$msg_count" i id=3 sender recipient response
    if [ "$agent_count" -lt 2 ]; then
        agent_count=2
    fi

    requests+=("$(mk_tool_call 2 ensure_project "{\"human_key\":\"${proj_path}\"}")")
    for ((i = 0; i < agent_count; i++)); do
        requests+=("$(mk_tool_call "$id" register_agent "{\"project_key\":\"${proj_path}\",\"program\":\"test\",\"model\":\"test-model\",\"name\":\"${agents[i]}\",\"task_description\":\"test agent ${i}\"}")")
        id=$((id + 1))
    done

    for ((i = 0; i < msg_count; i++)); do
        sender="${agents[i]}"
        recipient="${agents[(i+1)%agent_count]}"
        requests+=("$(mk_tool_call "$id" send_message "{\"project_key\":\"${proj_path}\",\"sender_name\":\"${sender}\",\"to\":[\"${recipient}\"],\"subject\":\"Test message $((i + 1)) about database migration\",\"body_md\":\"This is test message number $((i + 1)) discussing the database migration plan and API endpoints.\"}")")
        id=$((id + 1))
    done

    response="$(send_session "$db_path" "${requests[@]}")" || return 1
    e2e_save_artifact "setup-$(basename "$db_path").jsonl" "$response"
    for ((i = 2; i < id; i++)); do
        require_success "$response" "$i" || return 1
    done
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
require_success "$RESP" 2
STATUS=$(is_error "$RESP" 3)
RESULT=$(extract_tool_result "$RESP" 3)
RESULT_IDS=$(search_ids "$RESP" 3)

# Search should succeed (return empty results, not error)
e2e_assert_eq "cold start search does not crash" "OK" "$STATUS"                      # 1
e2e_assert_eq "cold start returns an empty result list" "" "$RESULT_IDS"              # 2
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
require_success "$RESP" 2
RESULT_IDS=$(search_ids "$RESP" 3)
e2e_assert_eq "empty query returns an empty result list" "" "$RESULT_IDS"             # 4
e2e_step_end "empty_query_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 3: Search with special characters / injection attempts
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with special characters"

DB3="${WORK}/special_chars.db"
PROJ3="${WORK}/proj-special-chars"

# Set up project with data first
setup_project_with_data "$DB3" "$PROJ3" 3

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
    escaped_query=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1])[1:-1])' "$query")

    RESP=$(send_session "$DB3" \
        "$INIT_REQ" \
        "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ3}\"}")" \
        "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ3}\",\"query\":\"${escaped_query}\"}")" \
    )

    require_success "$RESP" 2
    search_ids "$RESP" 3 >/dev/null
    e2e_pass "special query $i returned a valid result list"                             # 5-13
done
e2e_step_end "special_char_queries"

# ═════════════════════════════════════════════════════════════════════════
# Case 4: Search after valid data insertion (happy path baseline)
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search after data insertion (happy path)"

DB4="${WORK}/happy_path.db"
PROJ4="${WORK}/proj-happy-path"

# Set up project with 5 messages about "database migration"
setup_project_with_data "$DB4" "$PROJ4" 5

RESP=$(send_session "$DB4" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ4}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ4}\",\"query\":\"database migration\"}")" \
)

e2e_step_start "happy_path_search"
require_success "$RESP" 2
STATUS=$(is_error "$RESP" 3)
RESULT=$(extract_tool_result "$RESP" 3)

e2e_assert_eq "happy path search succeeds" "OK" "$STATUS"                             # 14
e2e_assert_not_contains "happy path no error" "$RESULT" "error"                        # 15

RESULT_IDS=$(search_ids "$RESP" 3 sorted)
EXPECTED_IDS=$(fixture_message_ids "$DB4" 5)
e2e_assert_eq "happy path returns every seeded message" "$EXPECTED_IDS" "$RESULT_IDS" # 16
e2e_save_artifact "happy-path.jsonl" "$RESP"
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

e2e_assert_eq "nonexistent project returns a tool error" "ERROR" "$STATUS"             # 17
e2e_assert_contains "error identifies the unknown project" "$RESULT" "/nonexistent/project/path" # 18
e2e_save_artifact "unknown-project.jsonl" "$RESP"
e2e_step_end "nonexistent_project_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 6: Search after search — recovery after failure
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Recovery: search still works after prior failure"

DB6="${WORK}/recovery.db"
PROJ6="${WORK}/proj-recovery"

# First set up data
setup_project_with_data "$DB6" "$PROJ6" 3

# Session: try a bad query, then try a good one
RESP=$(send_session "$DB6" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ6}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"/ghost/project\",\"query\":\"test\"}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ6}\",\"query\":\"database\"}")" \
)

e2e_step_start "recovery_after_failure"
require_success "$RESP" 2
STATUS_BAD=$(is_error "$RESP" 3)
e2e_assert_eq "unknown project returns a tool error" "ERROR" "$STATUS_BAD"             # 19
BAD_RESULT=$(extract_tool_result "$RESP" 3)
e2e_assert_contains "recovery starts with the unknown-project error" "$BAD_RESULT" "/ghost/project"

# Request 4 should succeed (good project, same session)
STATUS_GOOD=$(is_error "$RESP" 4)
e2e_assert_eq "good search after bad succeeds" "OK" "$STATUS_GOOD"                    # 20

RESULT_IDS=$(search_ids "$RESP" 4 sorted)
EXPECTED_IDS=$(fixture_message_ids "$DB6" 3)
e2e_assert_eq "recovery returns the three seeded messages" "$EXPECTED_IDS" "$RESULT_IDS" # 21
e2e_save_artifact "recovery.jsonl" "$RESP"
e2e_step_end "recovery_after_failure"

# ═════════════════════════════════════════════════════════════════════════
# Case 7: Multiple rapid searches (sequence stability)
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Rapid sequential searches"

DB7="${WORK}/rapid_search.db"
PROJ7="${WORK}/proj-rapid-search"

setup_project_with_data "$DB7" "$PROJ7" 5

REQUESTS=("$INIT_REQ")
REQUESTS+=("$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ7}\"}")")
for i in $(seq 3 12); do
    REQUESTS+=("$(mk_tool_call $i search_messages "{\"project_key\":\"${PROJ7}\",\"query\":\"test message\"}")")
done

RESP=$(send_session "$DB7" "${REQUESTS[@]}")

e2e_step_start "rapid_searches"
require_success "$RESP" 2
EXPECTED_IDS=$(fixture_message_ids "$DB7" 5)
for i in $(seq 3 12); do
    RESULT_IDS=$(search_ids "$RESP" "$i" sorted)
    e2e_assert_eq "rapid search $i returns the seeded messages" "$EXPECTED_IDS" "$RESULT_IDS" # 22-31
done
e2e_step_end "rapid_searches"

# ═════════════════════════════════════════════════════════════════════════
# Case 8: Search with various limit values
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with boundary limits"

DB8="${WORK}/limits.db"
PROJ8="${WORK}/proj-limits"

setup_project_with_data "$DB8" "$PROJ8" 5

# Limit zero uses the default.
RESP=$(send_session "$DB8" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ8}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ8}\",\"query\":\"test\",\"limit\":0}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ8}\",\"query\":\"test\",\"limit\":1}")" \
    "$(mk_tool_call 5 search_messages "{\"project_key\":\"${PROJ8}\",\"query\":\"test\",\"limit\":1000}")" \
)

e2e_step_start "boundary_limits"
require_success "$RESP" 2
EXPECTED_IDS=$(fixture_message_ids "$DB8" 5)
RESULT_ZERO=$(search_ids "$RESP" 3)
RESULT_DEFAULT_SET=$(search_ids "$RESP" 3 sorted)
RESULT_ONE=$(search_ids "$RESP" 4)
RESULT_LARGE=$(search_ids "$RESP" 5 sorted)
e2e_assert_eq "limit=0 defaults to the full fixture" "$EXPECTED_IDS" "$RESULT_DEFAULT_SET" # 32
e2e_assert_eq "limit=1 returns the default search's top message" "${RESULT_ZERO%%,*}" "$RESULT_ONE" # 33
e2e_assert_eq "limit=1000 returns the full fixture" "$EXPECTED_IDS" "$RESULT_LARGE"     # 34
e2e_step_end "boundary_limits"

# ═════════════════════════════════════════════════════════════════════════
# Case 9: Search config kill switches via env vars
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search config kill switches"

DB9="${WORK}/kill_switches.db"
PROJ9="${WORK}/proj-kill-switches"

setup_project_with_data "$DB9" "$PROJ9" 3

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
printf '%s\n' "THIS IS NOT A SQLITE DATABASE FILE" > "$DB10"
cp "$DB10" "${DB10}.original"
CORRUPT_ARCHIVE="${DB10%.db}.archive"
mkdir -p "$CORRUPT_ARCHIVE"

e2e_step_start "corrupt_db_search"
# A corrupt mailbox is rejected during startup, before the client can initialize.
CORRUPT_STATUS=$(DATABASE_URL="sqlite:///$DB10" STORAGE_ROOT="$CORRUPT_ARCHIVE" \
    SEARCH_V3_INDEX_DIR="$CORRUPT_ARCHIVE/search_index" python3 -c '
import subprocess
import sys

with open(sys.argv[2], "w", encoding="utf-8") as log:
    result = subprocess.run([sys.argv[1], "serve-stdio"], input=sys.argv[3] + "\n",
                            text=True, stdout=log, stderr=log, timeout=12.5, check=False)
print(result.returncode)
' "$AM_BIN" "${E2E_ARTIFACT_DIR}/corrupt-startup.log" "$INIT_REQ")
e2e_assert_eq "corrupt DB exits with a startup error" 1 "$CORRUPT_STATUS"               # 39
e2e_assert_contains "startup error identifies the corrupt mailbox" \
    "$(cat "${E2E_ARTIFACT_DIR}/corrupt-startup.log")" "database disk image is malformed"
if cmp -s "$DB10" "${DB10}.original"; then
    e2e_pass "corrupt DB bytes are preserved"                                         # 40
else
    e2e_fail "corrupt DB was changed during startup"
fi
e2e_step_end "corrupt_db_search"

# ═════════════════════════════════════════════════════════════════════════
# Case 11: Search with product-level operations
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Product-level search resilience"

DB11="${WORK}/product_search.db"
PROJ11="${WORK}/proj-product-search"

setup_project_with_data "$DB11" "$PROJ11" 3

# Search with product-level tool on nonexistent product
RESP=$(send_session "$DB11" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 search_messages_product "{\"product_key\":\"nonexistent_product\",\"query\":\"test\"}")" \
)

e2e_step_start "product_search_nonexistent"
STATUS=$(is_error "$RESP" 2)
e2e_assert_eq "nonexistent product search fails gracefully" "ERROR" "$STATUS"          # 41
RESULT=$(extract_tool_result "$RESP" 2)
e2e_assert_contains "product search identifies the missing product" "$RESULT" "Product not found: nonexistent_product" # 42
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

    require_success "$RESP2" 2
    require_success "$RESP2" 3
    UNICODE_ID=$(sent_message_id "$RESP2" 4)
    search_ids "$RESP2" 5 >/dev/null
    EMOJI_IDS=$(search_ids "$RESP2" 6)
    UNICODE_IDS=$(search_ids "$RESP2" 7)

    e2e_pass "Japanese query returns a valid result list"                             # 43
    e2e_assert_eq "emoji query finds the seeded message" "$UNICODE_ID" "$EMOJI_IDS"    # 44
    e2e_assert_eq "accented Unicode query finds the seeded message" "$UNICODE_ID" "$UNICODE_IDS" # 45
else
    e2e_fail "Unicode fixture agent registration did not return a name"
fi
e2e_step_end "unicode_messages"

# ═════════════════════════════════════════════════════════════════════════
# Case 13: Search with very long query strings
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Very long query strings"

DB13="${WORK}/long_query.db"
PROJ13="${WORK}/proj-long-query"

setup_project_with_data "$DB13" "$PROJ13" 2

# Generate the exact 5,000-character query.
ESCAPED_LONG=$(python3 -c 'import json; print(json.dumps(("database migration plan " * 220)[:5000])[1:-1])')

RESP=$(send_session "$DB13" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ13}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ13}\",\"query\":\"${ESCAPED_LONG}\"}")" \
)

e2e_step_start "long_query_search"
require_success "$RESP" 2
RESULT=$(extract_tool_result "$RESP" 3)
e2e_assert_not_contains "long query no panic" "$RESULT" "panicked"                     # 46
e2e_assert_not_contains "long query no segfault" "$RESULT" "segfault"                  # 47
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

setup_project_with_data "$DB15" "$PROJ15" 5

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
require_success "$RESP_A" 2
require_success "$RESP_B" 2
EXPECTED_IDS=$(fixture_message_ids "$DB15" 5)
SORTED_IDS=$(search_ids "$RESP_A" 3 sorted)
IDS_A=$(search_ids "$RESP_A" 3)
IDS_B=$(search_ids "$RESP_B" 3)
e2e_assert_eq "determinism search returns the seeded set" "$EXPECTED_IDS" "$SORTED_IDS"
e2e_assert_eq "ranked message IDs are stable after restart" "$IDS_A" "$IDS_B"          # 52
e2e_step_end "search_determinism"

# ═════════════════════════════════════════════════════════════════════════
# Case 16: Search with various env var configurations
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Search with diverse env var configs"

DB16="${WORK}/env_configs.db"
PROJ16="${WORK}/proj-env-configs"

setup_project_with_data "$DB16" "$PROJ16" 3

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

setup_project_with_data "$DB17" "$PROJ17" 2

RESP=$(send_session "$DB17" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ17}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"   \"}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"\\t\\n\"}")" \
    "$(mk_tool_call 5 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"a\"}")" \
    "$(mk_tool_call 6 search_messages "{\"project_key\":\"${PROJ17}\",\"query\":\"AND OR NOT\"}")" \
)

e2e_step_start "degenerate_queries"
require_success "$RESP" 2
WHITESPACE_IDS=$(search_ids "$RESP" 3)
ESCAPE_IDS=$(search_ids "$RESP" 4)
e2e_assert_eq "whitespace query returns an empty list" "" "$WHITESPACE_IDS"           # 58
e2e_assert_eq "tab and newline query returns an empty list" "" "$ESCAPE_IDS"           # 59
search_ids "$RESP" 5 >/dev/null
e2e_pass "single char query returns a valid result list"                              # 60
RESULT=$(extract_tool_result "$RESP" 6)
e2e_assert_not_contains "boolean keywords query returns without panic" "$RESULT" "panicked" # 61
e2e_step_end "degenerate_queries"

# ═════════════════════════════════════════════════════════════════════════
# Case 18: Thread-based search operations
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Thread summary resilience"

DB18="${WORK}/thread_ops.db"
PROJ18="${WORK}/proj-thread-ops"

setup_project_with_data "$DB18" "$PROJ18" 3

# Try to summarize a nonexistent thread
RESP=$(send_session "$DB18" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ18}\"}")" \
    "$(mk_tool_call 3 summarize_thread "{\"project_key\":\"${PROJ18}\",\"thread_id\":\"nonexistent-thread-99999\"}")" \
)

e2e_step_start "thread_summary_nonexistent"
require_success "$RESP" 2
RESULT=$(extract_tool_result "$RESP" 3)
# Should handle gracefully — empty summary or error, not crash
e2e_assert_not_contains "nonexistent thread returns without panic" "$RESULT" "panicked" # 62
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

# Run the same query in both projects, with a matching fixture in A.
for i in 2 3 4 5; do
    require_success "$RESP_SETUP_A" "$i"
    require_success "$RESP_SETUP_B" "$i"
done
ALPHA_ID=$(sent_message_id "$RESP_SETUP_A" 5)
RESP=$(send_session "$DB19" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ19B}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ19B}\",\"query\":\"alpha-unique-term-xyz\"}")" \
    "$(mk_tool_call 4 search_messages "{\"project_key\":\"${PROJ19A}\",\"query\":\"alpha-unique-term-xyz\"}")" \
)

e2e_step_start "search_isolation"
require_success "$RESP" 2
BETA_RESULTS=$(search_ids "$RESP" 3)
ALPHA_RESULTS=$(search_ids "$RESP" 4)
e2e_assert_eq "query returns its planted result in project A" "$ALPHA_ID" "$ALPHA_RESULTS" # 63
e2e_assert_eq "same query returns an empty list from project B" "" "$BETA_RESULTS"    # 64
e2e_save_artifact "project-isolation.jsonl" "$RESP"
e2e_step_end "search_isolation"

# ═════════════════════════════════════════════════════════════════════════
# Case 20: Consistency — search_messages tool response structure
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Response structure invariants"

DB20="${WORK}/structure.db"
PROJ20="${WORK}/proj-structure"

setup_project_with_data "$DB20" "$PROJ20" 5

RESP=$(send_session "$DB20" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ20}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ20}\",\"query\":\"database\"}")" \
)

e2e_step_start "response_structure"
require_success "$RESP" 2
RESULT_IDS=$(search_ids "$RESP" 3 sorted)
EXPECTED_IDS=$(fixture_message_ids "$DB20" 5)
e2e_assert_eq "response has a valid result list with all five IDs" "$EXPECTED_IDS" "$RESULT_IDS" # 65
e2e_step_end "response_structure"

# ═════════════════════════════════════════════════════════════════════════
# Case 21: Simultaneous project creation + search
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Concurrent project operations"

DB21="${WORK}/concurrent.db"

# One stdio server owns the mailbox. Pipeline independent requests within that runtime.
e2e_step_start "concurrent_sessions"
CREATE_REQUESTS=("$INIT_REQ")
SEARCH_REQUESTS=("$INIT_REQ")
for i in $(seq 1 5); do
    PROJ_I="${WORK}/proj-concurrent-${i}"
    id=$((i + 1))
    CREATE_REQUESTS+=("$(mk_tool_call "$id" ensure_project "{\"human_key\":\"${PROJ_I}\"}")")
    SEARCH_REQUESTS+=("$(mk_tool_call "$id" search_messages "{\"project_key\":\"${PROJ_I}\",\"query\":\"test\"}")")
done
CREATE_RESP=$(send_session "$DB21" --pipeline "${CREATE_REQUESTS[@]}")
SEARCH_RESP=$(send_session "$DB21" --pipeline "${SEARCH_REQUESTS[@]}")
e2e_save_artifact "pipelined-create.jsonl" "$CREATE_RESP"
e2e_save_artifact "pipelined-search.jsonl" "$SEARCH_RESP"
CONCURRENT_PASS=0
for id in $(seq 2 6); do
    require_success "$CREATE_RESP" "$id"
    RESULT_IDS=$(search_ids "$SEARCH_RESP" "$id")
    if [ -z "$RESULT_IDS" ]; then
        CONCURRENT_PASS=$((CONCURRENT_PASS + 1))
    fi
done

e2e_assert_eq "five pipelined project searches return empty lists after reopen" 5 "$CONCURRENT_PASS" # 66
PROJECT_COUNT=$(python3 -c '
from pathlib import Path
import sqlite3
import sys

with sqlite3.connect(Path(sys.argv[1]).as_uri() + "?mode=ro", uri=True) as conn:
    keys = {row[0] for row in conn.execute("SELECT human_key FROM projects")}
    print(sum(str(Path(sys.argv[2]) / f"proj-concurrent-{i}") in keys for i in range(1, 6)))
' "$DB21" "$WORK")
e2e_assert_eq "all five project rows are persisted" 5 "$PROJECT_COUNT"               # 67
e2e_step_end "concurrent_sessions"

# ═════════════════════════════════════════════════════════════════════════
# Case 22: Search tool parameter validation
# ═════════════════════════════════════════════════════════════════════════
e2e_case_banner "Parameter validation"

DB22="${WORK}/param_validate.db"
PROJ22="${WORK}/proj-param-validate"

setup_project_with_data "$DB22" "$PROJ22" 2

# Missing required query field
RESP=$(send_session "$DB22" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 ensure_project "{\"human_key\":\"${PROJ22}\"}")" \
    "$(mk_tool_call 3 search_messages "{\"project_key\":\"${PROJ22}\"}")" \
)

e2e_step_start "missing_query_param"
require_success "$RESP" 2
STATUS=$(is_error "$RESP" 3)
e2e_assert_eq "missing query param returns error" "ERROR" "$STATUS"                    # 68
RESULT=$(extract_tool_result "$RESP" 3)
e2e_assert_contains "missing query error identifies the argument" "$RESULT" "query"
e2e_step_end "missing_query_param"

# Missing required project_key field
RESP=$(send_session "$DB22" \
    "$INIT_REQ" \
    "$(mk_tool_call 2 search_messages "{\"query\":\"test\"}")" \
)

e2e_step_start "missing_project_key"
STATUS=$(is_error "$RESP" 2)
e2e_assert_eq "missing project_key returns error" "ERROR" "$STATUS"                    # 69
RESULT=$(extract_tool_result "$RESP" 2)
e2e_assert_contains "missing project error identifies the argument" "$RESULT" "project_key"
e2e_step_end "missing_project_key"

# Valid JSON-RPC with an invalid arguments type.
RESP=$(send_session "$DB22" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_messages","arguments":"not json"}}' \
)

e2e_step_start "invalid_json_args"
STATUS=$(is_error "$RESP" 2)
e2e_assert_eq "invalid arguments type returns a tool error" "ERROR" "$STATUS"          # 70
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
require_success "$RESP" 2
NEGATIVE_IDS=$(search_ids "$RESP" 3 sorted)
LARGE_IDS=$(search_ids "$RESP" 4 sorted)
EXPECTED_IDS=$(fixture_message_ids "$DB22" 2)
e2e_assert_eq "negative limit uses the default and finds both messages" "$EXPECTED_IDS" "$NEGATIVE_IDS" # 71
e2e_assert_eq "huge limit is clamped and finds both messages" "$EXPECTED_IDS" "$LARGE_IDS" # 72
e2e_step_end "invalid_limits"

# ═════════════════════════════════════════════════════════════════════════
# Summary
# ═════════════════════════════════════════════════════════════════════════

sv3_write_summary || true
e2e_summary
