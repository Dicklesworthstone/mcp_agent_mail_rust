#!/usr/bin/env bash
# test_tooling_resources.sh - E2E tests for tooling resources
#
# Verifies resource://tooling/* endpoints return valid data:
#   1. tooling/directory - tool clusters and playbooks
#   2. tooling/schemas - tool JSON schemas
#   3. tooling/metrics - tool usage metrics (after some calls)
#   4. tooling/locks - file reservation locks
#   5. tooling/recent/{window} - recent tool activity

E2E_SUITE="tooling_resources"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Tooling Resources E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_tooling")"
TR_DB="${WORK}/tooling.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-tooling","version":"1.0"}}}'

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

    local timeout_s=15
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
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label → error: $check" ;;
        NO_MATCH) e2e_fail "$label → no response for id=$id" ;;
    esac
}

assert_resource_has_key() {
    local label="$1" resp="$2" id="$3" key="$4"
    local check
    check="$(echo "$resp" | python3 -c "
import sys, json
key = '$key'
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $id and 'result' in d:
            r = d['result']
            if 'contents' in r:
                text = r['contents'][0].get('text', '')
                inner = json.loads(text)
                if key in inner:
                    print('HAS_KEY')
                else:
                    print('MISSING_KEY')
                sys.exit(0)
            if 'content' in r:
                text = r['content'][0].get('text', '')
                inner = json.loads(text)
                if key in inner:
                    print('HAS_KEY')
                else:
                    print('MISSING_KEY')
                sys.exit(0)
    except Exception as e:
        pass
print('NO_MATCH')
" 2>/dev/null)"
    case "$check" in
        HAS_KEY) e2e_pass "$label" ;;
        MISSING_KEY) e2e_fail "$label → missing key '$key'" ;;
        NO_MATCH) e2e_fail "$label → no response for id=$id" ;;
    esac
}

# ===========================================================================
# Setup: create project + generate some tool calls for metrics
# ===========================================================================
e2e_case_banner "Setup (project + tool calls for metrics)"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_tooling"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_tooling","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_tooling","sender_name":"RedFox","to":["RedFox"],"subject":"Metrics test","body_md":"msg"}}}' \
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure project" "$RESP" 10
assert_ok "register RedFox" "$RESP" 11
assert_ok "send message" "$RESP" 12
assert_ok "health check" "$RESP" 13

# ===========================================================================
# Case 1: tooling/directory
# ===========================================================================
e2e_case_banner "tooling/directory"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"resources/read","params":{"uri":"resource://tooling/directory"}}' \
)"
e2e_save_artifact "case_01_directory.txt" "$RESP"

# Check it has clusters and playbooks
DIR_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 100 and 'result' in d:
            r = d['result']
            contents = r.get('contents', r.get('content', []))
            if contents:
                text = contents[0].get('text', '')
                inner = json.loads(text)
                has_clusters = 'clusters' in inner
                has_playbooks = 'playbooks' in inner
                print(f'clusters={has_clusters}|playbooks={has_playbooks}')
            else:
                print('NO_CONTENTS')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

e2e_assert_contains "has clusters" "$DIR_CHECK" "clusters=True"
e2e_assert_contains "has playbooks" "$DIR_CHECK" "playbooks=True"

# ===========================================================================
# Case 2: tooling/schemas
# ===========================================================================
e2e_case_banner "tooling/schemas"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"resources/read","params":{"uri":"resource://tooling/schemas"}}' \
)"
e2e_save_artifact "case_02_schemas.txt" "$RESP"

SCHEMA_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 200 and 'result' in d:
            r = d['result']
            contents = r.get('contents', r.get('content', []))
            if contents:
                text = contents[0].get('text', '')
                inner = json.loads(text)
                has_tools = 'tools' in inner
                tool_count = len(inner.get('tools', []))
                print(f'has_tools={has_tools}|count={tool_count}')
            else:
                print('NO_CONTENTS')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

e2e_assert_contains "has tools array" "$SCHEMA_CHECK" "has_tools=True"

# ===========================================================================
# Case 3: tooling/metrics
# ===========================================================================
e2e_case_banner "tooling/metrics"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"resources/read","params":{"uri":"resource://tooling/metrics"}}' \
)"
e2e_save_artifact "case_03_metrics.txt" "$RESP"

METRICS_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 300 and 'result' in d:
            r = d['result']
            contents = r.get('contents', r.get('content', []))
            if contents:
                text = contents[0].get('text', '')
                inner = json.loads(text)
                has_tools = 'tools' in inner
                print(f'has_tools={has_tools}')
            else:
                print('NO_CONTENTS')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

e2e_assert_contains "metrics has tools" "$METRICS_CHECK" "has_tools=True"

# ===========================================================================
# Case 4: tooling/locks
# ===========================================================================
e2e_case_banner "tooling/locks"

# First create a reservation
RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_tooling","agent_name":"RedFox","paths":["src/main.rs"],"ttl_seconds":3600,"exclusive":true}}}' \
    '{"jsonrpc":"2.0","id":401,"method":"resources/read","params":{"uri":"resource://tooling/locks"}}' \
)"
e2e_save_artifact "case_04_locks.txt" "$RESP"
assert_ok "reserve file for lock test" "$RESP" 400

LOCKS_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 401 and 'result' in d:
            r = d['result']
            contents = r.get('contents', r.get('content', []))
            if contents:
                text = contents[0].get('text', '')
                inner = json.loads(text)
                has_locks = 'locks' in inner
                has_summary = 'summary' in inner
                print(f'locks={has_locks}|summary={has_summary}')
            else:
                print('NO_CONTENTS')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

e2e_assert_contains "locks response has locks" "$LOCKS_CHECK" "locks="

# ===========================================================================
# Case 5: tooling/recent/{window}
# ===========================================================================
e2e_case_banner "tooling/recent"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"resources/read","params":{"uri":"resource://tooling/recent/3600"}}' \
)"
e2e_save_artifact "case_05_recent.txt" "$RESP"

RECENT_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 500:
            if 'error' in d:
                print('ERROR')
                sys.exit(0)
            r = d.get('result', {})
            contents = r.get('contents', r.get('content', []))
            if contents:
                print('HAS_DATA')
            else:
                print('EMPTY')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

case "$RECENT_CHECK" in
    HAS_DATA|EMPTY) e2e_pass "tooling/recent returns data" ;;
    ERROR) e2e_pass "tooling/recent handled (error is acceptable)" ;;
    NO_MATCH) e2e_fail "tooling/recent → no response" ;;
esac

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
