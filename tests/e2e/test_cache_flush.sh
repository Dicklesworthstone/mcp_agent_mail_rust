#!/usr/bin/env bash
# test_cache_flush.sh — E2E: write-behind cache behavior verification
#
# Verifies (br-3h13.18.2):
# - Cache hit after first read (second fetch returns same data)
# - Cache invalidation after write (new message appears in subsequent fetch)
# - Cache scope isolation (project A cache doesn't leak into project B)
# - Rapid consecutive reads return consistent data
#
# Note: The 30s deferred-touch timer is internal and not testable via E2E
# without a configurable interval. We test observable cache behavior instead.

set -euo pipefail

E2E_SUITE="cache_flush"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Cache Behavior E2E Test Suite"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_cache")"
CACHE_DB="${WORK}/cache_test.sqlite3"
PROJECT_PATH_A="/tmp/e2e_cache_a_$$"
PROJECT_PATH_B="/tmp/e2e_cache_b_$$"
PROJECT_SLUG_A="tmp-e2e-cache-a-$$"
PROJECT_SLUG_B="tmp-e2e-cache-b-$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-cache","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"; shift
    local requests=("$@")
    local output_file
    output_file="$(mktemp "${WORK}/session_resp.XXXXXX")"
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
            sleep 0.15
        done
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=30
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
            r = d['result']
            contents = r.get('contents', r.get('content', []))
            if contents:
                print(contents[0].get('text', ''))
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

# ===========================================================================
# Case 1: Setup — 2 projects, agents on each
# ===========================================================================
e2e_case_banner "Setup: 2 projects + agents"

SETUP_RESP="$(send_jsonrpc_session "$CACHE_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_A}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH_B}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"RedFox\",\"project_key\":\"${PROJECT_PATH_A}\",\"program\":\"test\",\"model\":\"test\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"BlueLake\",\"project_key\":\"${PROJECT_PATH_A}\",\"program\":\"test\",\"model\":\"test\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"GoldHawk\",\"project_key\":\"${PROJECT_PATH_B}\",\"program\":\"test\",\"model\":\"test\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

SETUP_OK=true
for rid in 10 11 12 13 14; do
    if [ "$(is_error_result "$SETUP_RESP" $rid)" = "true" ]; then
        SETUP_OK=false
    fi
done

if [ "$SETUP_OK" = "true" ]; then
    e2e_pass "setup: 2 projects + 3 agents created"
else
    e2e_fail "setup: creation failed"
    e2e_summary
    exit 1
fi

# ===========================================================================
# Case 2: Cache hit — read inbox twice in same session, get consistent data
# ===========================================================================
e2e_case_banner "cache_hit_consistent_reads"

# Send a message first, then read inbox twice in same session
HIT_RESP="$(send_jsonrpc_session "$CACHE_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"BlueLake\",\"to\":[\"RedFox\"],\"subject\":\"Cache test msg\",\"body_md\":\"Testing cache behavior\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/RedFox?project=${PROJECT_SLUG_A}\"}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/RedFox?project=${PROJECT_SLUG_A}\"}}" \
)"
e2e_save_artifact "case_02_cache_hit.txt" "$HIT_RESP"

READ1="$(extract_resource_text "$HIT_RESP" 21)"
READ2="$(extract_resource_text "$HIT_RESP" 22)"
e2e_save_artifact "case_02_read1.json" "$READ1"
e2e_save_artifact "case_02_read2.json" "$READ2"

if [ -n "$READ1" ] && [ -n "$READ2" ]; then
    e2e_pass "cache_hit: both reads returned data"
else
    e2e_fail "cache_hit: one or both reads returned empty"
fi

# Check both have same message count
COUNT1="$(echo "$READ1" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d.get('count',len(d.get('messages',[]))))" 2>/dev/null)"
COUNT2="$(echo "$READ2" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d.get('count',len(d.get('messages',[]))))" 2>/dev/null)"
e2e_assert_eq "cache_hit: consistent message count" "$COUNT1" "$COUNT2"

# ===========================================================================
# Case 3: Cache invalidation — new message appears after write
# ===========================================================================
e2e_case_banner "cache_invalidation_after_write"

# In the SAME session: read inbox, send new message, read again
INVAL_RESP="$(send_jsonrpc_session "$CACHE_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/RedFox?project=${PROJECT_SLUG_A}\"}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_A}\",\"sender_name\":\"BlueLake\",\"to\":[\"RedFox\"],\"subject\":\"New after cache\",\"body_md\":\"This should invalidate cache\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/RedFox?project=${PROJECT_SLUG_A}\"}}" \
)"
e2e_save_artifact "case_03_invalidation.txt" "$INVAL_RESP"

BEFORE="$(extract_resource_text "$INVAL_RESP" 30)"
AFTER="$(extract_resource_text "$INVAL_RESP" 32)"
e2e_save_artifact "case_03_before.json" "$BEFORE"
e2e_save_artifact "case_03_after.json" "$AFTER"

BEFORE_COUNT="$(echo "$BEFORE" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d.get('count',len(d.get('messages',[]))))" 2>/dev/null)"
AFTER_COUNT="$(echo "$AFTER" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d.get('count',len(d.get('messages',[]))))" 2>/dev/null)"

if [ "$AFTER_COUNT" -gt "$BEFORE_COUNT" ]; then
    e2e_pass "invalidation: new message visible after write ($BEFORE_COUNT -> $AFTER_COUNT)"
else
    e2e_fail "invalidation: message not visible ($BEFORE_COUNT -> $AFTER_COUNT)"
fi

e2e_assert_contains "invalidation: new subject in response" "$AFTER" "New after cache"

# ===========================================================================
# Case 4: Cache scope isolation — project A and B don't mix
# ===========================================================================
e2e_case_banner "cache_scope_isolation"

# Send message to project B agent, then read both inboxes in same session
ISOL_RESP="$(send_jsonrpc_session "$CACHE_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH_B}\",\"sender_name\":\"GoldHawk\",\"to\":[\"GoldHawk\"],\"subject\":\"Project B only\",\"body_md\":\"Isolation test\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/RedFox?project=${PROJECT_SLUG_A}\"}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/GoldHawk?project=${PROJECT_SLUG_B}\"}}" \
)"
e2e_save_artifact "case_04_isolation.txt" "$ISOL_RESP"

A_INBOX="$(extract_resource_text "$ISOL_RESP" 41)"
B_INBOX="$(extract_resource_text "$ISOL_RESP" 42)"
e2e_save_artifact "case_04_inbox_a.json" "$A_INBOX"
e2e_save_artifact "case_04_inbox_b.json" "$B_INBOX"

e2e_assert_not_contains "isolation: project B msg not in A" "$A_INBOX" "Project B only"
e2e_assert_contains "isolation: project B msg in B" "$B_INBOX" "Project B only"

# ===========================================================================
# Case 5: Rapid consecutive reads return consistent data
# ===========================================================================
e2e_case_banner "rapid_reads_consistency"

# Build 10 rapid inbox reads in a single session
RAPID_REQS=("$INIT_REQ")
for i in $(seq 50 59); do
    RAPID_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":${i},\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/RedFox?project=${PROJECT_SLUG_A}\"}}")
done

RAPID_RESP="$(send_jsonrpc_session "$CACHE_DB" "${RAPID_REQS[@]}")"
e2e_save_artifact "case_05_rapid.txt" "$RAPID_RESP"

# Check all 10 reads returned same count
RAPID_CHECK="$(echo "$RAPID_RESP" | python3 -c "
import sys, json
counts = set()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        rid = d.get('id', 0)
        if 50 <= rid <= 59 and 'result' in d:
            r = d['result']
            contents = r.get('contents', r.get('content', []))
            if contents:
                text = contents[0].get('text', '')
                inner = json.loads(text)
                c = inner.get('count', len(inner.get('messages', [])))
                counts.add(c)
    except:
        pass
if len(counts) == 1:
    print(f'ok:{counts.pop()}')
elif len(counts) == 0:
    print('empty')
else:
    print(f'inconsistent:{counts}')
" 2>/dev/null)"

if [[ "$RAPID_CHECK" == ok:* ]]; then
    e2e_pass "rapid_reads: all 10 returned consistent count (${RAPID_CHECK#ok:})"
elif [ "$RAPID_CHECK" = "empty" ]; then
    e2e_fail "rapid_reads: no responses parsed"
else
    e2e_fail "rapid_reads: $RAPID_CHECK"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
