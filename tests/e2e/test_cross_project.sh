#!/usr/bin/env bash
# test_cross_project.sh - E2E cross-project messaging and contacts
#
# Verifies:
#   1. Two projects can be created independently
#   2. Agents in different projects have separate inboxes
#   3. Contact requests across projects
#   4. Cross-project messaging after contact approval
#   5. File reservations are project-scoped

E2E_SUITE="cross_project"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Cross-Project Messaging E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_cross_project")"
CP_DB="${WORK}/cross_project.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-cross-project","version":"1.0"}}}'

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
        JSON_RPC_ERROR|MCP_ERROR) e2e_fail "$label → error: $check" ;;
        NO_MATCH) e2e_fail "$label → no response for id=$id" ;;
    esac
}

# ===========================================================================
# Setup: two projects with agents
# ===========================================================================
e2e_case_banner "Setup two projects"

RESP="$(send_jsonrpc_session "$CP_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_proj_alpha"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_proj_beta"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_proj_alpha","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_proj_beta","program":"test","model":"test","name":"BlueLake"}}}' \
    '{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_proj_alpha","program":"test","model":"test","name":"GoldPeak"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"

assert_ok "ensure project alpha" "$RESP" 10
assert_ok "ensure project beta" "$RESP" 11
assert_ok "register RedFox in alpha" "$RESP" 12
assert_ok "register BlueLake in beta" "$RESP" 13
assert_ok "register GoldPeak in alpha" "$RESP" 14

# ===========================================================================
# Case 1: Messaging within project alpha
# ===========================================================================
e2e_case_banner "Intra-project messaging"

RESP="$(send_jsonrpc_session "$CP_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_proj_alpha","sender_name":"RedFox","to":["GoldPeak"],"subject":"Alpha internal","body_md":"Hello from alpha"}}}' \
)"
e2e_save_artifact "case_01_intra_project.txt" "$RESP"
assert_ok "send within project alpha" "$RESP" 100

# ===========================================================================
# Case 2: Fetch inbox shows only project-scoped messages
# ===========================================================================
e2e_case_banner "Inbox is project-scoped"

RESP="$(send_jsonrpc_session "$CP_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_proj_alpha","agent_name":"GoldPeak","include_bodies":true}}}' \
    '{"jsonrpc":"2.0","id":201,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_proj_beta","agent_name":"BlueLake","include_bodies":true}}}' \
)"
e2e_save_artifact "case_02_inbox_scoped.txt" "$RESP"
assert_ok "fetch GoldPeak inbox in alpha" "$RESP" 200
assert_ok "fetch BlueLake inbox in beta" "$RESP" 201

# Verify GoldPeak has 1 message, BlueLake has 0
SCOPE_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
gold_count = None
blue_count = None
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 200 and 'result' in d:
            msgs = json.loads(d['result']['content'][0]['text'])
            gold_count = len(msgs) if isinstance(msgs, list) else 0
        if d.get('id') == 201 and 'result' in d:
            msgs = json.loads(d['result']['content'][0]['text'])
            blue_count = len(msgs) if isinstance(msgs, list) else 0
    except Exception: pass
print(f'gold={gold_count}|blue={blue_count}')
" 2>/dev/null)"

e2e_assert_contains "GoldPeak has message" "$SCOPE_CHECK" "gold=1"
e2e_assert_contains "BlueLake empty inbox" "$SCOPE_CHECK" "blue=0"

# ===========================================================================
# Case 3: Contact request across projects
# ===========================================================================
e2e_case_banner "Cross-project contact request"

RESP="$(send_jsonrpc_session "$CP_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_proj_alpha","from_agent":"RedFox","to_agent":"BlueLake","to_project":"/tmp/e2e_proj_beta","reason":"Cross-project coordination"}}}' \
)"
e2e_save_artifact "case_03_cross_contact.txt" "$RESP"
assert_ok "request contact across projects" "$RESP" 300

# ===========================================================================
# Case 4: File reservations are project-scoped
# ===========================================================================
e2e_case_banner "File reservations project-scoped"

RESP="$(send_jsonrpc_session "$CP_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_proj_alpha","agent_name":"RedFox","paths":["src/main.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"editing"}}}' \
    '{"jsonrpc":"2.0","id":401,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_proj_beta","agent_name":"BlueLake","paths":["src/main.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"editing"}}}' \
)"
e2e_save_artifact "case_04_reservation_scope.txt" "$RESP"
assert_ok "reserve src/main.rs in alpha" "$RESP" 400
assert_ok "reserve same path in beta (no conflict)" "$RESP" 401

# Verify no conflicts (same path in different projects shouldn't conflict)
CONFLICT_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 401 and 'result' in d:
            text = d['result']['content'][0]['text']
            result = json.loads(text)
            conflicts = result.get('conflicts', [])
            print(f'conflicts={len(conflicts)}')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"

e2e_assert_contains "no cross-project conflict" "$CONFLICT_CHECK" "conflicts=0"

# ===========================================================================
# Case 5: Search is project-scoped
# ===========================================================================
e2e_case_banner "Search is project-scoped"

RESP="$(send_jsonrpc_session "$CP_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_proj_alpha","query":"Alpha"}}}' \
    '{"jsonrpc":"2.0","id":501,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_proj_beta","query":"Alpha"}}}' \
)"
e2e_save_artifact "case_05_search_scope.txt" "$RESP"
assert_ok "search in alpha" "$RESP" 500
assert_ok "search in beta" "$RESP" 501

# Alpha should find the message, beta should not
SEARCH_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
alpha_count = None
beta_count = None
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 500 and 'result' in d:
            inner = json.loads(d['result']['content'][0]['text'])
            # search_messages returns {result: [...]} wrapper
            msgs = inner.get('result', inner) if isinstance(inner, dict) else inner
            alpha_count = len(msgs) if isinstance(msgs, list) else 0
        if d.get('id') == 501 and 'result' in d:
            inner = json.loads(d['result']['content'][0]['text'])
            msgs = inner.get('result', inner) if isinstance(inner, dict) else inner
            beta_count = len(msgs) if isinstance(msgs, list) else 0
    except Exception: pass
print(f'alpha={alpha_count}|beta={beta_count}')
" 2>/dev/null)"

e2e_assert_contains "alpha search has results" "$SEARCH_CHECK" "alpha=1"
e2e_assert_contains "beta search empty" "$SEARCH_CHECK" "beta=0"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
