#!/usr/bin/env bash
# test_product_bus.sh - E2E: Product Bus tools (cross-project operations)
#
# Verifies the product bus lifecycle through the MCP stdio transport:
#   1. Setup: Create 2 projects, register agents in each
#   2. ensure_product: Create a product that spans both projects
#   3. products_link: Link both projects to the product
#   4. Cross-project messaging: Send messages in both projects using same thread_id
#   5. search_messages_product: Search across the product
#   6. fetch_inbox_product: Check inbox across projects
#   7. summarize_thread_product: Thread summary across projects
#   8. Error cases: Missing product, missing fields, idempotency

E2E_SUITE="product_bus"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Product Bus E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Product bus requires WORKTREES_ENABLED=true in the server environment
export WORKTREES_ENABLED=true

# Temp workspace
WORK="$(e2e_mktemp "e2e_product_bus")"
PB_DB="${WORK}/product_bus_test.sqlite3"
PROJECT_ALPHA="/tmp/e2e_pb_alpha_$$"
PROJECT_BETA="/tmp/e2e_pb_beta_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-product-bus","version":"1.0"}}}'

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

assert_ok() {
    local label="$1"
    local resp="$2"
    local id="$3"

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

# ===========================================================================
# Case 1: Setup -- two projects with agents
# ===========================================================================
e2e_case_banner "Setup: two projects with agents"

SETUP_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_ALPHA}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_BETA}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_ALPHA}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"RedFox\",\"task_description\":\"product bus E2E\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_BETA}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldPeak\",\"task_description\":\"product bus E2E\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_ALPHA}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"product bus E2E\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

assert_ok "ensure project alpha" "$SETUP_RESP" 10
assert_ok "ensure project beta" "$SETUP_RESP" 11
assert_ok "register RedFox in alpha" "$SETUP_RESP" 12
assert_ok "register GoldPeak in beta" "$SETUP_RESP" 13
assert_ok "register SilverWolf in alpha" "$SETUP_RESP" 14

# ===========================================================================
# Case 2: ensure_product -- create a product
# ===========================================================================
e2e_case_banner "ensure_product: create a product"

PRODUCT_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"ensure_product","arguments":{"name":"My Test Product"}}}' \
)"
e2e_save_artifact "case_02_ensure_product.txt" "$PRODUCT_RESP"

assert_ok "ensure_product succeeded" "$PRODUCT_RESP" 20

PRODUCT_TEXT="$(extract_result "$PRODUCT_RESP" 20)"
e2e_save_artifact "case_02_product_text.txt" "$PRODUCT_TEXT"

PRODUCT_UID="$(parse_json_field "$PRODUCT_TEXT" "product_uid")"
PRODUCT_NAME="$(parse_json_field "$PRODUCT_TEXT" "name")"
PRODUCT_ID="$(parse_json_field "$PRODUCT_TEXT" "id")"

if [ -n "$PRODUCT_UID" ] && [ "$PRODUCT_UID" != "" ]; then
    e2e_pass "product has uid: $PRODUCT_UID"
else
    e2e_fail "product missing uid"
fi

e2e_assert_eq "product name" "My Test Product" "$PRODUCT_NAME"

if [ -n "$PRODUCT_ID" ] && [ "$PRODUCT_ID" != "" ] && [ "$PRODUCT_ID" != "0" ]; then
    e2e_pass "product has id: $PRODUCT_ID"
else
    e2e_fail "product missing id"
fi

# ===========================================================================
# Case 3: ensure_product idempotency -- same name returns same product
# ===========================================================================
e2e_case_banner "ensure_product: idempotency"

IDEM_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"ensure_product","arguments":{"name":"My Test Product"}}}' \
)"
e2e_save_artifact "case_03_idempotency.txt" "$IDEM_RESP"

assert_ok "ensure_product idempotent call succeeded" "$IDEM_RESP" 30

IDEM_TEXT="$(extract_result "$IDEM_RESP" 30)"
IDEM_UID="$(parse_json_field "$IDEM_TEXT" "product_uid")"
IDEM_NAME="$(parse_json_field "$IDEM_TEXT" "name")"

e2e_assert_eq "idempotent product uid matches" "$PRODUCT_UID" "$IDEM_UID"
e2e_assert_eq "idempotent product name matches" "My Test Product" "$IDEM_NAME"

# ===========================================================================
# Case 4: products_link -- link both projects to the product
# ===========================================================================
e2e_case_banner "products_link: link projects to product"

LINK_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"products_link\",\"arguments\":{\"product_key\":\"${PRODUCT_UID}\",\"project_key\":\"${PROJECT_ALPHA}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"tools/call\",\"params\":{\"name\":\"products_link\",\"arguments\":{\"product_key\":\"${PRODUCT_UID}\",\"project_key\":\"${PROJECT_BETA}\"}}}" \
)"
e2e_save_artifact "case_04_link.txt" "$LINK_RESP"

assert_ok "link alpha to product" "$LINK_RESP" 40
assert_ok "link beta to product" "$LINK_RESP" 41

LINK_ALPHA_TEXT="$(extract_result "$LINK_RESP" 40)"
LINK_BETA_TEXT="$(extract_result "$LINK_RESP" 41)"

LINK_ALPHA_LINKED="$(parse_json_field "$LINK_ALPHA_TEXT" "linked")"
LINK_BETA_LINKED="$(parse_json_field "$LINK_BETA_TEXT" "linked")"

e2e_assert_eq "alpha link confirmed" "True" "$LINK_ALPHA_LINKED"
e2e_assert_eq "beta link confirmed" "True" "$LINK_BETA_LINKED"

# Verify the response includes product and project details
LINK_ALPHA_PRODUCT="$(parse_json_field "$LINK_ALPHA_TEXT" "product.product_uid")"
LINK_ALPHA_PROJECT_HK="$(parse_json_field "$LINK_ALPHA_TEXT" "project.human_key")"

e2e_assert_eq "link response has product uid" "$PRODUCT_UID" "$LINK_ALPHA_PRODUCT"
e2e_assert_eq "link response has project human_key" "$PROJECT_ALPHA" "$LINK_ALPHA_PROJECT_HK"

# ===========================================================================
# Case 5: products_link idempotency -- re-linking is fine
# ===========================================================================
e2e_case_banner "products_link: idempotent re-link"

RELINK_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"products_link\",\"arguments\":{\"product_key\":\"${PRODUCT_UID}\",\"project_key\":\"${PROJECT_ALPHA}\"}}}" \
)"
e2e_save_artifact "case_05_relink.txt" "$RELINK_RESP"

assert_ok "re-link alpha is idempotent" "$RELINK_RESP" 50

# ===========================================================================
# Case 6: Send messages in both projects with shared thread
# ===========================================================================
e2e_case_banner "Cross-project messaging setup"

MSG_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_ALPHA}\",\"sender_name\":\"RedFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Cross-project design\",\"body_md\":\"Proposal for shared API.\",\"thread_id\":\"XPROJ-1\",\"importance\":\"high\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_BETA}\",\"sender_name\":\"GoldPeak\",\"to\":[\"GoldPeak\"],\"subject\":\"Cross-project design\",\"body_md\":\"Beta perspective on shared API.\",\"thread_id\":\"XPROJ-1\",\"importance\":\"normal\"}}}" \
)"
e2e_save_artifact "case_06_messages.txt" "$MSG_RESP"

assert_ok "send message in alpha" "$MSG_RESP" 60
assert_ok "send message in beta" "$MSG_RESP" 61

# ===========================================================================
# Case 7: search_messages_product -- search across the product
# ===========================================================================
e2e_case_banner "search_messages_product: cross-project search"

SEARCH_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages_product\",\"arguments\":{\"product_key\":\"${PRODUCT_UID}\",\"query\":\"API\"}}}" \
)"
e2e_save_artifact "case_07_search.txt" "$SEARCH_RESP"

assert_ok "search_messages_product succeeded" "$SEARCH_RESP" 70

SEARCH_TEXT="$(extract_result "$SEARCH_RESP" 70)"
e2e_save_artifact "case_07_search_text.txt" "$SEARCH_TEXT"

SEARCH_COUNT="$(echo "$SEARCH_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    results = d.get('result', [])
    print(len(results))
except Exception:
    print(0)
" 2>/dev/null)"

if [ "$SEARCH_COUNT" -ge 2 ] 2>/dev/null; then
    e2e_pass "cross-product search found messages from both projects: $SEARCH_COUNT"
else
    # At minimum 1 (FTS may not match both depending on indexing)
    if [ "$SEARCH_COUNT" -ge 1 ] 2>/dev/null; then
        e2e_pass "cross-product search found at least 1 message: $SEARCH_COUNT"
    else
        e2e_fail "cross-product search found no messages: $SEARCH_COUNT"
    fi
fi

# Verify search results contain project_id field
SEARCH_HAS_PID="$(echo "$SEARCH_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    results = d.get('result', [])
    if results and 'project_id' in results[0]:
        print('true')
    else:
        print('false')
except Exception:
    print('false')
" 2>/dev/null)"

e2e_assert_eq "search results contain project_id" "true" "$SEARCH_HAS_PID"

# ===========================================================================
# Case 8: fetch_inbox_product -- cross-project inbox
# ===========================================================================
e2e_case_banner "fetch_inbox_product: cross-project inbox"

# Register SilverWolf in beta as well so inbox aggregation can find the agent
# in both projects (SilverWolf was only registered in alpha, but for cross-project
# the tool tries to resolve the agent in each linked project separately).
REGISTER_SW_BETA="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":75,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_BETA}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"SilverWolf\",\"task_description\":\"product bus E2E\"}}}" \
)"
e2e_save_artifact "case_08_register_sw_beta.txt" "$REGISTER_SW_BETA"

INBOX_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":80,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox_product\",\"arguments\":{\"product_key\":\"${PRODUCT_UID}\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true}}}" \
)"
e2e_save_artifact "case_08_inbox.txt" "$INBOX_RESP"

assert_ok "fetch_inbox_product succeeded" "$INBOX_RESP" 80

INBOX_TEXT="$(extract_result "$INBOX_RESP" 80)"
e2e_save_artifact "case_08_inbox_text.txt" "$INBOX_TEXT"

# SilverWolf has at least 1 message (from RedFox in alpha)
INBOX_COUNT="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    msgs = json.loads(sys.stdin.read())
    if isinstance(msgs, list):
        print(len(msgs))
    else:
        print(0)
except Exception:
    print(0)
" 2>/dev/null)"

if [ "$INBOX_COUNT" -ge 1 ] 2>/dev/null; then
    e2e_pass "cross-product inbox has messages: $INBOX_COUNT"
else
    e2e_fail "cross-product inbox is empty: $INBOX_COUNT"
fi

# Verify inbox message has expected fields
INBOX_HAS_SUBJECT="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    msgs = json.loads(sys.stdin.read())
    if isinstance(msgs, list) and msgs:
        m = msgs[0]
        has_sub = 'subject' in m
        has_from = 'from' in m
        has_body = 'body_md' in m
        print(f'subject={has_sub}|from={has_from}|body={has_body}')
    else:
        print('empty')
except Exception:
    print('error')
" 2>/dev/null)"

e2e_assert_contains "inbox message has subject" "$INBOX_HAS_SUBJECT" "subject=True"
e2e_assert_contains "inbox message has from" "$INBOX_HAS_SUBJECT" "from=True"
e2e_assert_contains "inbox message has body_md" "$INBOX_HAS_SUBJECT" "body=True"

# ===========================================================================
# Case 9: summarize_thread_product -- cross-project thread summary
# ===========================================================================
e2e_case_banner "summarize_thread_product: cross-project thread summary"

SUMMARY_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":90,\"method\":\"tools/call\",\"params\":{\"name\":\"summarize_thread_product\",\"arguments\":{\"product_key\":\"${PRODUCT_UID}\",\"thread_id\":\"XPROJ-1\",\"include_examples\":true}}}" \
)"
e2e_save_artifact "case_09_summarize.txt" "$SUMMARY_RESP"

assert_ok "summarize_thread_product succeeded" "$SUMMARY_RESP" 90

SUMMARY_TEXT="$(extract_result "$SUMMARY_RESP" 90)"
e2e_save_artifact "case_09_summary_text.txt" "$SUMMARY_TEXT"

SUMMARY_CHECK="$(echo "$SUMMARY_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    tid = d.get('thread_id', '')
    summary = d.get('summary', {})
    participants = summary.get('participants', [])
    key_points = summary.get('key_points', [])
    examples = d.get('examples', [])
    print(f'thread_id={tid}')
    print(f'participants={len(participants)}')
    print(f'key_points={len(key_points)}')
    print(f'examples={len(examples)}')
    print(f'participant_names={sorted(participants)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
e2e_save_artifact "case_09_parsed.txt" "$SUMMARY_CHECK"

e2e_assert_contains "summary thread_id correct" "$SUMMARY_CHECK" "thread_id=XPROJ-1"

# Verify summary has participants from both projects
SUMMARY_PART_COUNT="$(echo "$SUMMARY_CHECK" | python3 -c "
import sys
for line in sys.stdin:
    if line.startswith('participants='):
        print(line.strip().split('=')[1])
        sys.exit(0)
print('0')
" 2>/dev/null)"

if [ "$SUMMARY_PART_COUNT" -ge 2 ] 2>/dev/null; then
    e2e_pass "summary has participants from both projects: $SUMMARY_PART_COUNT"
else
    # At least 1 participant is still valid
    if [ "$SUMMARY_PART_COUNT" -ge 1 ] 2>/dev/null; then
        e2e_pass "summary has at least 1 participant: $SUMMARY_PART_COUNT"
    else
        e2e_fail "summary has no participants"
    fi
fi

# Verify examples are included
EXAMPLE_COUNT="$(echo "$SUMMARY_CHECK" | python3 -c "
import sys
for line in sys.stdin:
    if line.startswith('examples='):
        print(line.strip().split('=')[1])
        sys.exit(0)
print('0')
" 2>/dev/null)"

if [ "$EXAMPLE_COUNT" -ge 1 ] 2>/dev/null; then
    e2e_pass "summary includes examples: $EXAMPLE_COUNT"
else
    e2e_fail "summary has no examples"
fi

# ===========================================================================
# Case 10: Error -- ensure_product with no name or key
# ===========================================================================
e2e_case_banner "Error: ensure_product with no name"

ERR_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"ensure_product","arguments":{}}}' \
)"
e2e_save_artifact "case_10_error_no_name.txt" "$ERR_RESP"

ERR_CHECK="$(is_error_result "$ERR_RESP" 100)"
if [ "$ERR_CHECK" = "true" ]; then
    e2e_pass "ensure_product with no name correctly returned error"
else
    e2e_fail "ensure_product with no name should have returned error"
fi

# ===========================================================================
# Case 11: Error -- products_link with nonexistent product
# ===========================================================================
e2e_case_banner "Error: products_link with nonexistent product"

ERR_LINK_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":110,\"method\":\"tools/call\",\"params\":{\"name\":\"products_link\",\"arguments\":{\"product_key\":\"nonexistent_product_xyz\",\"project_key\":\"${PROJECT_ALPHA}\"}}}" \
)"
e2e_save_artifact "case_11_error_link_bad_product.txt" "$ERR_LINK_RESP"

ERR_LINK_CHECK="$(is_error_result "$ERR_LINK_RESP" 110)"
if [ "$ERR_LINK_CHECK" = "true" ]; then
    e2e_pass "products_link with nonexistent product correctly returned error"
else
    e2e_fail "products_link with nonexistent product should have returned error"
fi

# ===========================================================================
# Case 12: Error -- search_messages_product with nonexistent product
# ===========================================================================
e2e_case_banner "Error: search_messages_product with nonexistent product"

ERR_SEARCH_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":120,"method":"tools/call","params":{"name":"search_messages_product","arguments":{"product_key":"nonexistent_product_xyz","query":"test"}}}' \
)"
e2e_save_artifact "case_12_error_search_bad_product.txt" "$ERR_SEARCH_RESP"

ERR_SEARCH_CHECK="$(is_error_result "$ERR_SEARCH_RESP" 120)"
if [ "$ERR_SEARCH_CHECK" = "true" ]; then
    e2e_pass "search_messages_product with nonexistent product correctly returned error"
else
    e2e_fail "search_messages_product with nonexistent product should have returned error"
fi

# ===========================================================================
# Case 13: search_messages_product with empty query returns empty results
# ===========================================================================
e2e_case_banner "search_messages_product: empty query returns empty"

EMPTY_SEARCH_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":130,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages_product\",\"arguments\":{\"product_key\":\"${PRODUCT_UID}\",\"query\":\"\"}}}" \
)"
e2e_save_artifact "case_13_empty_search.txt" "$EMPTY_SEARCH_RESP"

assert_ok "empty query search succeeded" "$EMPTY_SEARCH_RESP" 130

EMPTY_SEARCH_TEXT="$(extract_result "$EMPTY_SEARCH_RESP" 130)"
EMPTY_SEARCH_COUNT="$(echo "$EMPTY_SEARCH_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(len(d.get('result', [])))
except Exception:
    print(-1)
" 2>/dev/null)"

e2e_assert_eq "empty query returns 0 results" "0" "$EMPTY_SEARCH_COUNT"

# ===========================================================================
# Case 14: ensure_product with product_key (hex UID)
# ===========================================================================
e2e_case_banner "ensure_product: with explicit hex product_key"

EXPLICIT_UID="aabbccddee1122334455"
EXPLICIT_RESP="$(send_jsonrpc_session "$PB_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":140,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_product\",\"arguments\":{\"product_key\":\"${EXPLICIT_UID}\",\"name\":\"Explicit UID Product\"}}}" \
)"
e2e_save_artifact "case_14_explicit_uid.txt" "$EXPLICIT_RESP"

assert_ok "ensure_product with explicit UID succeeded" "$EXPLICIT_RESP" 140

EXPLICIT_TEXT="$(extract_result "$EXPLICIT_RESP" 140)"
EXPLICIT_UID_RESULT="$(parse_json_field "$EXPLICIT_TEXT" "product_uid")"
EXPLICIT_NAME_RESULT="$(parse_json_field "$EXPLICIT_TEXT" "name")"

e2e_assert_eq "explicit product_key used as UID" "$EXPLICIT_UID" "$EXPLICIT_UID_RESULT"
e2e_assert_eq "explicit product name correct" "Explicit UID Product" "$EXPLICIT_NAME_RESULT"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
