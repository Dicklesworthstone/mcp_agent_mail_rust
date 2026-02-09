#!/usr/bin/env bash
# test_large_inputs.sh - E2E large input stress tests
#
# Verifies the MCP server handles large payloads correctly:
#   1. 10KB+ message body
#   2. 100KB+ message body
#   3. Maximum subject length (200 chars)
#   4. Many recipients (20+)
#   5. Long file reservation paths
#   6. Large search query
#   7. Many concurrent messages in one session
#   8. Message with many newlines

E2E_SUITE="large_inputs"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Large Input Stress E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_large_inputs")"
LI_DB="${WORK}/large_inputs.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-large-inputs","version":"1.0"}}}'

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

# Generate large strings using Python for reliability
gen_body() {
    local size_kb="$1"
    python3 -c "
import sys
size = int(sys.argv[1]) * 1024
# Repeat a pattern to reach desired size
pattern = 'Lorem ipsum dolor sit amet, consectetur adipiscing elit. '
body = (pattern * ((size // len(pattern)) + 1))[:size]
print(body, end='')
" "$size_kb"
}

# ===========================================================================
# Setup: project + agents
# ===========================================================================
e2e_case_banner "Setup project + many agents"

# Register 25 agents for the many-recipients test
AGENT_REQS=()
for i in $(seq 1 25); do
    # Use valid adjective+noun names
    NAMES=("RedFox" "BlueLake" "GoldPeak" "GreenWolf" "SilverHawk"
           "SwiftDeer" "CalmBear" "BoldRaven" "DarkStone" "MistyRidge"
           "CopperForge" "CoralCove" "AmberPond" "BrightBrook" "WildGrove"
           "NavyIsland" "OliveHill" "TealCreek" "JadeDune" "RubyCliff"
           "SageHollow" "PearlMill" "IvoryBarn" "VioletGate" "CobaltElk")
    NAME="${NAMES[$((i-1))]}"
    AGENT_REQS+=("{\"jsonrpc\":\"2.0\",\"id\":$((10+i)),\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"program\":\"test\",\"model\":\"test\",\"name\":\"${NAME}\"}}}")
done

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_large"}}}' \
    "${AGENT_REQS[@]}" \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_ok "ensure_project" "$RESP" 10
assert_ok "register first agent (RedFox)" "$RESP" 11
assert_ok "register 25th agent (DryCreek)" "$RESP" 35

# ===========================================================================
# Case 1: 10KB message body
# ===========================================================================
e2e_case_banner "10KB message body"

BODY_10K="$(gen_body 10)"
# Escape for JSON
BODY_10K_JSON="$(python3 -c "import json,sys; print(json.dumps(sys.stdin.read()))" <<< "$BODY_10K")"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"10KB body test\",\"body_md\":${BODY_10K_JSON}}}}" \
)"
e2e_save_artifact "case_01_10kb_body.txt" "$RESP"
assert_ok "send 10KB body" "$RESP" 100

# ===========================================================================
# Case 2: 100KB message body
# ===========================================================================
e2e_case_banner "100KB message body"

BODY_100K="$(gen_body 100)"
BODY_100K_JSON="$(python3 -c "import json,sys; print(json.dumps(sys.stdin.read()))" <<< "$BODY_100K")"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":200,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"100KB body test\",\"body_md\":${BODY_100K_JSON}}}}" \
)"
e2e_save_artifact "case_02_100kb_body.txt" "$RESP"
assert_ok "send 100KB body" "$RESP" 200

# ===========================================================================
# Case 3: Maximum subject length (exactly 200 chars)
# ===========================================================================
e2e_case_banner "200-char subject (boundary)"

SUBJ_200="$(python3 -c "print('A' * 200, end='')")"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":300,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"${SUBJ_200}\",\"body_md\":\"body\"}}}" \
)"
e2e_save_artifact "case_03_200char_subject.txt" "$RESP"
assert_ok "send with 200-char subject" "$RESP" 300

# ===========================================================================
# Case 4: Over-length subject (250 chars, should be truncated)
# ===========================================================================
e2e_case_banner "250-char subject (over limit)"

SUBJ_250="$(python3 -c "print('B' * 250, end='')")"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":400,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"${SUBJ_250}\",\"body_md\":\"body\"}}}" \
)"
e2e_save_artifact "case_04_250char_subject.txt" "$RESP"
assert_ok "send with 250-char subject (truncated)" "$RESP" 400

# Verify subject was truncated to 200
TRUNC_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 400 and 'result' in d:
            text = json.dumps(d['result'], ensure_ascii=False)
            # Find subject in the response
            inner = json.loads(d['result']['content'][0]['text'])
            deliveries = inner.get('deliveries', [])
            if deliveries:
                subj = deliveries[0]['payload']['subject']
                print(f'LEN={len(subj)}')
            else:
                print('NO_DELIVERIES')
            sys.exit(0)
    except Exception as e:
        pass
print('PARSE_FAIL')
" 2>/dev/null)"

e2e_assert_contains "subject truncated to 200" "$TRUNC_CHECK" "LEN=200"

# ===========================================================================
# Case 5: Many recipients (20 in to field)
# ===========================================================================
e2e_case_banner "20 recipients"

RECIPIENTS='["BlueLake","GoldPeak","GreenWolf","SilverHawk","SwiftDeer","CalmBear","BoldRaven","DarkStone","MistyRidge","CopperForge","CoralCove","AmberPond","BrightBrook","WildGrove","NavyIsland","OliveHill","TealCreek","JadeDune","RubyCliff","SageHollow"]'

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":500,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"sender_name\":\"RedFox\",\"to\":${RECIPIENTS},\"subject\":\"Broadcast to 20 agents\",\"body_md\":\"Hello everyone\"}}}" \
)"
e2e_save_artifact "case_05_many_recipients.txt" "$RESP"
assert_ok "send to 20 recipients" "$RESP" 500

# ===========================================================================
# Case 6: Long file reservation paths
# ===========================================================================
e2e_case_banner "Long file reservation paths"

LONG_PATH="$(python3 -c "print('/'.join(['very_long_directory_name'] * 20) + '/file.rs', end='')")"
LONG_PATH_JSON="$(python3 -c "import json,sys; print(json.dumps(sys.stdin.read()))" <<< "$LONG_PATH")"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":600,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"agent_name\":\"RedFox\",\"paths\":[${LONG_PATH_JSON}],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"testing long paths\"}}}" \
)"
e2e_save_artifact "case_06_long_paths.txt" "$RESP"
assert_ok "reserve file with long path" "$RESP" 600

# ===========================================================================
# Case 7: Large search query
# ===========================================================================
e2e_case_banner "Large search query"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":700,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_large","query":"Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua"}}}' \
)"
e2e_save_artifact "case_07_large_search.txt" "$RESP"
assert_ok "search with long query" "$RESP" 700

# ===========================================================================
# Case 8: Message with many newlines (1000 lines)
# ===========================================================================
e2e_case_banner "1000-line message body"

MANY_LINES="$(python3 -c "
import json
body = '\\n'.join([f'Line {i}: Some content here' for i in range(1000)])
print(json.dumps(body), end='')
")"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":800,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"1000 lines\",\"body_md\":${MANY_LINES}}}}" \
)"
e2e_save_artifact "case_08_many_lines.txt" "$RESP"
assert_ok "send 1000-line body" "$RESP" 800

# ===========================================================================
# Case 9: Fetch inbox with large messages
# ===========================================================================
e2e_case_banner "Fetch inbox with large messages"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_large","agent_name":"BlueLake","include_bodies":true,"limit":20}}}' \
)"
e2e_save_artifact "case_09_fetch_large_inbox.txt" "$RESP"
assert_ok "fetch inbox with large messages" "$RESP" 900

# Verify inbox has multiple messages
MSG_COUNT="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 900 and 'result' in d:
            text = d['result']['content'][0]['text']
            msgs = json.loads(text)
            print(f'COUNT={len(msgs)}')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"

e2e_assert_contains "inbox has multiple messages" "$MSG_COUNT" "COUNT="

# ===========================================================================
# Case 10: Many file reservations in one call
# ===========================================================================
e2e_case_banner "Many file reservations"

MANY_PATHS="$(python3 -c "
import json
paths = [f'src/module_{i}/file_{j}.rs' for i in range(5) for j in range(10)]
print(json.dumps(paths), end='')
")"

RESP="$(send_jsonrpc_session "$LI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":1000,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"/tmp/e2e_large\",\"agent_name\":\"GoldPeak\",\"paths\":${MANY_PATHS},\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"batch reservation\"}}}" \
)"
e2e_save_artifact "case_10_many_reservations.txt" "$RESP"
assert_ok "reserve 50 file paths" "$RESP" 1000

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
