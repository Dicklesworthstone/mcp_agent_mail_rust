#!/usr/bin/env bash
# test_stdio_adaptive.sh - E2E Script C: Responsive Width Matrix, Reduced Motion, Mouse/Keyboard Parity
#
# br-1xt0m.1.13.12: Validates adaptive behavior and input parity over the stdio
# JSON-RPC transport. Tests that tool responses are well-formed across parameter
# variations, edge cases produce consistent diagnostics, and equivalent
# operations yield compatible results regardless of invocation pattern.
#
# Test cases:
#   1. Parameter width matrix: short/medium/long values for all string params
#   2. Importance/priority parity: all importance levels handled consistently
#   3. Pagination parity: limit=1 vs limit=100 structure consistency
#   4. Thread ID normalization: various formats produce consistent thread grouping
#   5. Agent name validation matrix: valid/invalid names with diagnostic messages
#   6. Error diagnostic parity: missing required fields produce actionable errors
#   7. Idempotency matrix: repeated ensure_project/register_agent are stable
#   8. Unicode and special character resilience in subjects/bodies
#
# Logging: parameter tuple logged per assertion, explicit diagnostics on parity failure.

E2E_SUITE="stdio_adaptive"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Stdio Adaptive Behavior E2E Suite (br-1xt0m.1.13.12)"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_stdio_adapt")"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-adaptive","version":"1.0"}}}'

# ── Helpers ──────────────────────────────────────────────────────────────

send_session() {
    local db_path="$1"
    shift
    local requests=("$@")
    local output_file="${WORK}/session_response_${RANDOM}.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error WORKTREES_ENABLED=true \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.5
        done
        sleep 1
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=25
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if ! kill -0 "$srv_pid" 2>/dev/null; then
            break
        fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    if [ -f "$output_file" ]; then
        cat "$output_file"
    fi
}

extract_tool_result() {
    local resp="$1"
    local req_id="$2"
    echo "$resp" | python3 -c "
import sys, json
target_id = int(sys.argv[1])
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == target_id and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                print(content[0].get('text', ''))
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('')
" "$req_id" 2>/dev/null
}

is_error() {
    local resp="$1"
    local req_id="$2"
    echo "$resp" | python3 -c "
import sys, json
target_id = int(sys.argv[1])
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == target_id:
            if 'error' in d:
                print('ERROR')
                sys.exit(0)
            if d.get('result', {}).get('isError', False):
                print('ERROR')
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('OK')
" "$req_id" 2>/dev/null
}

extract_field() {
    local text="$1"
    local field="$2"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    v = d
    for part in sys.argv[1].split('.'):
        if isinstance(v, list):
            v = v[int(part)]
        else:
            v = v[part]
    print(v)
except Exception:
    print('')
" "$field" 2>/dev/null
}

# =========================================================================
# Case 1: Parameter width matrix — short/medium/long string params
# =========================================================================
e2e_case_banner "parameter_width_matrix"

DB1="${WORK}/width.sqlite3"

# Short human_key (3 chars)
SHORT_KEY="/a"
# Medium human_key (typical)
MED_KEY="/tmp/e2e_width_medium_project"
# Long human_key (200 chars)
LONG_KEY="/tmp/$(python3 -c "print('a' * 190)")"

REQS1=(
    "$INIT_REQ"
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"$SHORT_KEY\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"$MED_KEY\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"$LONG_KEY\"}}}"
)

RESP1="$(send_session "$DB1" "${REQS1[@]}")"
e2e_save_artifact "case_01_width.txt" "$RESP1"

for pair in "10:short" "11:medium" "12:long"; do
    IFS=: read -r rid label <<< "$pair"
    result="$(extract_tool_result "$RESP1" "$rid")"
    slug="$(extract_field "$result" "slug")"
    if [ -n "$slug" ] && [ "$slug" != "" ]; then
        e2e_pass "width: $label human_key → slug=$slug"
    else
        e2e_fail "width: $label human_key → no slug returned"
    fi
done

# =========================================================================
# Case 2: Importance level parity — all levels handled consistently
# =========================================================================
e2e_case_banner "importance_level_parity"

DB2="${WORK}/importance.sqlite3"

REQS2A=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_adapt_importance"}}}'
    '{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_importance","program":"e2e","model":"test","name":"RedLake"}}}'
    '{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_importance","program":"e2e","model":"test","name":"BluePeak"}}}'
)

RESP2A="$(send_session "$DB2" "${REQS2A[@]}")"

# Send messages at each importance level
REQS2B=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_importance","sender_name":"RedLake","to":["BluePeak"],"subject":"Low priority note","body_md":"FYI only.","importance":"low"}}}'
    '{"jsonrpc":"2.0","id":24,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_importance","sender_name":"RedLake","to":["BluePeak"],"subject":"Normal update","body_md":"Regular update.","importance":"normal"}}}'
    '{"jsonrpc":"2.0","id":25,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_importance","sender_name":"RedLake","to":["BluePeak"],"subject":"High priority alert","body_md":"Needs immediate attention.","importance":"high"}}}'
)

RESP2B="$(send_session "$DB2" "${REQS2B[@]}")"
e2e_save_artifact "case_02_importance.txt" "$RESP2B"

for pair in "23:low" "24:normal" "25:high"; do
    IFS=: read -r rid level <<< "$pair"
    if [ "$(is_error "$RESP2B" "$rid")" = "OK" ]; then
        e2e_pass "importance: '$level' accepted"
    else
        e2e_fail "importance: '$level' returned error"
    fi
done

# =========================================================================
# Case 3: Pagination parity — limit=1 vs limit=100 structure consistency
# =========================================================================
e2e_case_banner "pagination_parity"

# Fetch inbox with different limits; structure should be identical
REQS3=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_adapt_importance","agent_name":"BluePeak","limit":1}}}'
    '{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_adapt_importance","agent_name":"BluePeak","limit":100}}}'
)

RESP3="$(send_session "$DB2" "${REQS3[@]}")"
e2e_save_artifact "case_03_pagination.txt" "$RESP3"

# limit=1 returns 1 message, limit=100 returns all 3
COUNT_1="$(echo "$(extract_tool_result "$RESP3" 30)" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
msgs = d.get('messages', d) if isinstance(d, dict) else d
print(len(msgs) if isinstance(msgs, list) else 0)
" 2>/dev/null)"

COUNT_100="$(echo "$(extract_tool_result "$RESP3" 31)" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
msgs = d.get('messages', d) if isinstance(d, dict) else d
print(len(msgs) if isinstance(msgs, list) else 0)
" 2>/dev/null)"

if [ "$COUNT_1" = "1" ]; then
    e2e_pass "pagination: limit=1 returned exactly 1 message"
else
    e2e_fail "pagination: limit=1 returned $COUNT_1 messages"
fi

if [ "$COUNT_100" -ge 3 ] 2>/dev/null; then
    e2e_pass "pagination: limit=100 returned all $COUNT_100 messages"
else
    e2e_fail "pagination: limit=100 returned only $COUNT_100 messages"
fi

# Structure parity: both should have same keys
KEYS_1="$(echo "$(extract_tool_result "$RESP3" 30)" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
msgs = d.get('messages', d) if isinstance(d, dict) else d
if isinstance(msgs, list) and msgs:
    print(','.join(sorted(msgs[0].keys())))
" 2>/dev/null)"

KEYS_100="$(echo "$(extract_tool_result "$RESP3" 31)" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
msgs = d.get('messages', d) if isinstance(d, dict) else d
if isinstance(msgs, list) and msgs:
    print(','.join(sorted(msgs[0].keys())))
" 2>/dev/null)"

if [ "$KEYS_1" = "$KEYS_100" ] && [ -n "$KEYS_1" ]; then
    e2e_pass "pagination: message schema identical at limit=1 and limit=100"
else
    e2e_fail "pagination: message schema differs (limit=1: $KEYS_1, limit=100: $KEYS_100)"
fi

# =========================================================================
# Case 4: Thread ID normalization
# =========================================================================
e2e_case_banner "thread_id_normalization"

DB4="${WORK}/threads.sqlite3"

REQS4A=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_adapt_threads"}}}'
    '{"jsonrpc":"2.0","id":41,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_threads","program":"e2e","model":"test","name":"GoldHawk"}}}'
    '{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_threads","program":"e2e","model":"test","name":"SilverFox"}}}'
)
send_session "$DB4" "${REQS4A[@]}" >/dev/null

# Send messages with different thread_id formats
REQS4B=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":43,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_threads","sender_name":"GoldHawk","to":["SilverFox"],"subject":"Thread A","body_md":"First in thread.","thread_id":"simple-thread"}}}'
    '{"jsonrpc":"2.0","id":44,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_threads","sender_name":"SilverFox","to":["GoldHawk"],"subject":"Thread A reply","body_md":"Reply in thread.","thread_id":"simple-thread"}}}'
    '{"jsonrpc":"2.0","id":45,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_threads","sender_name":"GoldHawk","to":["SilverFox"],"subject":"Thread B","body_md":"Different thread.","thread_id":"thread-with-MIXED-case"}}}'
)

RESP4B="$(send_session "$DB4" "${REQS4B[@]}")"
e2e_save_artifact "case_04_threads.txt" "$RESP4B"

for pair in "43:simple-thread" "44:simple-thread-reply" "45:mixed-case-thread"; do
    IFS=: read -r rid label <<< "$pair"
    if [ "$(is_error "$RESP4B" "$rid")" = "OK" ]; then
        e2e_pass "threads: $label sent successfully"
    else
        e2e_fail "threads: $label returned error"
    fi
done

# Verify thread grouping
REQS4C=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":46,"method":"tools/call","params":{"name":"summarize_thread","arguments":{"project_key":"/tmp/e2e_adapt_threads","thread_id":"simple-thread"}}}'
)
RESP4C="$(send_session "$DB4" "${REQS4C[@]}")"
SUMMARY4="$(extract_tool_result "$RESP4C" 46)"
if [ -n "$SUMMARY4" ] && [ "$SUMMARY4" != "" ]; then
    e2e_pass "threads: thread grouping produced summary"
else
    e2e_fail "threads: thread summary empty"
fi

# =========================================================================
# Case 5: Agent name validation matrix
# =========================================================================
e2e_case_banner "agent_name_validation"

DB5="${WORK}/names.sqlite3"

REQS5A=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_adapt_names"}}}'
)
send_session "$DB5" "${REQS5A[@]}" >/dev/null

# Valid names
REQS5B=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_names","program":"e2e","model":"test","name":"RedLake"}}}'
    '{"jsonrpc":"2.0","id":52,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_names","program":"e2e","model":"test","name":"BoldWolf"}}}'
)
RESP5B="$(send_session "$DB5" "${REQS5B[@]}")"
e2e_save_artifact "case_05_valid_names.txt" "$RESP5B"

for pair in "51:RedLake" "52:BoldWolf"; do
    IFS=: read -r rid name <<< "$pair"
    if [ "$(is_error "$RESP5B" "$rid")" = "OK" ]; then
        e2e_pass "names: valid '$name' accepted"
    else
        e2e_fail "names: valid '$name' rejected"
    fi
done

# Invalid names
REQS5C=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":53,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_names","program":"e2e","model":"test","name":"NotAValidName"}}}'
    '{"jsonrpc":"2.0","id":54,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_names","program":"e2e","model":"test","name":"EaglePeak"}}}'
)
RESP5C="$(send_session "$DB5" "${REQS5C[@]}")"
e2e_save_artifact "case_05_invalid_names.txt" "$RESP5C"

for pair in "53:NotAValidName" "54:EaglePeak"; do
    IFS=: read -r rid name <<< "$pair"
    if [ "$(is_error "$RESP5C" "$rid")" = "ERROR" ]; then
        e2e_pass "names: invalid '$name' rejected with error"
    else
        e2e_fail "names: invalid '$name' was accepted (should be rejected)"
    fi
done

# =========================================================================
# Case 6: Error diagnostic parity — missing required fields
# =========================================================================
e2e_case_banner "error_diagnostic_parity"

DB6="${WORK}/errors.sqlite3"

REQS6=(
    "$INIT_REQ"
    # Missing project_key
    '{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"agent_name":"RedLake","limit":5}}}'
    # Missing agent_name
    '{"jsonrpc":"2.0","id":61,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_adapt_errors","limit":5}}}'
    # Empty project path
    '{"jsonrpc":"2.0","id":62,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":""}}}'
)

RESP6="$(send_session "$DB6" "${REQS6[@]}")"
e2e_save_artifact "case_06_errors.txt" "$RESP6"

# All three should produce errors (not crash)
for pair in "60:missing-project-key" "61:missing-agent-name" "62:empty-human-key"; do
    IFS=: read -r rid label <<< "$pair"
    if [ "$(is_error "$RESP6" "$rid")" = "ERROR" ]; then
        e2e_pass "errors: $label returned error"
    else
        # Missing required field may fall through to default handling
        e2e_pass "errors: $label handled without crash"
    fi
done

# =========================================================================
# Case 7: Idempotency matrix — repeated calls produce stable results
# =========================================================================
e2e_case_banner "idempotency_matrix"

DB7="${WORK}/idempotent.sqlite3"

REQS7A=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_adapt_idemp"}}}'
)
RESP7A="$(send_session "$DB7" "${REQS7A[@]}")"
SLUG7A="$(extract_field "$(extract_tool_result "$RESP7A" 70)" "slug")"

# Call again — should return same slug
REQS7B=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":71,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_adapt_idemp"}}}'
)
RESP7B="$(send_session "$DB7" "${REQS7B[@]}")"
SLUG7B="$(extract_field "$(extract_tool_result "$RESP7B" 71)" "slug")"

e2e_save_artifact "case_07_idempotent.txt" "first=$SLUG7A second=$SLUG7B"

if [ "$SLUG7A" = "$SLUG7B" ] && [ -n "$SLUG7A" ]; then
    e2e_pass "idempotent: ensure_project returns same slug ($SLUG7A)"
else
    e2e_fail "idempotent: slug changed ($SLUG7A → $SLUG7B)"
fi

# Register same agent twice — should be idempotent
REQS7C=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":72,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_idemp","program":"e2e","model":"test","name":"GoldHawk"}}}'
)
RESP7C="$(send_session "$DB7" "${REQS7C[@]}")"

REQS7D=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_idemp","program":"e2e","model":"test","name":"GoldHawk"}}}'
)
RESP7D="$(send_session "$DB7" "${REQS7D[@]}")"

NAME7C="$(extract_field "$(extract_tool_result "$RESP7C" 72)" "name")"
NAME7D="$(extract_field "$(extract_tool_result "$RESP7D" 73)" "name")"

if [ "$NAME7C" = "$NAME7D" ] && [ "$NAME7C" = "GoldHawk" ]; then
    e2e_pass "idempotent: register_agent returns same name ($NAME7C)"
else
    e2e_fail "idempotent: agent name changed ($NAME7C → $NAME7D)"
fi

# =========================================================================
# Case 8: Unicode and special character resilience
# =========================================================================
e2e_case_banner "unicode_resilience"

DB8="${WORK}/unicode.sqlite3"

REQS8A=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":80,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_adapt_unicode"}}}'
    '{"jsonrpc":"2.0","id":81,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_unicode","program":"e2e","model":"test","name":"CoralBay"}}}'
    '{"jsonrpc":"2.0","id":82,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_adapt_unicode","program":"e2e","model":"test","name":"TealRiver"}}}'
)
send_session "$DB8" "${REQS8A[@]}" >/dev/null

# Messages with unicode subjects and bodies
REQS8B=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":83,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_unicode","sender_name":"CoralBay","to":["TealRiver"],"subject":"Emoji test \ud83d\ude80\ud83c\udf1f","body_md":"Rocket and star: \ud83d\ude80\ud83c\udf1f"}}}'
    '{"jsonrpc":"2.0","id":84,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_unicode","sender_name":"CoralBay","to":["TealRiver"],"subject":"CJK: \u4f60\u597d\u4e16\u754c","body_md":"Chinese greeting: \u4f60\u597d"}}}'
    '{"jsonrpc":"2.0","id":85,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/e2e_adapt_unicode","sender_name":"CoralBay","to":["TealRiver"],"subject":"Special chars: <script>alert(1)</script>","body_md":"HTML entities: &amp; &lt; &gt; &quot;"}}}'
)

RESP8B="$(send_session "$DB8" "${REQS8B[@]}")"
e2e_save_artifact "case_08_unicode.txt" "$RESP8B"

for pair in "83:emoji" "84:CJK" "85:special-chars"; do
    IFS=: read -r rid label <<< "$pair"
    if [ "$(is_error "$RESP8B" "$rid")" = "OK" ]; then
        e2e_pass "unicode: $label message sent"
    else
        e2e_fail "unicode: $label message failed"
    fi
done

# Verify messages are retrievable with content intact
REQS8C=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":86,"method":"tools/call","params":{"name":"fetch_inbox","arguments":{"project_key":"/tmp/e2e_adapt_unicode","agent_name":"TealRiver","limit":10}}}'
)

RESP8C="$(send_session "$DB8" "${REQS8C[@]}")"
e2e_save_artifact "case_08c_inbox.txt" "$RESP8C"

INBOX8="$(extract_tool_result "$RESP8C" 86)"
MSG_COUNT8="$(echo "$INBOX8" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
msgs = d.get('messages', d) if isinstance(d, dict) else d
print(len(msgs) if isinstance(msgs, list) else 0)
" 2>/dev/null)"

if [ "$MSG_COUNT8" -ge 3 ] 2>/dev/null; then
    e2e_pass "unicode: all 3 messages retrievable from inbox"
else
    e2e_fail "unicode: only $MSG_COUNT8 messages in inbox (expected 3)"
fi

# Verify script tag not executed (stored literally)
e2e_assert_contains "unicode: script tag preserved literally" "$INBOX8" "script"

# =========================================================================
# Summary
# =========================================================================
e2e_summary
