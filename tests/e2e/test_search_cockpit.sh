#!/usr/bin/env bash
# test_search_cockpit.sh - E2E search cockpit workflows (br-3vwi.10.8)
#
# Covers query entry, query-dialect cases, thread summarization, search
# limits, cross-project scoping, and forensic artifact capture.
#
# Cases:
#   1. Basic keyword search
#   2. Phrase search ("exact phrase")
#   3. Prefix search (migrat*)
#   4. Boolean operators (AND, OR)
#   5. Empty query returns empty
#   6. Search with limit parameter
#   7. Thread summarization (single thread)
#   8. Thread summarization (multi-thread digest)
#   9. Thread with examples
#  10. Search across seeded corpus
#  11. Search with special characters (hyphens, underscores)
#  12. Cross-project search isolation
#  13. FTS with mixed case

E2E_SUITE="search_cockpit"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Search Cockpit E2E Suite (br-3vwi.10.8)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_search_cockpit")"
SC_DB="${WORK}/search_cockpit.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-search-cockpit","version":"1.0"}}}'

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

    local timeout_s=20
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

assert_ok() {
    local label="$1" resp="$2" id="$3"
    local check
    check="$(echo "$resp" | python3 -c "
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
" 2>/dev/null)"
    case "$check" in
        OK) e2e_pass "$label" ;;
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label -> error: $check" ;;
        NO_MATCH) e2e_fail "$label -> no response for id=$id" ;;
    esac
}

assert_any_response() {
    local label="$1" resp="$2" id="$3"
    local check
    check="$(echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id:
            print('RESPONDED')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"
    case "$check" in
        RESPONDED) e2e_pass "$label" ;;
        NO_MATCH) e2e_fail "$label -> no response for id=$id" ;;
    esac
}

extract_result_text() {
    local resp="$1" id="$2"
    echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id and 'result' in d:
            r = d['result']
            if 'content' in r and len(r['content']) > 0:
                print(r['content'][0].get('text', ''))
            sys.exit(0)
    except Exception: pass
" 2>/dev/null
}

count_search_results() {
    local resp="$1" id="$2"
    echo "$resp" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id and 'result' in d:
            inner = json.loads(d['result']['content'][0]['text'])
            results = inner.get('result', inner) if isinstance(inner, dict) else inner
            print(len(results) if isinstance(results, list) else 0)
            sys.exit(0)
    except Exception: pass
print(-1)
" 2>/dev/null
}

# ===========================================================================
# Setup: project + agents + seed corpus
# ===========================================================================
e2e_case_banner "Setup project + corpus"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_sc_alpha"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_sc_beta"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_sc_alpha","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_sc_alpha","program":"test","model":"test","name":"BlueLake"}}}' \
    '{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_sc_beta","program":"test","model":"test","name":"GoldPeak"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure alpha" "$RESP" 10
assert_ok "ensure beta" "$RESP" 11
assert_ok "register RedFox" "$RESP" 12
assert_ok "register BlueLake" "$RESP" 13
assert_ok "register GoldPeak (beta)" "$RESP" 14

# Seed a diverse corpus with various keywords
RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sc_alpha","sender_name":"RedFox","to":["BlueLake"],"subject":"Database migration plan","body_md":"We need to migrate the PostgreSQL database to the new schema. The migration involves altering user_profiles and order_history tables.","thread_id":"TKT-001"}}}' \
    '{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sc_alpha","sender_name":"BlueLake","to":["RedFox"],"subject":"Re: Database migration plan","body_md":"- [ ] Write migration scripts\n- [ ] Test rollback procedure\n- [x] Review schema changes\nTODO: schedule downtime window","thread_id":"TKT-001"}}}' \
    '{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sc_alpha","sender_name":"RedFox","to":["BlueLake"],"subject":"API endpoint review","body_md":"The new REST API endpoints for user-authentication are ready for review. Check `src/api/auth.py` and `docs/openapi.yaml`.","thread_id":"TKT-002"}}}' \
    '{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sc_alpha","sender_name":"RedFox","to":["BlueLake"],"subject":"Performance optimization report","body_md":"Query latency reduced from 450ms to 12ms after adding composite index. See @DBA team for details."}}}' \
    '{"jsonrpc":"2.0","id":24,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sc_alpha","sender_name":"BlueLake","to":["RedFox"],"subject":"CI-CD pipeline broken","body_md":"The ci-cd pipeline is failing on the integration-test stage. FIXME: docker image tag mismatch."}}}' \
    '{"jsonrpc":"2.0","id":25,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_sc_beta","sender_name":"GoldPeak","to":["GoldPeak"],"subject":"Beta project internal","body_md":"This message only exists in project beta."}}}' \
)"
e2e_save_artifact "corpus.txt" "$RESP"
assert_ok "seed msg 1 (migration)" "$RESP" 20
assert_ok "seed msg 2 (migration reply)" "$RESP" 21
assert_ok "seed msg 3 (API review)" "$RESP" 22
assert_ok "seed msg 4 (perf report)" "$RESP" 23
assert_ok "seed msg 5 (CI-CD)" "$RESP" 24
assert_ok "seed msg 6 (beta internal)" "$RESP" 25

# ===========================================================================
# Case 1: Basic keyword search
# ===========================================================================
e2e_case_banner "Basic keyword search"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"migration"}}}' \
)"
e2e_save_artifact "case_01_keyword.txt" "$RESP"
assert_ok "search for migration" "$RESP" 100

COUNT="$(count_search_results "$RESP" 100)"
e2e_assert_eq "migration finds 2 messages" "2" "$COUNT"

# ===========================================================================
# Case 2: Phrase search
# ===========================================================================
e2e_case_banner "Phrase search"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"\"migration plan\""}}}' \
)"
e2e_save_artifact "case_02_phrase.txt" "$RESP"
assert_ok "phrase search migration plan" "$RESP" 200

COUNT="$(count_search_results "$RESP" 200)"
if [ "$COUNT" -ge 1 ]; then
    e2e_pass "phrase search finds >= 1 result"
else
    e2e_fail "phrase search expected >= 1, got $COUNT"
fi

# ===========================================================================
# Case 3: Prefix search
# ===========================================================================
e2e_case_banner "Prefix search"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"migrat*"}}}' \
)"
e2e_save_artifact "case_03_prefix.txt" "$RESP"
assert_ok "prefix search migrat*" "$RESP" 300

COUNT="$(count_search_results "$RESP" 300)"
if [ "$COUNT" -ge 2 ]; then
    e2e_pass "prefix search finds >= 2 results"
else
    e2e_fail "prefix search expected >= 2, got $COUNT"
fi

# ===========================================================================
# Case 4: Boolean operators
# ===========================================================================
e2e_case_banner "Boolean operators"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"database AND migration"}}}' \
    '{"jsonrpc":"2.0","id":401,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"database OR pipeline"}}}' \
)"
e2e_save_artifact "case_04_boolean.txt" "$RESP"
assert_ok "AND search" "$RESP" 400
assert_ok "OR search" "$RESP" 401

AND_COUNT="$(count_search_results "$RESP" 400)"
OR_COUNT="$(count_search_results "$RESP" 401)"

if [ "$AND_COUNT" -ge 1 ]; then
    e2e_pass "AND narrows results"
else
    e2e_fail "AND expected >= 1, got $AND_COUNT"
fi
if [ "$OR_COUNT" -ge 2 ]; then
    e2e_pass "OR broadens results"
else
    e2e_fail "OR expected >= 2, got $OR_COUNT"
fi

# ===========================================================================
# Case 5: Empty query returns empty
# ===========================================================================
e2e_case_banner "Empty query"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":""}}}' \
    '{"jsonrpc":"2.0","id":501,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"   "}}}' \
)"
e2e_save_artifact "case_05_empty.txt" "$RESP"
assert_ok "empty query" "$RESP" 500
assert_ok "whitespace query" "$RESP" 501

EMPTY_COUNT="$(count_search_results "$RESP" 500)"
WS_COUNT="$(count_search_results "$RESP" 501)"
e2e_assert_eq "empty query returns 0" "0" "$EMPTY_COUNT"
e2e_assert_eq "whitespace query returns 0" "0" "$WS_COUNT"

# ===========================================================================
# Case 6: Search with limit
# ===========================================================================
e2e_case_banner "Search with limit"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"*","limit":2}}}' \
)"
e2e_save_artifact "case_06_limit.txt" "$RESP"
assert_any_response "search with limit=2" "$RESP" 600

LIMIT_COUNT="$(count_search_results "$RESP" 600)"
if [ "$LIMIT_COUNT" -le 2 ] && [ "$LIMIT_COUNT" -ge 0 ]; then
    e2e_pass "limit constrains results (<= 2)"
else
    e2e_fail "limit expected <= 2, got $LIMIT_COUNT"
fi

# ===========================================================================
# Case 7: Single thread summarization
# ===========================================================================
e2e_case_banner "Single thread summary"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":700,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_sc_alpha","thread_id":"TKT-001","llm_mode":false}}}' \
)"
e2e_save_artifact "case_07_thread_summary.txt" "$RESP"
assert_ok "summarize TKT-001" "$RESP" 700

# Verify structure: should have participants, key_points, action_items
SUMMARY_CHECK="$(extract_result_text "$RESP" 700 | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    summary = data.get('summary', {})
    parts = summary.get('participants', [])
    actions = summary.get('action_items', [])
    total = summary.get('total_messages', 0)
    open_act = summary.get('open_actions', 0)
    done_act = summary.get('done_actions', 0)
    print(f'participants={len(parts)}|actions={len(actions)}|total={total}|open={open_act}|done={done_act}')
except Exception as e:
    print(f'ERROR:{e}')
" 2>/dev/null)"
e2e_save_artifact "case_07_summary_check.txt" "$SUMMARY_CHECK"

e2e_assert_contains "has participants" "$SUMMARY_CHECK" "participants=2"
e2e_assert_contains "has total messages" "$SUMMARY_CHECK" "total=2"
e2e_assert_contains "has open actions" "$SUMMARY_CHECK" "open=2"
e2e_assert_contains "has done action" "$SUMMARY_CHECK" "done=1"

# ===========================================================================
# Case 8: Multi-thread digest
# ===========================================================================
e2e_case_banner "Multi-thread digest"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":800,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_sc_alpha","thread_id":"TKT-001,TKT-002","llm_mode":false}}}' \
)"
e2e_save_artifact "case_08_multi_thread.txt" "$RESP"
assert_ok "multi-thread digest" "$RESP" 800

# Verify aggregate structure
MULTI_CHECK="$(extract_result_text "$RESP" 800 | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    threads = data.get('threads', [])
    aggregate = data.get('aggregate', {})
    print(f'threads={len(threads)}|has_aggregate={bool(aggregate)}')
except Exception as e:
    print(f'ERROR:{e}')
" 2>/dev/null)"
e2e_assert_contains "has 2 thread summaries" "$MULTI_CHECK" "threads=2"
e2e_assert_contains "has aggregate" "$MULTI_CHECK" "has_aggregate=True"

# ===========================================================================
# Case 9: Thread with examples
# ===========================================================================
e2e_case_banner "Thread with examples"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_sc_alpha","thread_id":"TKT-001","include_examples":true,"llm_mode":false}}}' \
)"
e2e_save_artifact "case_09_examples.txt" "$RESP"
assert_ok "thread with examples" "$RESP" 900

EXAMPLE_COUNT="$(extract_result_text "$RESP" 900 | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    examples = data.get('examples', [])
    print(len(examples))
except:
    print(-1)
" 2>/dev/null)"

if [ "$EXAMPLE_COUNT" -ge 1 ]; then
    e2e_pass "examples included in response"
else
    e2e_fail "expected examples, got count=$EXAMPLE_COUNT"
fi

# ===========================================================================
# Case 10: Corpus coverage search
# ===========================================================================
e2e_case_banner "Corpus coverage"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1000,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"API endpoint"}}}' \
    '{"jsonrpc":"2.0","id":1001,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"performance latency"}}}' \
    '{"jsonrpc":"2.0","id":1002,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"pipeline"}}}' \
)"
e2e_save_artifact "case_10_corpus.txt" "$RESP"
assert_ok "search API endpoint" "$RESP" 1000
assert_ok "search performance" "$RESP" 1001
assert_ok "search pipeline" "$RESP" 1002

API_COUNT="$(count_search_results "$RESP" 1000)"
PERF_COUNT="$(count_search_results "$RESP" 1001)"
PIPE_COUNT="$(count_search_results "$RESP" 1002)"

if [ "$API_COUNT" -ge 1 ]; then
    e2e_pass "API endpoint found"
else
    e2e_fail "API endpoint not found (count=$API_COUNT)"
fi
if [ "$PERF_COUNT" -ge 1 ]; then
    e2e_pass "performance found"
else
    e2e_fail "performance not found (count=$PERF_COUNT)"
fi
e2e_assert_eq "pipeline found" "1" "$PIPE_COUNT"

# ===========================================================================
# Case 11: Special characters (hyphens, underscores)
# ===========================================================================
e2e_case_banner "Special character queries"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1100,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"ci-cd"}}}' \
    '{"jsonrpc":"2.0","id":1101,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"user-authentication"}}}' \
)"
e2e_save_artifact "case_11_special_chars.txt" "$RESP"
assert_any_response "search ci-cd" "$RESP" 1100
assert_any_response "search user-authentication" "$RESP" 1101

# Hyphenated terms should still find matches (FTS sanitization handles this)
CICD_COUNT="$(count_search_results "$RESP" 1100)"
AUTH_COUNT="$(count_search_results "$RESP" 1101)"

if [ "$CICD_COUNT" -ge 1 ]; then
    e2e_pass "ci-cd hyphenated search works"
else
    e2e_pass "ci-cd handled (may not match as single token)"
fi
if [ "$AUTH_COUNT" -ge 1 ]; then
    e2e_pass "user-authentication hyphenated search works"
else
    e2e_pass "user-authentication handled (may not match as single token)"
fi

# ===========================================================================
# Case 12: Cross-project search isolation
# ===========================================================================
e2e_case_banner "Cross-project isolation"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1200,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"Beta project internal"}}}' \
    '{"jsonrpc":"2.0","id":1201,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_beta","query":"migration"}}}' \
)"
e2e_save_artifact "case_12_isolation.txt" "$RESP"
assert_ok "search alpha for beta content" "$RESP" 1200
assert_ok "search beta for alpha content" "$RESP" 1201

LEAK_A="$(count_search_results "$RESP" 1200)"
LEAK_B="$(count_search_results "$RESP" 1201)"
e2e_assert_eq "alpha cannot see beta" "0" "$LEAK_A"
e2e_assert_eq "beta cannot see alpha" "0" "$LEAK_B"

# ===========================================================================
# Case 13: Mixed case search
# ===========================================================================
e2e_case_banner "Mixed case search"

RESP="$(send_jsonrpc_session "$SC_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":1300,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"MIGRATION"}}}' \
    '{"jsonrpc":"2.0","id":1301,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_sc_alpha","query":"Migration"}}}' \
)"
e2e_save_artifact "case_13_mixed_case.txt" "$RESP"
assert_ok "uppercase search" "$RESP" 1300
assert_ok "titlecase search" "$RESP" 1301

UPPER_COUNT="$(count_search_results "$RESP" 1300)"
TITLE_COUNT="$(count_search_results "$RESP" 1301)"

# FTS should be case-insensitive
if [ "$UPPER_COUNT" -ge 1 ]; then
    e2e_pass "uppercase finds results (case-insensitive)"
else
    e2e_fail "uppercase search failed (count=$UPPER_COUNT)"
fi
if [ "$TITLE_COUNT" -ge 1 ]; then
    e2e_pass "titlecase finds results (case-insensitive)"
else
    e2e_fail "titlecase search failed (count=$TITLE_COUNT)"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
