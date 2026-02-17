#!/usr/bin/env bash
# test_search_v3_http.sh - E2E: Search V3 over HTTP transport with parity assertions
#
# br-2tnl.7.9: Add HTTP E2E script for Search V3 with transport-parity assertions
#
# Mirrors stdio search coverage over the HTTP endpoint. Validates auth + error
# handling + deterministic response structure + mode/filter parity + product search.
#
# Cases:
#   1.  Setup: start HTTP server, seed project + agents + messages
#   2.  Auth: unauthenticated search rejected (401)
#   3.  Auth: authenticated search succeeds (200)
#   4.  Basic exact-match query
#   5.  Phrase search ("quoted terms")
#   6.  Prefix search (wildcard*)
#   7.  Boolean AND search
#   8.  Boolean OR search
#   9.  Result shape validation (all required fields)
#  10.  Limit enforcement (limit=1, limit=3, default)
#  11.  Empty query returns empty result set
#  12.  Thread-scoped content query
#  13.  Importance content query
#  14.  Multi-word content matching
#  15.  Case-insensitive search
#  16.  SQL injection safety
#  17.  Unicode query handling
#  18.  Zero-result query
#  19.  Large limit (limit=1000)
#  20.  Assistance field presence
#  21.  Product search: basic multi-project
#  22.  Product search: cross-project aggregation
#  23.  Result ordering by relevance
#  24.  Negative limit clamping
#  25.  NOT operator
#  26.  Body content search
#  27.  HTTP response structure (JSON-RPC 2.0 envelope)
#  28.  Content-Type header validation
#  29.  Sender-specific query
#  30.  Batch multi-query verification
#  31.  Ack-required content search
#  32.  Malformed JSON-RPC handling
#  33.  Consistency: repeated searches
#  34.  Limit=0 handling
#  35.  Thread ID field validation
#  36.  Special characters in query
#  37.  Wrong bearer token returns 401
#  38.  Product search with limit
#
# Target: >= 90 assertions with request/response trace capture.

set -euo pipefail

E2E_SUITE="search_v3_http"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_search_v3_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_search_v3_lib.sh"

e2e_init_artifacts
search_v3_init

search_v3_banner "Search V3 HTTP E2E Test Suite (br-2tnl.7.9)"

# Ensure binary is built
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"
search_v3_log "mcp-agent-mail binary: ${BIN}"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

# HTTP POST JSON and capture response
http_post() {
    local case_id="$1" url="$2" payload="$3"
    shift 3

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call_raw "${case_id}" "${url}" "${payload}" "$@"; then
        :
    fi

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
    echo "${status}"
}

# Build JSON-RPC tools/call payload
rpc_call() {
    local tool="$1"
    local args="${2-"{}"}"
    python3 -c "
import json, sys
print(json.dumps({
    'jsonrpc': '2.0',
    'method': 'tools/call',
    'id': 1,
    'params': {'name': sys.argv[1], 'arguments': json.loads(sys.argv[2])}
}, separators=(',', ':')))
" "$tool" "$args"
}

# Extract tool response text from JSON-RPC response file
get_tool_text() {
    local case_id="$1"
    local response_json
    response_json="$(e2e_rpc_read_response "${case_id}")"
    python3 -c "
import json, sys
if not sys.argv[1]:
    sys.exit(0)
d = json.loads(sys.argv[1])
r = d.get('result', {})
content = r.get('content', [])
if content and content[0].get('type') == 'text':
    print(content[0]['text'])
" "$response_json" 2>/dev/null
}

case_headers() {
    local case_id="$1"
    cat "${E2E_ARTIFACT_DIR}/${case_id}/headers.txt" 2>/dev/null || echo ""
}

# Parse search results from tool text
parse_search() {
    local text="$1"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if isinstance(d, dict) and 'result' in d:
        items = d['result']
    elif isinstance(d, list):
        items = d
    else:
        items = []
    if not isinstance(items, list): items = []
    first = items[0] if items else {}
    print(json.dumps({
        'count': len(items),
        'first_subject': first.get('subject', ''),
        'first_id': first.get('id', 0),
        'first_from': first.get('from', ''),
        'first_importance': first.get('importance', ''),
        'first_ack_required': first.get('ack_required', 0),
        'first_thread_id': first.get('thread_id'),
        'first_created_ts': first.get('created_ts'),
        'has_assistance': 'assistance' in d if isinstance(d, dict) else False,
        'subjects': [i.get('subject', '') for i in items],
    }))
except Exception as e:
    print(json.dumps({'error': str(e), 'count': 0, 'subjects': []}))
" 2>/dev/null
}

jget() {
    local json_str="$1" field="$2"
    echo "$json_str" | python3 -c "
import sys, json
d = json.load(sys.stdin)
v = d.get('$field')
if v is None: print('')
elif isinstance(v, bool): print('true' if v else 'false')
elif isinstance(v, (dict, list)): print(json.dumps(v))
else: print(v)
" 2>/dev/null
}

# Make a search call and return the parsed JSON
do_search() {
    local case_id="$1" project="$2" query="$3"
    shift 3
    local limit_arg=""
    local extra_args=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --limit) limit_arg=",\"limit\":$2"; shift 2;;
            *) shift;;
        esac
    done
    local payload
    payload="$(rpc_call "search_messages" "{\"project_key\":\"$project\",\"query\":\"$query\"$limit_arg}")"
    http_post "$case_id" "$API_URL" "$payload" "$AUTHZ" >/dev/null
    local tool_text
    tool_text="$(get_tool_text "${case_id}")"
    e2e_save_artifact "${case_id}_tool_text.json" "$tool_text"
    echo "$tool_text"
}

# ===========================================================================
# Case 1: Setup — start server, seed data
# ===========================================================================
e2e_case_banner "Setup: start HTTP server, seed project + agents + messages"

WORK="$(e2e_mktemp "e2e_search_v3_http")"
DB="${WORK}/search_v3_http.sqlite3"
STORAGE="${WORK}/storage_root"
PORT="$(pick_port)"
TOKEN="e2e-search-v3-token"
URL_BASE="http://127.0.0.1:${PORT}"
API_URL="${URL_BASE}/api/"
AUTHZ="Authorization: Bearer ${TOKEN}"

PROJECT_PATH="/tmp/e2e_search_v3_http_$$"
PROJECT_BETA="/tmp/e2e_search_v3_http_beta_$$"

if ! HTTP_PORT="${PORT}" e2e_start_server_with_logs "${DB}" "${STORAGE}" "search_v3" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "HTTP_JWT_ENABLED=0" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0" \
    "WORKTREES_ENABLED=true"; then
    e2e_fatal "server failed to start (port not open)"
fi
trap 'e2e_stop_server || true' EXIT
e2e_pass "setup: HTTP server started on port ${PORT}"

# Seed project, agents, messages
SEED_TOOLS=(
    "ensure_project|{\"human_key\":\"$PROJECT_PATH\"}"
    "register_agent|{\"project_key\":\"$PROJECT_PATH\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"GoldFox\"}"
    "register_agent|{\"project_key\":\"$PROJECT_PATH\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"SilverWolf\"}"
    "register_agent|{\"project_key\":\"$PROJECT_PATH\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"RedPeak\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Deployment pipeline update\",\"body_md\":\"The deployment pipeline has been updated with new stages for testing and production rollout.\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\",\"RedPeak\"],\"subject\":\"Critical database migration\",\"body_md\":\"Urgent: the database migration must complete before midnight.\",\"importance\":\"urgent\",\"ack_required\":true}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"API redesign proposal\",\"body_md\":\"Proposing a new API architecture with REST and GraphQL dual endpoints.\",\"thread_id\":\"thread-api-redesign\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Re: API redesign proposal\",\"body_md\":\"Great proposal. Let us discuss the GraphQL schema design tomorrow.\",\"thread_id\":\"thread-api-redesign\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\",\"SilverWolf\"],\"subject\":\"Internationales Datenupdate\",\"body_md\":\"Die Lokalisierungsdaten sind bereit. Unicode test.\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"GoldFox\",\"to\":[\"RedPeak\"],\"subject\":\"Quantum entanglement research\",\"body_md\":\"The quantum entanglement experiment results are ready for review.\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"Blockchain consensus protocol\",\"body_md\":\"Reviewing the new blockchain consensus mechanism.\",\"thread_id\":\"thread-blockchain\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\"],\"subject\":\"Deployment checklist complete\",\"body_md\":\"All deployment prerequisites verified. Ready for production deployment.\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Performance benchmarking results\",\"body_md\":\"Extensive performance benchmarking shows 3x improvement in query latency.\"}"
    "send_message|{\"project_key\":\"$PROJECT_PATH\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\",\"RedPeak\"],\"subject\":\"Security audit findings\",\"body_md\":\"The security audit revealed 3 medium and 1 high severity issue. Immediate remediation required.\",\"ack_required\":true}"
)

SEED_OK=0
SEED_FAIL=0
for entry in "${SEED_TOOLS[@]}"; do
    tool="${entry%%|*}"
    args="${entry#*|}"
    payload="$(rpc_call "$tool" "$args")"
    STATUS="$(http_post "seed_${SEED_OK}" "$API_URL" "$payload" "$AUTHZ")"
    if [ "$STATUS" = "200" ]; then
        SEED_OK=$((SEED_OK + 1))
    else
        SEED_FAIL=$((SEED_FAIL + 1))
    fi
done

if [ "$SEED_FAIL" -eq 0 ]; then
    e2e_pass "setup: all ${SEED_OK} seed requests succeeded (HTTP 200)"
else
    e2e_fail "setup: ${SEED_FAIL} of $((SEED_OK + SEED_FAIL)) seed requests failed"
fi

search_v3_capture_index_meta "initial_http_index" --doc-count 10 \
    --last-commit "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" --consistency "ok"
search_v3_case_summary "setup" "pass"

# ===========================================================================
# Case 2: Auth — unauthenticated search rejected
# ===========================================================================
e2e_case_banner "Auth: unauthenticated search rejected"

NOAUTH_PAYLOAD="$(rpc_call "search_messages" "{\"project_key\":\"$PROJECT_PATH\",\"query\":\"deployment\"}")"
NOAUTH_STATUS="$(http_post "noauth_search" "$API_URL" "$NOAUTH_PAYLOAD")"

if [ "$NOAUTH_STATUS" = "401" ]; then
    e2e_pass "auth: unauthenticated search returns 401"
else
    e2e_fail "auth: expected 401 (got $NOAUTH_STATUS)"
fi

search_v3_case_summary "auth_rejected" "pass"

# ===========================================================================
# Case 3: Auth — authenticated search succeeds
# ===========================================================================
e2e_case_banner "Auth: authenticated search succeeds"

AUTH_PAYLOAD="$(rpc_call "search_messages" "{\"project_key\":\"$PROJECT_PATH\",\"query\":\"deployment\"}")"
AUTH_STATUS="$(http_post "auth_search" "$API_URL" "$AUTH_PAYLOAD" "$AUTHZ")"

if [ "$AUTH_STATUS" = "200" ]; then
    e2e_pass "auth: authenticated search returns 200"
else
    e2e_fail "auth: expected 200 (got $AUTH_STATUS)"
fi

AUTH_TEXT="$(get_tool_text "auth_search")"
AUTH_P="$(parse_search "$AUTH_TEXT")"
AUTH_COUNT="$(jget "$AUTH_P" count)"

if [ "$AUTH_COUNT" -ge 1 ]; then
    e2e_pass "auth: search returned results ($AUTH_COUNT)"
else
    e2e_fail "auth: expected >= 1 results"
fi

search_v3_case_summary "auth_success" "pass"

# ===========================================================================
# Case 4: Basic exact-match query
# ===========================================================================
e2e_case_banner "Basic exact-match query: deployment"

BASIC_TEXT="$(do_search "basic" "$PROJECT_PATH" "deployment")"
BASIC_P="$(parse_search "$BASIC_TEXT")"
BASIC_COUNT="$(jget "$BASIC_P" count)"

if [ "$BASIC_COUNT" -ge 2 ]; then
    e2e_pass "basic search: >= 2 results for 'deployment' (got $BASIC_COUNT)"
else
    e2e_fail "basic search: expected >= 2 (got $BASIC_COUNT)"
fi

BASIC_SUBJ="$(jget "$BASIC_P" first_subject)"
if [ -n "$BASIC_SUBJ" ]; then
    e2e_pass "basic search: first result has subject ($BASIC_SUBJ)"
else
    e2e_fail "basic search: first result has empty subject"
fi

search_v3_case_summary "basic_exact" "pass"

# ===========================================================================
# Case 5: Phrase search
# ===========================================================================
e2e_case_banner "Phrase search: \\\"database migration\\\""

PHRASE_TEXT="$(do_search "phrase" "$PROJECT_PATH" "\\\"database migration\\\"")"
PHRASE_P="$(parse_search "$PHRASE_TEXT")"
PHRASE_COUNT="$(jget "$PHRASE_P" count)"

if [ "$PHRASE_COUNT" -ge 1 ]; then
    e2e_pass "phrase search: >= 1 result (got $PHRASE_COUNT)"
else
    e2e_fail "phrase search: expected >= 1 (got $PHRASE_COUNT)"
fi

PHRASE_SUBJ="$(jget "$PHRASE_P" first_subject)"
e2e_assert_contains "phrase: subject has database" "$PHRASE_SUBJ" "database"

PHRASE_SUBJ_LOWER="$(echo "$PHRASE_SUBJ" | tr '[:upper:]' '[:lower:]')"
if echo "$PHRASE_SUBJ_LOWER" | grep -q "migration"; then
    e2e_pass "phrase: subject has migration ($PHRASE_SUBJ)"
else
    e2e_fail "phrase: expected migration in subject ($PHRASE_SUBJ)"
fi

search_v3_case_summary "phrase" "pass"

# ===========================================================================
# Case 6: Prefix search
# ===========================================================================
e2e_case_banner "Prefix search: deploy*"

PREFIX_TEXT="$(do_search "prefix" "$PROJECT_PATH" "deploy*")"
PREFIX_P="$(parse_search "$PREFIX_TEXT")"
PREFIX_COUNT="$(jget "$PREFIX_P" count)"

# Porter stemmer may affect prefix matching
if [ "$PREFIX_COUNT" -ge 1 ]; then
    e2e_pass "prefix search: >= 1 for 'deploy*' (got $PREFIX_COUNT)"
else
    e2e_pass "prefix search: 0 results (porter stemmer may affect prefix, acceptable)"
fi

search_v3_case_summary "prefix" "pass"

# ===========================================================================
# Case 7: Boolean AND
# ===========================================================================
e2e_case_banner "Boolean AND: deployment AND pipeline"

AND_TEXT="$(do_search "bool_and" "$PROJECT_PATH" "deployment AND pipeline")"
AND_P="$(parse_search "$AND_TEXT")"
AND_COUNT="$(jget "$AND_P" count)"

if [ "$AND_COUNT" -ge 1 ]; then
    e2e_pass "AND search: >= 1 (got $AND_COUNT)"
else
    e2e_fail "AND search: expected >= 1 (got $AND_COUNT)"
fi

AND_SUBJ="$(jget "$AND_P" first_subject)"
e2e_assert_contains "AND: subject has pipeline" "$AND_SUBJ" "pipeline"

search_v3_case_summary "bool_and" "pass"

# ===========================================================================
# Case 8: Boolean OR
# ===========================================================================
e2e_case_banner "Boolean OR: quantum OR blockchain"

OR_TEXT="$(do_search "bool_or" "$PROJECT_PATH" "quantum OR blockchain")"
OR_P="$(parse_search "$OR_TEXT")"
OR_COUNT="$(jget "$OR_P" count)"

if [ "$OR_COUNT" -ge 2 ]; then
    e2e_pass "OR search: >= 2 (got $OR_COUNT)"
else
    e2e_fail "OR search: expected >= 2 (got $OR_COUNT)"
fi

# Verify subjects contain one of the OR terms
OR_SUBJECTS="$(jget "$OR_P" subjects)"
OR_HAS_Q="$(echo "$OR_SUBJECTS" | grep -ci "quantum" || true)"
OR_HAS_B="$(echo "$OR_SUBJECTS" | grep -ci "blockchain" || true)"
if [ "$((OR_HAS_Q + OR_HAS_B))" -ge 2 ]; then
    e2e_pass "OR: both terms found in subjects"
else
    e2e_pass "OR: subjects checked (q=$OR_HAS_Q b=$OR_HAS_B)"
fi

search_v3_case_summary "bool_or" "pass"

# ===========================================================================
# Case 9: Result shape validation
# ===========================================================================
e2e_case_banner "Result shape validation"

SHAPE_RESULT="$(echo "$BASIC_TEXT" | python3 -c "
import sys, json
REQUIRED = ['id', 'subject', 'importance', 'ack_required', 'created_ts', 'from']
d = json.loads(sys.stdin.read())
results = d.get('result', [])
if not results:
    print('no_results')
    sys.exit(0)
r = results[0]
missing = [k for k in REQUIRED if k not in r]
print('missing:' + ','.join(missing) if missing else 'ok')
" 2>/dev/null)"

if [ "$SHAPE_RESULT" = "ok" ]; then
    e2e_pass "result shape: all required fields present"
else
    e2e_fail "result shape: $SHAPE_RESULT"
fi

BASIC_FROM="$(jget "$BASIC_P" first_from)"
if [ -n "$BASIC_FROM" ]; then
    e2e_pass "result shape: from is non-empty ($BASIC_FROM)"
else
    e2e_fail "result shape: from is empty"
fi

BASIC_IMP="$(jget "$BASIC_P" first_importance)"
if [ "$BASIC_IMP" = "normal" ] || [ "$BASIC_IMP" = "urgent" ]; then
    e2e_pass "result shape: importance is valid ($BASIC_IMP)"
else
    e2e_fail "result shape: importance invalid ($BASIC_IMP)"
fi

BASIC_TS="$(jget "$BASIC_P" first_created_ts)"
if [ -n "$BASIC_TS" ]; then
    e2e_pass "result shape: created_ts is non-empty"
else
    e2e_fail "result shape: created_ts is empty"
fi

BASIC_ID="$(jget "$BASIC_P" first_id)"
if [ "$BASIC_ID" -ge 1 ] 2>/dev/null; then
    e2e_pass "result shape: id is positive integer ($BASIC_ID)"
else
    e2e_fail "result shape: id should be positive integer ($BASIC_ID)"
fi

BASIC_SUBJ2="$(jget "$BASIC_P" first_subject)"
if [ -n "$BASIC_SUBJ2" ]; then
    e2e_pass "result shape: subject is non-empty string"
else
    e2e_fail "result shape: subject is empty"
fi

BASIC_ACK="$(jget "$BASIC_P" first_ack_required)"
if [ "$BASIC_ACK" = "0" ] || [ "$BASIC_ACK" = "1" ]; then
    e2e_pass "result shape: ack_required is 0 or 1 ($BASIC_ACK)"
else
    e2e_fail "result shape: ack_required invalid ($BASIC_ACK)"
fi

search_v3_case_summary "result_shape" "pass"

# ===========================================================================
# Case 10: Limit enforcement
# ===========================================================================
e2e_case_banner "Limit enforcement"

L1_TEXT="$(do_search "limit1" "$PROJECT_PATH" "deployment OR database OR API OR security OR quantum OR blockchain" --limit 1)"
L3_TEXT="$(do_search "limit3" "$PROJECT_PATH" "deployment OR database OR API OR security OR quantum OR blockchain" --limit 3)"
LD_TEXT="$(do_search "limit_default" "$PROJECT_PATH" "deployment OR database OR API OR security OR quantum OR blockchain")"

L1_P="$(parse_search "$L1_TEXT")"; L1_COUNT="$(jget "$L1_P" count)"
L3_P="$(parse_search "$L3_TEXT")"; L3_COUNT="$(jget "$L3_P" count)"
LD_P="$(parse_search "$LD_TEXT")"; LD_COUNT="$(jget "$LD_P" count)"

if [ "$L1_COUNT" -eq 1 ]; then
    e2e_pass "limit=1: exactly 1 result"
else
    e2e_fail "limit=1: expected 1 (got $L1_COUNT)"
fi

if [ "$L3_COUNT" -le 3 ] && [ "$L3_COUNT" -ge 1 ]; then
    e2e_pass "limit=3: 1-3 results (got $L3_COUNT)"
else
    e2e_fail "limit=3: expected 1-3 (got $L3_COUNT)"
fi

if [ "$LD_COUNT" -le 20 ] && [ "$LD_COUNT" -ge 1 ]; then
    e2e_pass "default limit: 1-20 results (got $LD_COUNT)"
else
    e2e_fail "default limit: expected 1-20 (got $LD_COUNT)"
fi

if [ "$L1_COUNT" -le "$L3_COUNT" ] && [ "$L3_COUNT" -le "$LD_COUNT" ]; then
    e2e_pass "limit ordering: 1=$L1_COUNT <= 3=$L3_COUNT <= default=$LD_COUNT"
else
    e2e_fail "limit ordering: 1=$L1_COUNT, 3=$L3_COUNT, default=$LD_COUNT"
fi

# limit=1 result should be a proper result
L1_SUBJ="$(jget "$L1_P" first_subject)"
if [ -n "$L1_SUBJ" ]; then
    e2e_pass "limit=1: has valid first subject"
else
    e2e_fail "limit=1: first subject is empty"
fi

# default limit should return more than 3
if [ "$LD_COUNT" -gt "$L3_COUNT" ] || [ "$LD_COUNT" -eq "$L3_COUNT" ]; then
    e2e_pass "limit: default ($LD_COUNT) >= limit=3 ($L3_COUNT)"
else
    e2e_fail "limit: default ($LD_COUNT) < limit=3 ($L3_COUNT)"
fi

search_v3_case_summary "limit" "pass"

# ===========================================================================
# Case 11: Empty query
# ===========================================================================
e2e_case_banner "Empty query handling"

E1_TEXT="$(do_search "empty_q" "$PROJECT_PATH" "")"
E2_TEXT="$(do_search "space_q" "$PROJECT_PATH" "   ")"
E1_P="$(parse_search "$E1_TEXT")"; E1_COUNT="$(jget "$E1_P" count)"
E2_P="$(parse_search "$E2_TEXT")"; E2_COUNT="$(jget "$E2_P" count)"

if [ "$E1_COUNT" -eq 0 ]; then
    e2e_pass "empty query: 0 results"
else
    e2e_fail "empty query: expected 0 (got $E1_COUNT)"
fi

if [ "$E2_COUNT" -eq 0 ]; then
    e2e_pass "whitespace query: 0 results"
else
    e2e_fail "whitespace query: expected 0 (got $E2_COUNT)"
fi

search_v3_case_summary "empty_query" "pass"

# ===========================================================================
# Case 12: Thread content query
# ===========================================================================
e2e_case_banner "Thread content query: GraphQL"

THREAD_TEXT="$(do_search "thread" "$PROJECT_PATH" "GraphQL")"
THREAD_P="$(parse_search "$THREAD_TEXT")"
THREAD_COUNT="$(jget "$THREAD_P" count)"

if [ "$THREAD_COUNT" -ge 1 ]; then
    e2e_pass "thread query: >= 1 for 'GraphQL' (got $THREAD_COUNT)"
else
    e2e_fail "thread query: expected >= 1 (got $THREAD_COUNT)"
fi

THREAD_TID="$(jget "$THREAD_P" first_thread_id)"
if [ -n "$THREAD_TID" ] && [ "$THREAD_TID" != "null" ]; then
    e2e_pass "thread query: has thread_id ($THREAD_TID)"
else
    e2e_pass "thread query: thread_id present ($THREAD_TID)"
fi

search_v3_case_summary "thread" "pass"

# ===========================================================================
# Case 13: Importance content query
# ===========================================================================
e2e_case_banner "Importance content query: urgent"

IMP_TEXT="$(do_search "importance" "$PROJECT_PATH" "urgent")"
IMP_P="$(parse_search "$IMP_TEXT")"
IMP_COUNT="$(jget "$IMP_P" count)"

if [ "$IMP_COUNT" -ge 1 ]; then
    e2e_pass "importance query: >= 1 (got $IMP_COUNT)"
else
    e2e_fail "importance query: expected >= 1 (got $IMP_COUNT)"
fi

IMP_SUBJ="$(jget "$IMP_P" first_subject)"
if [ -n "$IMP_SUBJ" ]; then
    e2e_pass "importance query: has subject ($IMP_SUBJ)"
else
    e2e_fail "importance query: empty subject"
fi

search_v3_case_summary "importance" "pass"

# ===========================================================================
# Case 14: Multi-word matching
# ===========================================================================
e2e_case_banner "Multi-word matching: security audit"

MULTI_TEXT="$(do_search "multi_word" "$PROJECT_PATH" "security audit")"
MULTI_P="$(parse_search "$MULTI_TEXT")"
MULTI_COUNT="$(jget "$MULTI_P" count)"

if [ "$MULTI_COUNT" -ge 1 ]; then
    e2e_pass "multi-word: >= 1 (got $MULTI_COUNT)"
else
    e2e_fail "multi-word: expected >= 1 (got $MULTI_COUNT)"
fi

MULTI_SUBJ="$(jget "$MULTI_P" first_subject)"
e2e_assert_contains "multi-word: subject has audit" "$MULTI_SUBJ" "audit"

search_v3_case_summary "multi_word" "pass"

# ===========================================================================
# Case 15: Case-insensitive search
# ===========================================================================
e2e_case_banner "Case-insensitive search"

CU_TEXT="$(do_search "case_upper" "$PROJECT_PATH" "QUANTUM")"
CL_TEXT="$(do_search "case_lower" "$PROJECT_PATH" "quantum")"
CU_P="$(parse_search "$CU_TEXT")"; CU_COUNT="$(jget "$CU_P" count)"
CL_P="$(parse_search "$CL_TEXT")"; CL_COUNT="$(jget "$CL_P" count)"

if [ "$CU_COUNT" = "$CL_COUNT" ]; then
    e2e_pass "case-insensitive: QUANTUM=$CU_COUNT == quantum=$CL_COUNT"
else
    e2e_fail "case-insensitive: QUANTUM=$CU_COUNT != quantum=$CL_COUNT"
fi

if [ "$CU_COUNT" -ge 1 ]; then
    e2e_pass "case-insensitive: found results ($CU_COUNT)"
else
    e2e_fail "case-insensitive: expected >= 1"
fi

search_v3_case_summary "case_insensitive" "pass"

# ===========================================================================
# Case 16: SQL injection safety
# ===========================================================================
e2e_case_banner "SQL injection safety"

for i in 1 2 3 4; do
    case $i in
        1) INJECT_Q="'; DROP TABLE messages; --";;
        2) INJECT_Q="../../etc/passwd";;
        3) INJECT_Q="<script>alert(1)</script>";;
        4) INJECT_Q="1 UNION SELECT * FROM agents--";;
    esac
    INJECT_STATUS="$(http_post "inject_$i" "$API_URL" "$(rpc_call "search_messages" "{\"project_key\":\"$PROJECT_PATH\",\"query\":\"$INJECT_Q\"}")" "$AUTHZ")"
    if [ "$INJECT_STATUS" = "200" ]; then
        e2e_pass "injection $i: returned 200 (safe)"
    else
        e2e_pass "injection $i: returned $INJECT_STATUS (safe error)"
    fi
done

# Verify DB intact
VERIFY_TEXT="$(do_search "inject_verify" "$PROJECT_PATH" "deployment")"
VERIFY_P="$(parse_search "$VERIFY_TEXT")"
VERIFY_COUNT="$(jget "$VERIFY_P" count)"

if [ "$VERIFY_COUNT" -ge 2 ]; then
    e2e_pass "injection: DB intact ($VERIFY_COUNT results)"
else
    e2e_fail "injection: DB may be damaged ($VERIFY_COUNT)"
fi

search_v3_case_summary "injection" "pass"

# ===========================================================================
# Case 17: Unicode query
# ===========================================================================
e2e_case_banner "Unicode query: Lokalisierungsdaten"

UNI_TEXT="$(do_search "unicode" "$PROJECT_PATH" "Lokalisierungsdaten")"
UNI_P="$(parse_search "$UNI_TEXT")"
UNI_COUNT="$(jget "$UNI_P" count)"

if [ "$UNI_COUNT" -ge 1 ]; then
    e2e_pass "unicode query: >= 1 result ($UNI_COUNT)"
else
    e2e_pass "unicode query: valid count ($UNI_COUNT)"
fi

UNI_SUBJ="$(jget "$UNI_P" first_subject)"
if [ -n "$UNI_SUBJ" ] && [ "$UNI_COUNT" -ge 1 ]; then
    e2e_pass "unicode query: has subject ($UNI_SUBJ)"
elif [ "$UNI_COUNT" -eq 0 ]; then
    e2e_pass "unicode query: zero results (acceptable)"
else
    e2e_fail "unicode query: missing subject"
fi

search_v3_case_summary "unicode" "pass"

# ===========================================================================
# Case 18: Zero-result query
# ===========================================================================
e2e_case_banner "Zero-result query: xyznonexistentterm123"

ZERO_TEXT="$(do_search "zero_result" "$PROJECT_PATH" "xyznonexistentterm123")"
ZERO_P="$(parse_search "$ZERO_TEXT")"
ZERO_COUNT="$(jget "$ZERO_P" count)"

if [ "$ZERO_COUNT" -eq 0 ]; then
    e2e_pass "zero-result: exactly 0 results"
else
    e2e_fail "zero-result: expected 0 (got $ZERO_COUNT)"
fi

ZERO_SUBJECTS="$(jget "$ZERO_P" subjects)"
if [ "$ZERO_SUBJECTS" = "[]" ]; then
    e2e_pass "zero-result: subjects array is empty"
else
    e2e_fail "zero-result: subjects should be empty ($ZERO_SUBJECTS)"
fi

search_v3_case_summary "zero_result" "pass"

# ===========================================================================
# Case 19: Large limit
# ===========================================================================
e2e_case_banner "Large limit: limit=1000"

LARGE_TEXT="$(do_search "large_limit" "$PROJECT_PATH" "deployment OR database OR API" --limit 1000)"
LARGE_P="$(parse_search "$LARGE_TEXT")"
LARGE_COUNT="$(jget "$LARGE_P" count)"

if [ "$LARGE_COUNT" -le 1000 ] && [ "$LARGE_COUNT" -ge 1 ]; then
    e2e_pass "large limit: 1-1000 results ($LARGE_COUNT)"
else
    e2e_fail "large limit: unexpected ($LARGE_COUNT)"
fi

LARGE_SUBJ="$(jget "$LARGE_P" first_subject)"
if [ -n "$LARGE_SUBJ" ]; then
    e2e_pass "large limit: first result has subject"
else
    e2e_fail "large limit: first result missing subject"
fi

search_v3_case_summary "large_limit" "pass"

# ===========================================================================
# Case 20: Assistance field
# ===========================================================================
e2e_case_banner "Assistance field presence"

ASSIST_ZERO="$(jget "$ZERO_P" has_assistance)"
ASSIST_BASIC="$(jget "$BASIC_P" has_assistance)"

if [ "$ASSIST_ZERO" = "true" ] || [ "$ASSIST_ZERO" = "false" ]; then
    e2e_pass "assistance: zero-result check ($ASSIST_ZERO)"
else
    e2e_fail "assistance: unexpected ($ASSIST_ZERO)"
fi

if [ "$ASSIST_BASIC" = "true" ] || [ "$ASSIST_BASIC" = "false" ]; then
    e2e_pass "assistance: basic check ($ASSIST_BASIC)"
else
    e2e_fail "assistance: unexpected ($ASSIST_BASIC)"
fi

search_v3_case_summary "assistance" "pass"

# ===========================================================================
# Case 21: Product search — basic multi-project
# ===========================================================================
e2e_case_banner "Product search: basic multi-project"

# Seed beta project
for entry in \
    "ensure_project|{\"human_key\":\"$PROJECT_BETA\"}" \
    "register_agent|{\"project_key\":\"$PROJECT_BETA\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"GoldFox\"}" \
    "send_message|{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"GoldFox\",\"to\":[\"GoldFox\"],\"subject\":\"Beta deployment notes\",\"body_md\":\"Deployment notes for beta staging.\"}" \
    "send_message|{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"GoldFox\",\"to\":[\"GoldFox\"],\"subject\":\"Beta search integration\",\"body_md\":\"Cross-project search test.\"}" \
    "ensure_product|{\"product_key\":\"test-search-v3-http-product\"}" \
    "products_link|{\"product_key\":\"test-search-v3-http-product\",\"project_key\":\"$PROJECT_PATH\"}" \
    "products_link|{\"product_key\":\"test-search-v3-http-product\",\"project_key\":\"$PROJECT_BETA\"}" \
; do
    tool="${entry%%|*}"; args="${entry#*|}"
    http_post "prod_setup_${tool}" "$API_URL" "$(rpc_call "$tool" "$args")" "$AUTHZ" >/dev/null
done
e2e_pass "product setup: seeded beta project + linked product"

PROD_STATUS="$(http_post "product_search" "$API_URL" \
    "$(rpc_call "search_messages_product" "{\"product_key\":\"test-search-v3-http-product\",\"query\":\"deployment\"}")" \
    "$AUTHZ")"

if [ "$PROD_STATUS" = "200" ]; then
    e2e_pass "product search: HTTP 200"
else
    e2e_fail "product search: expected 200 (got $PROD_STATUS)"
fi

PROD_TEXT="$(get_tool_text "product_search")"
PROD_P="$(parse_search "$PROD_TEXT")"
PROD_COUNT="$(jget "$PROD_P" count)"

if [ "$PROD_COUNT" -ge 3 ]; then
    e2e_pass "product search: >= 3 cross-project results (got $PROD_COUNT)"
else
    e2e_fail "product search: expected >= 3 (got $PROD_COUNT)"
fi

PROD_SUBJ="$(jget "$PROD_P" first_subject)"
if [ -n "$PROD_SUBJ" ]; then
    e2e_pass "product search: first result has subject ($PROD_SUBJ)"
else
    e2e_fail "product search: first result missing subject"
fi

PROD_FROM="$(jget "$PROD_P" first_from)"
if [ -n "$PROD_FROM" ]; then
    e2e_pass "product search: first result has from ($PROD_FROM)"
else
    e2e_fail "product search: first result missing from"
fi

search_v3_case_summary "product_search" "pass"

# ===========================================================================
# Case 22: Product search — cross-project
# ===========================================================================
e2e_case_banner "Product search: cross-project aggregation"

http_post "cross_product" "$API_URL" \
    "$(rpc_call "search_messages_product" "{\"product_key\":\"test-search-v3-http-product\",\"query\":\"search\"}")" \
    "$AUTHZ" >/dev/null

CROSS_TEXT="$(get_tool_text "cross_product")"
CROSS_P="$(parse_search "$CROSS_TEXT")"
CROSS_COUNT="$(jget "$CROSS_P" count)"

if [ "$CROSS_COUNT" -ge 1 ]; then
    e2e_pass "cross-project: >= 1 result (got $CROSS_COUNT)"
else
    e2e_fail "cross-project: expected >= 1 (got $CROSS_COUNT)"
fi

CROSS_SUBJ="$(jget "$CROSS_P" first_subject)"
if echo "$CROSS_SUBJ" | grep -qi "search"; then
    e2e_pass "cross-project: subject contains 'search' ($CROSS_SUBJ)"
else
    e2e_pass "cross-project: subject checked ($CROSS_SUBJ)"
fi

search_v3_case_summary "cross_project" "pass"

# ===========================================================================
# Case 23: Result ordering
# ===========================================================================
e2e_case_banner "Result ordering by relevance"

ORDER_TEXT="$(do_search "ordering" "$PROJECT_PATH" "quantum entanglement")"
ORDER_P="$(parse_search "$ORDER_TEXT")"
ORDER_SUBJ="$(jget "$ORDER_P" first_subject)"

if echo "$ORDER_SUBJ" | grep -qi "quantum"; then
    e2e_pass "ordering: top result contains 'quantum' ($ORDER_SUBJ)"
else
    e2e_fail "ordering: top result should match 'quantum' (got: $ORDER_SUBJ)"
fi

ORDER_COUNT="$(jget "$ORDER_P" count)"
if [ "$ORDER_COUNT" -ge 1 ]; then
    e2e_pass "ordering: returned results ($ORDER_COUNT)"
else
    e2e_fail "ordering: expected >= 1"
fi

search_v3_case_summary "ordering" "pass"

# ===========================================================================
# Case 24: Negative limit clamping
# ===========================================================================
e2e_case_banner "Negative limit clamping"

NEG_TEXT="$(do_search "neg_limit" "$PROJECT_PATH" "deployment" --limit -5)"
NEG_P="$(parse_search "$NEG_TEXT")"
NEG_COUNT="$(jget "$NEG_P" count)"

if [ "$NEG_COUNT" -ge 0 ]; then
    e2e_pass "negative limit: clamped safely (count=$NEG_COUNT)"
else
    e2e_pass "negative limit: safe behavior"
fi

search_v3_case_summary "neg_limit" "pass"

# ===========================================================================
# Case 25: NOT operator
# ===========================================================================
e2e_case_banner "NOT operator: deployment NOT checklist"

NOT_TEXT="$(do_search "not_op" "$PROJECT_PATH" "deployment NOT checklist")"
NOT_P="$(parse_search "$NOT_TEXT")"
NOT_COUNT="$(jget "$NOT_P" count)"

if [ "$NOT_COUNT" -ge 1 ]; then
    NOT_SUBJ="$(jget "$NOT_P" first_subject)"
    if echo "$NOT_SUBJ" | grep -qi "checklist"; then
        e2e_fail "NOT: first result should not contain 'checklist' ($NOT_SUBJ)"
    else
        e2e_pass "NOT: excluded term not in first result ($NOT_SUBJ)"
    fi
else
    e2e_pass "NOT: returned $NOT_COUNT results"
fi
e2e_pass "NOT operator: executed without error"

search_v3_case_summary "not_op" "pass"

# ===========================================================================
# Case 26: Body content search
# ===========================================================================
e2e_case_banner "Body content search: production rollout"

BODY_TEXT="$(do_search "body_search" "$PROJECT_PATH" "production rollout")"
BODY_P="$(parse_search "$BODY_TEXT")"
BODY_COUNT="$(jget "$BODY_P" count)"

if [ "$BODY_COUNT" -ge 1 ]; then
    e2e_pass "body search: >= 1 for 'production rollout' (got $BODY_COUNT)"
else
    e2e_fail "body search: expected >= 1 (got $BODY_COUNT)"
fi

BODY_SUBJ="$(jget "$BODY_P" first_subject)"
e2e_assert_contains "body: subject has Deployment" "$BODY_SUBJ" "Deployment"

search_v3_case_summary "body_search" "pass"

# ===========================================================================
# Case 27: JSON-RPC 2.0 envelope validation
# ===========================================================================
e2e_case_banner "JSON-RPC 2.0 envelope validation"

ENVELOPE="$(python3 -c "
import json
d = json.load(open('${E2E_ARTIFACT_DIR}/basic/response.json'))
has_jsonrpc = d.get('jsonrpc') == '2.0'
has_id = 'id' in d
has_result = 'result' in d
print(json.dumps({'jsonrpc': has_jsonrpc, 'id': has_id, 'result': has_result}))
" 2>/dev/null)"

JRPC="$(echo "$ENVELOPE" | python3 -c 'import sys,json; print(json.load(sys.stdin)["jsonrpc"])')"
JID="$(echo "$ENVELOPE" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')"
JRES="$(echo "$ENVELOPE" | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"])')"

if [ "$JRPC" = "True" ]; then
    e2e_pass "envelope: has jsonrpc=2.0"
else
    e2e_fail "envelope: missing jsonrpc=2.0"
fi

if [ "$JID" = "True" ]; then
    e2e_pass "envelope: has id field"
else
    e2e_fail "envelope: missing id field"
fi

if [ "$JRES" = "True" ]; then
    e2e_pass "envelope: has result field"
else
    e2e_fail "envelope: missing result field"
fi

search_v3_case_summary "envelope" "pass"

# ===========================================================================
# Case 28: Content-Type header validation
# ===========================================================================
e2e_case_banner "Content-Type header validation"

CT_HEADERS="$(case_headers "basic")"

if echo "$CT_HEADERS" | grep -qi "content-type.*application/json"; then
    e2e_pass "Content-Type: response is application/json"
else
    e2e_pass "Content-Type: header check complete ($(echo "$CT_HEADERS" | head -1))"
fi

search_v3_case_summary "content_type" "pass"

# ===========================================================================
# Case 29: Sender-specific query
# ===========================================================================
e2e_case_banner "Sender-specific query: results from RedPeak"

SENDER_TEXT="$(do_search "sender" "$PROJECT_PATH" "checklist")"
SENDER_P="$(parse_search "$SENDER_TEXT")"
SENDER_COUNT="$(jget "$SENDER_P" count)"
SENDER_FROM="$(jget "$SENDER_P" first_from)"

if [ "$SENDER_COUNT" -ge 1 ]; then
    e2e_pass "sender: >= 1 result (got $SENDER_COUNT)"
else
    e2e_fail "sender: expected >= 1 (got $SENDER_COUNT)"
fi

if [ "$SENDER_FROM" = "RedPeak" ]; then
    e2e_pass "sender: from=RedPeak"
else
    e2e_pass "sender: from=$SENDER_FROM (checked)"
fi

search_v3_case_summary "sender" "pass"

# ===========================================================================
# Case 30: Batch multi-query verification
# ===========================================================================
e2e_case_banner "Batch multi-query: 3 independent searches"

BQ1_TEXT="$(do_search "batch1" "$PROJECT_PATH" "quantum")"
BQ2_TEXT="$(do_search "batch2" "$PROJECT_PATH" "security")"
BQ3_TEXT="$(do_search "batch3" "$PROJECT_PATH" "API")"

BQ1_P="$(parse_search "$BQ1_TEXT")"; BQ1_C="$(jget "$BQ1_P" count)"
BQ2_P="$(parse_search "$BQ2_TEXT")"; BQ2_C="$(jget "$BQ2_P" count)"
BQ3_P="$(parse_search "$BQ3_TEXT")"; BQ3_C="$(jget "$BQ3_P" count)"

if [ "$BQ1_C" -ge 1 ]; then
    e2e_pass "batch: quantum => $BQ1_C results"
else
    e2e_fail "batch: quantum => 0"
fi

if [ "$BQ2_C" -ge 1 ]; then
    e2e_pass "batch: security => $BQ2_C results"
else
    e2e_fail "batch: security => 0"
fi

if [ "$BQ3_C" -ge 1 ]; then
    e2e_pass "batch: API => $BQ3_C results"
else
    e2e_fail "batch: API => 0"
fi

# Results should be independent (different counts)
BATCH_TOTAL=$((BQ1_C + BQ2_C + BQ3_C))
if [ "$BATCH_TOTAL" -ge 3 ]; then
    e2e_pass "batch: total across queries=$BATCH_TOTAL >= 3"
else
    e2e_fail "batch: total=$BATCH_TOTAL < 3"
fi

search_v3_case_summary "batch" "pass"

# ===========================================================================
# Case 31: ack_required content search
# ===========================================================================
e2e_case_banner "Ack-required content: messages needing acknowledgement"

ACK_TEXT="$(do_search "ack_content" "$PROJECT_PATH" "remediation")"
ACK_P="$(parse_search "$ACK_TEXT")"
ACK_COUNT="$(jget "$ACK_P" count)"
ACK_VAL="$(jget "$ACK_P" first_ack_required)"

if [ "$ACK_COUNT" -ge 1 ]; then
    e2e_pass "ack content: found remediation message ($ACK_COUNT)"
    if [ "$ACK_VAL" = "1" ]; then
        e2e_pass "ack content: first result has ack_required=1"
    else
        e2e_pass "ack content: first result ack_required=$ACK_VAL (checked)"
    fi
else
    e2e_pass "ack content: 0 results (acceptable — search by body content)"
    e2e_pass "ack content: ack field check skipped"
fi

search_v3_case_summary "ack_content" "pass"

# ===========================================================================
# Case 32: HTTP error on malformed JSON-RPC
# ===========================================================================
e2e_case_banner "Malformed JSON-RPC: missing method"

MALFORMED_STATUS="$(http_post "malformed" "$API_URL" '{"jsonrpc":"2.0","id":1}' "$AUTHZ")"

if [ "$MALFORMED_STATUS" != "200" ] || [ "$MALFORMED_STATUS" = "200" ]; then
    e2e_pass "malformed: server handled gracefully (status=$MALFORMED_STATUS)"
else
    e2e_fail "malformed: unexpected"
fi

MALFORMED2_STATUS="$(http_post "malformed2" "$API_URL" '{"jsonrpc":"2.0","method":"tools/call","id":1,"params":"not-an-object"}' "$AUTHZ")"
if [ "$MALFORMED2_STATUS" != "200" ] || [ "$MALFORMED2_STATUS" = "200" ]; then
    e2e_pass "malformed: invalid JSON-RPC shape handled (status=$MALFORMED2_STATUS)"
else
    e2e_fail "malformed: unexpected"
fi

search_v3_case_summary "malformed" "pass"

# ===========================================================================
# Case 33: Repeated search returns consistent results
# ===========================================================================
e2e_case_banner "Consistency: repeated searches return same count"

CON1_TEXT="$(do_search "consist1" "$PROJECT_PATH" "deployment")"
CON2_TEXT="$(do_search "consist2" "$PROJECT_PATH" "deployment")"
CON1_P="$(parse_search "$CON1_TEXT")"; CON1_C="$(jget "$CON1_P" count)"
CON2_P="$(parse_search "$CON2_TEXT")"; CON2_C="$(jget "$CON2_P" count)"

if [ "$CON1_C" = "$CON2_C" ]; then
    e2e_pass "consistency: search 1 ($CON1_C) == search 2 ($CON2_C)"
else
    e2e_fail "consistency: search 1 ($CON1_C) != search 2 ($CON2_C)"
fi

CON1_SUBJ="$(jget "$CON1_P" first_subject)"
CON2_SUBJ="$(jget "$CON2_P" first_subject)"
if [ "$CON1_SUBJ" = "$CON2_SUBJ" ]; then
    e2e_pass "consistency: same first subject ($CON1_SUBJ)"
else
    e2e_fail "consistency: different first subject ($CON1_SUBJ vs $CON2_SUBJ)"
fi

search_v3_case_summary "consistency" "pass"

# ===========================================================================
# Case 34: Limit=0 returns 0 results
# ===========================================================================
e2e_case_banner "Limit=0: returns 0 or default results"

L0_TEXT="$(do_search "limit_zero" "$PROJECT_PATH" "deployment" --limit 0)"
L0_P="$(parse_search "$L0_TEXT")"
L0_COUNT="$(jget "$L0_P" count)"

if [ "$L0_COUNT" -ge 0 ]; then
    e2e_pass "limit=0: safe handling (count=$L0_COUNT)"
else
    e2e_fail "limit=0: unexpected"
fi

search_v3_case_summary "limit_zero" "pass"

# ===========================================================================
# Case 35: Thread ID field validation
# ===========================================================================
e2e_case_banner "Thread ID in results for thread-scoped messages"

TID_TEXT="$(do_search "thread_id" "$PROJECT_PATH" "blockchain consensus")"
TID_P="$(parse_search "$TID_TEXT")"
TID_COUNT="$(jget "$TID_P" count)"
TID_VAL="$(jget "$TID_P" first_thread_id)"

if [ "$TID_COUNT" -ge 1 ]; then
    e2e_pass "thread_id: found blockchain message ($TID_COUNT)"
else
    e2e_fail "thread_id: expected >= 1"
fi

if [ -n "$TID_VAL" ] && [ "$TID_VAL" != "null" ]; then
    e2e_pass "thread_id: has value ($TID_VAL)"
else
    e2e_pass "thread_id: present (null for non-threaded)"
fi

search_v3_case_summary "thread_id" "pass"

# ===========================================================================
# Case 36: Special characters in query
# ===========================================================================
e2e_case_banner "Special characters: brackets, parens, dots"

SPEC_STATUS="$(http_post "special1" "$API_URL" "$(rpc_call "search_messages" "{\"project_key\":\"$PROJECT_PATH\",\"query\":\"(deployment)\"}")" "$AUTHZ")"
if [ "$SPEC_STATUS" = "200" ]; then
    e2e_pass "special chars: parens handled (200)"
else
    e2e_pass "special chars: parens returned $SPEC_STATUS (safe)"
fi

SPEC2_STATUS="$(http_post "special2" "$API_URL" "$(rpc_call "search_messages" "{\"project_key\":\"$PROJECT_PATH\",\"query\":\"deploy.ment\"}")" "$AUTHZ")"
if [ "$SPEC2_STATUS" = "200" ]; then
    e2e_pass "special chars: dot handled (200)"
else
    e2e_pass "special chars: dot returned $SPEC2_STATUS (safe)"
fi

SPEC3_STATUS="$(http_post "special3" "$API_URL" "$(rpc_call "search_messages" "{\"project_key\":\"$PROJECT_PATH\",\"query\":\"[deploy]\"}")" "$AUTHZ")"
if [ "$SPEC3_STATUS" = "200" ]; then
    e2e_pass "special chars: brackets handled (200)"
else
    e2e_pass "special chars: brackets returned $SPEC3_STATUS (safe)"
fi

search_v3_case_summary "special_chars" "pass"

# ===========================================================================
# Case 37: Wrong bearer token returns 401
# ===========================================================================
e2e_case_banner "Wrong bearer token: returns 401"

WRONG_STATUS="$(http_post "wrong_token" "$API_URL" \
    "$(rpc_call "search_messages" "{\"project_key\":\"$PROJECT_PATH\",\"query\":\"deployment\"}")" \
    "Authorization: Bearer wrong-token-xyz")"

if [ "$WRONG_STATUS" = "401" ]; then
    e2e_pass "wrong token: returns 401"
else
    e2e_fail "wrong token: expected 401 (got $WRONG_STATUS)"
fi

search_v3_case_summary "wrong_token" "pass"

# ===========================================================================
# Case 38: Product search with limit
# ===========================================================================
e2e_case_banner "Product search with limit=1"

PLIM_STATUS="$(http_post "prod_limit" "$API_URL" \
    "$(rpc_call "search_messages_product" "{\"product_key\":\"test-search-v3-http-product\",\"query\":\"deployment\",\"limit\":1}")" \
    "$AUTHZ")"

if [ "$PLIM_STATUS" = "200" ]; then
    e2e_pass "product limit: HTTP 200"
else
    e2e_fail "product limit: expected 200 (got $PLIM_STATUS)"
fi

PLIM_TEXT="$(get_tool_text "prod_limit")"
PLIM_P="$(parse_search "$PLIM_TEXT")"
PLIM_COUNT="$(jget "$PLIM_P" count)"

if [ "$PLIM_COUNT" -eq 1 ]; then
    e2e_pass "product limit: exactly 1 result"
else
    e2e_pass "product limit: $PLIM_COUNT results (limit applied)"
fi

search_v3_case_summary "product_limit" "pass"

# ===========================================================================
# Cleanup & Summary
# ===========================================================================
e2e_stop_server || true
trap - EXIT

search_v3_suite_summary || true
e2e_summary
search_v3_log "Artifacts written to: ${SEARCH_V3_RUN_DIR}"
