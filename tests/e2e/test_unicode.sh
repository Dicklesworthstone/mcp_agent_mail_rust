#!/usr/bin/env bash
# test_unicode.sh - E2E unicode/emoji stress tests
#
# Verifies the MCP server handles Unicode and emoji correctly in all fields:
# subjects, body, agent names, project keys, search queries, file paths.

E2E_SUITE="unicode"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Unicode/Emoji Stress E2E Suite"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_unicode")"
UNI_DB="${WORK}/unicode_test.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-unicode","version":"1.0"}}}'

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

# Helper: check tool call succeeded (id match, no isError)
assert_tool_ok() {
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

assert_tool_error() {
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
            if 'error' in d or d.get('result',{}).get('isError', False):
                print('ERROR')
                sys.exit(0)
            print('RESULT')
            sys.exit(0)
    except Exception: pass
print('NO_MATCH')
" 2>/dev/null)"

    case "$check" in
        ERROR) e2e_pass "$label → error (expected)" ;;
        RESULT) e2e_pass "$label → result (acceptable)" ;;
        NO_MATCH) e2e_fail "$label → no response" ;;
    esac
}

# ===========================================================================
# Setup: project + agents
# ===========================================================================
e2e_case_banner "Setup with Unicode project path"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_日本語_プロジェクト"}}}' \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","program":"test","model":"test","name":"GoldFox"}}}' \
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","program":"test","model":"test","name":"SilverWolf"}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"

assert_tool_ok "ensure_project with Japanese path" "$RESP" 10
assert_tool_ok "register GoldFox agent" "$RESP" 11
assert_tool_ok "register SilverWolf agent" "$RESP" 12

# ===========================================================================
# Case 1: Send message with emoji subject + body
# ===========================================================================
e2e_case_banner "Message with emoji subject and body"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","sender_name":"GoldFox","to":["SilverWolf"],"subject":"🚀 Deploy report 🎉✅","body_md":"## Status 🟢\n\n- ✅ Tests pass\n- 🔥 Hot path optimized\n- 📊 Dashboard updated\n\n> 💡 **Note**: CJK text: 这是中文, これは日本語, 이것은 한국어"}}}' \
)"
e2e_save_artifact "case_01_emoji_message.txt" "$RESP"
assert_tool_ok "send message with emoji subject + CJK body" "$RESP" 100

# ===========================================================================
# Case 2: Fetch inbox returns Unicode content intact
# ===========================================================================
e2e_case_banner "Fetch inbox preserves Unicode"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","agent_name":"SilverWolf","include_bodies":true}}}' \
)"
e2e_save_artifact "case_02_fetch_inbox.txt" "$RESP"
assert_tool_ok "fetch_inbox for SilverWolf" "$RESP" 200

# Check emoji preserved in response
EMOJI_CHECK="$(echo "$RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 200 and 'result' in d:
            text = json.dumps(d['result'], ensure_ascii=False)
            has_emoji = '\U0001f680' in text
            has_cjk = '中文' in text or '日本語' in text
            print(f'emoji={has_emoji}|cjk={has_cjk}')
            sys.exit(0)
    except Exception: pass
print('PARSE_FAIL')
" 2>/dev/null)"

e2e_assert_contains "emoji preserved in inbox" "$EMOJI_CHECK" "emoji=True"

# ===========================================================================
# Case 3: Search with Unicode query
# ===========================================================================
e2e_case_banner "Search with Unicode query"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"search_messages","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","query":"Deploy 🚀"}}}' \
)"
e2e_save_artifact "case_03_unicode_search.txt" "$RESP"
assert_tool_ok "search with emoji query" "$RESP" 300

# ===========================================================================
# Case 4: File reservation with Unicode path
# ===========================================================================
e2e_case_banner "File reservation with Unicode path"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","agent_name":"GoldFox","paths":["src/数据/模型.rs","docs/ドキュメント.md"],"ttl_seconds":3600,"exclusive":true,"reason":"编辑中文文件"}}}' \
)"
e2e_save_artifact "case_04_unicode_reservation.txt" "$RESP"
assert_tool_ok "reserve files with CJK paths" "$RESP" 400

# ===========================================================================
# Case 5: Reply with RTL text (Arabic/Hebrew)
# ===========================================================================
e2e_case_banner "Reply with RTL text"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":500,"method":"tools/call","params":{"name":"reply_message","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","message_id":1,"sender_name":"SilverWolf","body_md":"شكراً لك! هذا رائع\n\nתודה רבה! זה נהדר"}}}' \
)"
e2e_save_artifact "case_05_rtl_reply.txt" "$RESP"
assert_tool_ok "reply with Arabic + Hebrew text" "$RESP" 500

# ===========================================================================
# Case 6: Message with zero-width characters and combining marks
# ===========================================================================
e2e_case_banner "Zero-width and combining characters"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","sender_name":"GoldFox","to":["SilverWolf"],"subject":"ZWJ test: 👨‍👩‍👧‍👦 combined","body_md":"Test: e\u0301 (e + combining acute) and \u200b (zero-width space)"}}}' \
)"
e2e_save_artifact "case_06_combining.txt" "$RESP"
assert_tool_ok "message with combining marks and ZWJ" "$RESP" 600

# ===========================================================================
# Case 7: Long Unicode subject (boundary truncation test)
# ===========================================================================
e2e_case_banner "Long Unicode subject (200+ chars)"

# Build a 210-char CJK subject
LONG_SUBJECT="日本語テスト"
while [ ${#LONG_SUBJECT} -lt 210 ]; do
    LONG_SUBJECT="${LONG_SUBJECT}日本語テスト"
done

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":700,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"/tmp/e2e_日本語_プロジェクト\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"${LONG_SUBJECT}\",\"body_md\":\"body\"}}}" \
)"
e2e_save_artifact "case_07_long_subject.txt" "$RESP"
assert_tool_ok "send message with 210+ char CJK subject" "$RESP" 700

# ===========================================================================
# Case 8: Mathematical symbols and special Unicode blocks
# ===========================================================================
e2e_case_banner "Mathematical/special Unicode"

RESP="$(send_jsonrpc_session "$UNI_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":800,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_日本語_プロジェクト","sender_name":"GoldFox","to":["SilverWolf"],"subject":"Math: ∀x∈ℝ, ∃ε>0","body_md":"## Proof\n\n∫₀^∞ e^{-x²} dx = √π/2\n\n𝕳𝖊𝖑𝖑𝖔 𝖂𝖔𝖗𝖑𝖉 (Fraktur)\n\n♠♥♦♣ ★☆ ⚡⚠"}}}' \
)"
e2e_save_artifact "case_08_math_symbols.txt" "$RESP"
assert_tool_ok "message with math/special symbols" "$RESP" 800

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
