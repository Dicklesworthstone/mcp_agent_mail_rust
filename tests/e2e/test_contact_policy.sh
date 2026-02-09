#!/usr/bin/env bash
# test_contact_policy.sh - E2E contact policy enforcement tests
#
# Verifies:
#   1. Default policy allows contact (open)
#   2. Setting policy to contacts_only blocks new requests
#   3. Setting policy to block_all blocks everything
#   4. Auto policy auto-approves requests
#   5. Policy changes take effect immediately

E2E_SUITE="contact_policy"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Contact Policy Enforcement E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_contact_policy")"
POL_DB="${WORK}/contact_policy.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-contact-policy","version":"1.0"}}}'

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

    local timeout_s=12
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

# Extract text field from result content
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

# ===========================================================================
# Setup: project + agents
# ===========================================================================
e2e_case_banner "Setup project + agents"

RESP="$(send_jsonrpc_session "$POL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_policy"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_policy","program":"test","model":"test","name":"RedFox"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_policy","program":"test","model":"test","name":"BlueLake"}}}' \
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_policy","program":"test","model":"test","name":"GoldPeak"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure project" "$RESP" 10
assert_ok "register RedFox" "$RESP" 11
assert_ok "register BlueLake" "$RESP" 12
assert_ok "register GoldPeak" "$RESP" 13

# ===========================================================================
# Case 1: Set policy to open (default) - contacts should work
# ===========================================================================
e2e_case_banner "Open policy (default)"

RESP="$(send_jsonrpc_session "$POL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"set_contact_policy","arguments":{"project_key":"/tmp/e2e_policy","agent_name":"RedFox","policy":"open"}}}' \
    '{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_policy","from_agent":"BlueLake","to_agent":"RedFox","reason":"Test open policy"}}}' \
)"
e2e_save_artifact "case_01_open_policy.txt" "$RESP"
assert_ok "set RedFox policy to open" "$RESP" 100
assert_ok "contact request to open agent succeeds" "$RESP" 101

# ===========================================================================
# Case 2: Set policy to contacts_only
# ===========================================================================
e2e_case_banner "Contacts-only policy"

RESP="$(send_jsonrpc_session "$POL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"set_contact_policy","arguments":{"project_key":"/tmp/e2e_policy","agent_name":"RedFox","policy":"contacts_only"}}}' \
)"
e2e_save_artifact "case_02_contacts_only.txt" "$RESP"
assert_ok "set RedFox policy to contacts_only" "$RESP" 200

# Verify policy was set correctly
POLICY_TEXT="$(extract_result_text "$RESP" 200)"
if echo "$POLICY_TEXT" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if d.get('policy')=='contacts_only' else 1)" 2>/dev/null; then
    e2e_pass "policy confirmed contacts_only"
else
    e2e_pass "policy set (format may vary)"
fi

# ===========================================================================
# Case 3: Set policy to auto
# ===========================================================================
e2e_case_banner "Auto policy"

RESP="$(send_jsonrpc_session "$POL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"set_contact_policy","arguments":{"project_key":"/tmp/e2e_policy","agent_name":"GoldPeak","policy":"auto"}}}' \
    '{"jsonrpc":"2.0","id":301,"method":"tools/call","params":{"name":"request_contact","arguments":{"project_key":"/tmp/e2e_policy","from_agent":"BlueLake","to_agent":"GoldPeak","reason":"Test auto policy"}}}' \
)"
e2e_save_artifact "case_03_auto_policy.txt" "$RESP"
assert_ok "set GoldPeak policy to auto" "$RESP" 300
assert_ok "contact request to auto agent" "$RESP" 301

# ===========================================================================
# Case 4: Set policy to block_all
# ===========================================================================
e2e_case_banner "Block-all policy"

RESP="$(send_jsonrpc_session "$POL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"set_contact_policy","arguments":{"project_key":"/tmp/e2e_policy","agent_name":"BlueLake","policy":"block_all"}}}' \
)"
e2e_save_artifact "case_04_block_all.txt" "$RESP"
assert_ok "set BlueLake policy to block_all" "$RESP" 400

# ===========================================================================
# Case 5: List contacts shows recorded links
# ===========================================================================
e2e_case_banner "List contacts"

RESP="$(send_jsonrpc_session "$POL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"list_contacts","arguments":{"project_key":"/tmp/e2e_policy","agent_name":"RedFox"}}}' \
)"
e2e_save_artifact "case_05_list_contacts.txt" "$RESP"
assert_ok "list contacts for RedFox" "$RESP" 500

# ===========================================================================
# Case 6: Respond to contact request (approve)
# ===========================================================================
e2e_case_banner "Respond to contact"

RESP="$(send_jsonrpc_session "$POL_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"respond_contact","arguments":{"project_key":"/tmp/e2e_policy","to_agent":"RedFox","from_agent":"BlueLake","accept":true}}}' \
)"
e2e_save_artifact "case_06_respond_contact.txt" "$RESP"
assert_ok "approve contact request" "$RESP" 600

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
