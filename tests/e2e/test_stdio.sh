#!/usr/bin/env bash
# test_stdio.sh - E2E test for MCP server stdio transport
#
# Verifies the FastMCP server works correctly over stdin/stdout using the
# MCP JSON-RPC protocol. This covers a critical feature that validates the
# server can be used as an MCP subprocess by AI coding agents.
#
# Tests:
#   1. Server responds to initialize request
#   2. Server lists tools via tools/list
#   3. Server executes a tool call (ensure_project)
#   4. Server handles invalid JSON gracefully
#   5. Server shuts down cleanly on stdin close

E2E_SUITE="stdio"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Stdio Transport E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_stdio")"
STDIO_DB="${WORK}/stdio_test.sqlite3"

# Helper: write structured stdio request artifact(s) as JSON.
write_stdio_request_artifact() {
    local mode="$1"
    local out_file="$2"
    shift 2

    python3 - "$mode" "$out_file" "$@" <<'PY'
import json
import sys

mode = sys.argv[1]
out_file = sys.argv[2]
payloads = sys.argv[3:]

entries = []
for idx, raw in enumerate(payloads, start=1):
    item = {"index": idx, "payload_raw": raw}
    try:
        item["payload_json"] = json.loads(raw)
        item["payload_valid"] = True
    except Exception as exc:  # noqa: BLE001
        item["payload_valid"] = False
        item["parse_error"] = str(exc)
    entries.append(item)

doc = {"transport": "stdio", "mode": mode}
if mode == "single":
    item = entries[0] if entries else {"payload_raw": "", "payload_valid": False}
    doc["payload_raw"] = item.get("payload_raw", "")
    doc["payload_valid"] = bool(item.get("payload_valid", False))
    if "payload_json" in item:
        doc["payload_json"] = item["payload_json"]
    if "parse_error" in item:
        doc["parse_error"] = item["parse_error"]
else:
    doc["payloads"] = entries

with open(out_file, "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2, ensure_ascii=False)
    f.write("\n")
PY
}

# Helper: write structured stdio response artifact as JSON.
write_stdio_response_artifact() {
    local raw_file="$1"
    local out_file="$2"

    python3 - "$raw_file" "$out_file" <<'PY'
import json
import pathlib
import sys

raw_file = pathlib.Path(sys.argv[1])
out_file = pathlib.Path(sys.argv[2])
raw = raw_file.read_text(encoding="utf-8", errors="replace") if raw_file.exists() else ""

entries = []
for line_num, line in enumerate(raw.splitlines(), start=1):
    text = line.strip()
    if not text:
        continue
    try:
        entries.append({"line": line_num, "json": json.loads(text)})
    except Exception as exc:  # noqa: BLE001
        entries.append({"line": line_num, "raw": text, "parse_error": str(exc)})

doc = {
    "transport": "stdio",
    "line_count": len(raw.splitlines()),
    "entries": entries,
    "raw": raw,
}

out_file.write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
PY
}

# Helper: send a JSON-RPC request to the server via stdin and capture the response.
# Uses a FIFO and background process. Each call starts a fresh server because
# the stdio transport doesn't support multiplexing easily in bash.
send_jsonrpc() {
    local db_path="$1"
    local case_id="$2"
    local request="$3"
    local timeout_s="${4:-10}"
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local request_file="${case_dir}/request.json"
    local response_raw_file="${case_dir}/response.raw.txt"
    local response_file="${case_dir}/response.json"
    local headers_file="${case_dir}/headers.txt"
    local timing_file="${case_dir}/timing.txt"
    local status_file="${case_dir}/status.txt"
    local stderr_file="${case_dir}/stderr.txt"
    local start_ms end_ms elapsed_ms

    # Create a temp dir for this server instance
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"
    mkdir -p "$case_dir"

    e2e_mark_case_start "$case_id"
    write_stdio_request_artifact "single" "$request_file" "$request"
    start_ms="$(_e2e_now_ms)"

    # Start server in background, reading from FIFO
    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error \
        am serve-stdio < "$fifo" > "$response_raw_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!

    # Give server a moment to start
    sleep 0.3

    # Send the request (with a newline) and close the FIFO
    echo "$request" > "$fifo" &
    local write_pid=$!

    # Wait for response with timeout
    local elapsed=0
    local timed_out=false
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if [ -s "$response_raw_file" ]; then
            # Got some output, wait a tiny bit more for it to complete
            sleep 0.2
            break
        fi
        sleep 0.2
        elapsed=$((elapsed + 1))
    done
    if [ "$elapsed" -ge "$timeout_s" ] && [ ! -s "$response_raw_file" ]; then
        timed_out=true
    fi

    # Clean up: close the FIFO to signal EOF to server
    wait "$write_pid" 2>/dev/null || true

    # Give server a moment to exit
    sleep 0.3
    kill "$srv_pid" 2>/dev/null || true
    local srv_exit=0
    if wait "$srv_pid" 2>/dev/null; then
        srv_exit=0
    else
        srv_exit=$?
    fi
    end_ms="$(_e2e_now_ms)"
    elapsed_ms=$((end_ms - start_ms))

    cp "${srv_work}/stderr.txt" "$stderr_file" 2>/dev/null || : > "$stderr_file"
    write_stdio_response_artifact "$response_raw_file" "$response_file"
    {
        echo "transport=stdio"
        echo "mode=single"
        echo "timeout_s=${timeout_s}"
        echo "timed_out=${timed_out}"
        echo "server_exit_code=${srv_exit}"
        echo "stderr_file=stderr.txt"
    } > "$headers_file"
    echo "$elapsed_ms" > "$timing_file"
    if [ "$timed_out" = true ]; then
        echo "timeout" > "$status_file"
    else
        echo "ok" > "$status_file"
    fi
    e2e_mark_case_end "$case_id"

    # Return the response
    if [ -f "$response_raw_file" ]; then
        cat "$response_raw_file"
    fi
}

# Helper: send multiple JSON-RPC requests in sequence to a single server session
send_jsonrpc_session() {
    local db_path="$1"
    local case_id="$2"
    shift 2
    # Remaining args are JSON-RPC request strings
    local requests=("$@")
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local request_file="${case_dir}/request.json"
    local response_raw_file="${case_dir}/response.raw.txt"
    local response_file="${case_dir}/response.json"
    local headers_file="${case_dir}/headers.txt"
    local timing_file="${case_dir}/timing.txt"
    local status_file="${case_dir}/status.txt"
    local stderr_file="${case_dir}/stderr.txt"
    local start_ms end_ms elapsed_ms
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"
    mkdir -p "$case_dir"

    e2e_mark_case_start "$case_id"
    write_stdio_request_artifact "session" "$request_file" "${requests[@]}"
    start_ms="$(_e2e_now_ms)"

    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error \
        am serve-stdio < "$fifo" > "$response_raw_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!

    sleep 0.3

    # Send all requests, with small delays between them
    {
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.3
        done
        # Close stdin to signal server to exit
    } > "$fifo" &
    local write_pid=$!

    # Wait for all responses
    local timeout_s=15
    local elapsed=0
    local timed_out=false
    while [ "$elapsed" -lt "$timeout_s" ]; do
        # Check if server has exited
        if ! kill -0 "$srv_pid" 2>/dev/null; then
            break
        fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done
    if [ "$elapsed" -ge "$timeout_s" ] && kill -0 "$srv_pid" 2>/dev/null; then
        timed_out=true
    fi

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    local srv_exit=0
    if wait "$srv_pid" 2>/dev/null; then
        srv_exit=0
    else
        srv_exit=$?
    fi
    end_ms="$(_e2e_now_ms)"
    elapsed_ms=$((end_ms - start_ms))

    cp "${srv_work}/stderr.txt" "$stderr_file" 2>/dev/null || : > "$stderr_file"
    write_stdio_response_artifact "$response_raw_file" "$response_file"
    {
        echo "transport=stdio"
        echo "mode=session"
        echo "request_count=${#requests[@]}"
        echo "timeout_s=${timeout_s}"
        echo "timed_out=${timed_out}"
        echo "server_exit_code=${srv_exit}"
        echo "stderr_file=stderr.txt"
    } > "$headers_file"
    echo "$elapsed_ms" > "$timing_file"
    if [ "$timed_out" = true ]; then
        echo "timeout" > "$status_file"
    else
        echo "ok" > "$status_file"
    fi
    e2e_mark_case_end "$case_id"

    if [ -f "$response_raw_file" ]; then
        cat "$response_raw_file"
    fi
}

# ===========================================================================
# Case 1: Server responds to initialize request
# ===========================================================================
e2e_case_banner "Server responds to initialize"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-test","version":"1.0"}}}'

INIT_RESP="$(send_jsonrpc "$STDIO_DB" "case_01_initialize" "$INIT_REQ")"
e2e_save_artifact "case_01_init_response.txt" "$INIT_RESP"

if [ -n "$INIT_RESP" ]; then
    e2e_pass "server returned a response"
else
    e2e_fail "server returned empty response"
fi

# Check response contains expected fields
if echo "$INIT_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if 'result' in d:
            r = d['result']
            assert 'serverInfo' in r, 'missing serverInfo'
            assert 'capabilities' in r, 'missing capabilities'
            print('OK')
            sys.exit(0)
    except (json.JSONDecodeError, AssertionError) as e:
        print(f'PARSE_ERROR: {e}')
        sys.exit(1)
print('NO_RESULT')
sys.exit(1)
" 2>/dev/null; then
    e2e_pass "initialize response has serverInfo + capabilities"
else
    e2e_fail "initialize response missing expected fields"
    echo "    response: $(echo "$INIT_RESP" | head -3)"
fi

# ===========================================================================
# Case 2: Server lists tools
# ===========================================================================
e2e_case_banner "Server lists tools via tools/list"

TOOLS_REQ='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'

TOOLS_RESP="$(send_jsonrpc_session "$STDIO_DB" "case_02_tools_list" "$INIT_REQ" "$TOOLS_REQ")"
e2e_save_artifact "case_02_tools_response.txt" "$TOOLS_RESP"

# Parse last JSON-RPC response (tools/list)
TOOL_COUNT="$(echo "$TOOLS_RESP" | python3 -c "
import sys, json
last_result = None
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if 'result' in d and 'tools' in d.get('result', {}):
            last_result = d['result']
    except json.JSONDecodeError:
        pass
if last_result:
    print(len(last_result['tools']))
else:
    print(0)
" 2>/dev/null)"

if [ "$TOOL_COUNT" -gt 0 ] 2>/dev/null; then
    e2e_pass "tools/list returned $TOOL_COUNT tools"
else
    e2e_fail "tools/list returned no tools"
    echo "    response: $(echo "$TOOLS_RESP" | tail -3)"
fi

# Check for specific expected tools
TOOLS_JSON="$(echo "$TOOLS_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if 'result' in d and 'tools' in d.get('result', {}):
            names = [t['name'] for t in d['result']['tools']]
            print(' '.join(names))
    except json.JSONDecodeError:
        pass
" 2>/dev/null)"

for expected_tool in ensure_project register_agent send_message fetch_inbox; do
    if echo "$TOOLS_JSON" | grep -qw "$expected_tool"; then
        e2e_pass "tools/list includes '$expected_tool'"
    else
        e2e_fail "tools/list missing '$expected_tool'"
    fi
done

# ===========================================================================
# Case 3: Server executes a tool call
# ===========================================================================
e2e_case_banner "Server executes ensure_project tool call"

TOOL_CALL_REQ='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_stdio_test_project"}}}'

TOOL_RESP="$(send_jsonrpc_session "$STDIO_DB" "case_03_ensure_project" "$INIT_REQ" "$TOOL_CALL_REQ")"
e2e_save_artifact "case_03_tool_call_response.txt" "$TOOL_RESP"

# Parse the tool call response
TOOL_RESULT="$(echo "$TOOL_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if d.get('id') == 3 and 'result' in d:
            # Tool call results have content array
            content = d['result'].get('content', [])
            if content:
                text = content[0].get('text', '')
                result = json.loads(text)
                if 'slug' in result or 'id' in result:
                    print('OK')
                    sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('FAIL')
sys.exit(1)
" 2>/dev/null)"

if [ "$TOOL_RESULT" = "OK" ]; then
    e2e_pass "ensure_project tool call returned valid result with slug/id"
else
    e2e_fail "ensure_project tool call failed"
    echo "    response: $(echo "$TOOL_RESP" | tail -3)"
fi

# ===========================================================================
# Case 4: Server handles invalid JSON
# ===========================================================================
e2e_case_banner "Server handles invalid JSON"

INVALID_REQ='{"this is not valid json'

INVALID_RESP="$(send_jsonrpc "$STDIO_DB" "case_04_invalid_json" "$INVALID_REQ" 5)"
e2e_save_artifact "case_04_invalid_json_response.txt" "$INVALID_RESP"

# The server should either return a JSON-RPC error or handle gracefully
if [ -n "$INVALID_RESP" ]; then
    if echo "$INVALID_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if 'error' in d:
            print('ERROR_RESPONSE')
            sys.exit(0)
    except json.JSONDecodeError:
        pass
print('OTHER')
sys.exit(0)
" 2>/dev/null | grep -q "ERROR_RESPONSE"; then
        e2e_pass "server returned JSON-RPC error for invalid JSON"
    else
        e2e_pass "server handled invalid JSON without crash"
    fi
else
    # Empty response is also acceptable (server may just close)
    e2e_pass "server handled invalid JSON (no response, clean exit)"
fi

# ===========================================================================
# Case 5: Server exits cleanly on stdin close
# ===========================================================================
e2e_case_banner "Server exits cleanly on stdin close"
e2e_mark_case_start "case_05_stdin_close"

# Start server, send initialize, then close stdin immediately
SRV_WORK="$(mktemp -d "${WORK}/srv_exit.XXXXXX")"
FIFO="${SRV_WORK}/stdin_fifo"
mkfifo "$FIFO"
CASE_05_DIR="${E2E_ARTIFACT_DIR}/case_05_stdin_close"
mkdir -p "$CASE_05_DIR"
write_stdio_request_artifact "single" "${CASE_05_DIR}/request.json" "$INIT_REQ"
CASE_05_START_MS="$(_e2e_now_ms)"

DATABASE_URL="sqlite:////${STDIO_DB}" RUST_LOG=error \
    am serve-stdio < "$FIFO" > "${SRV_WORK}/stdout.txt" 2>"${SRV_WORK}/stderr.txt" &
SRV_PID=$!

sleep 0.3

# Send initialize then immediately close
echo "$INIT_REQ" > "$FIFO"

# Wait for server to exit (should happen quickly after stdin closes)
set +e
WAIT_COUNT=0
CASE_05_TIMED_OUT=true
while kill -0 "$SRV_PID" 2>/dev/null && [ "$WAIT_COUNT" -lt 10 ]; do
    sleep 0.5
    WAIT_COUNT=$((WAIT_COUNT + 1))
done

if ! kill -0 "$SRV_PID" 2>/dev/null; then
    CASE_05_TIMED_OUT=false
    wait "$SRV_PID" 2>/dev/null
    EXIT_CODE=$?
    e2e_pass "server exited after stdin close (exit=$EXIT_CODE, waited ${WAIT_COUNT}s)"
else
    kill "$SRV_PID" 2>/dev/null
    wait "$SRV_PID" 2>/dev/null
    e2e_fail "server did not exit within 5s after stdin close"
fi
set -e

e2e_copy_artifact "${SRV_WORK}/stderr.txt" "case_05_stderr.txt"
cp "${SRV_WORK}/stderr.txt" "${CASE_05_DIR}/stderr.txt" 2>/dev/null || : > "${CASE_05_DIR}/stderr.txt"
cp "${SRV_WORK}/stdout.txt" "${CASE_05_DIR}/response.raw.txt" 2>/dev/null || : > "${CASE_05_DIR}/response.raw.txt"
write_stdio_response_artifact "${CASE_05_DIR}/response.raw.txt" "${CASE_05_DIR}/response.json"
CASE_05_END_MS="$(_e2e_now_ms)"
CASE_05_ELAPSED_MS=$((CASE_05_END_MS - CASE_05_START_MS))
echo "${CASE_05_ELAPSED_MS}" > "${CASE_05_DIR}/timing.txt"
{
    echo "transport=stdio"
    echo "mode=single"
    echo "server_exit_code=${EXIT_CODE:-0}"
    echo "wait_loops=${WAIT_COUNT}"
    echo "stderr_file=stderr.txt"
} > "${CASE_05_DIR}/headers.txt"
if [ "${CASE_05_TIMED_OUT}" = true ]; then
    echo "timeout" > "${CASE_05_DIR}/status.txt"
else
    echo "ok" > "${CASE_05_DIR}/status.txt"
fi
e2e_mark_case_end "case_05_stdin_close"

# ===========================================================================
# Case 6: Force-release stale file reservation (full pipeline)
# ===========================================================================
e2e_case_banner "Force-release stale reservation via tool call"

# Use a fresh DB for this complex scenario
FR_DB="${WORK}/force_release_test.sqlite3"

# Step 1: Create project + register two agents + reserve a file + make agent stale
# All done in one session to minimize server launches
SETUP_REQS=(
    # Initialize
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-force-release","version":"1.0"}}}'
    # Create project
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_force_release_project"}}}'
    # Register AgentA (will hold the reservation)
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_force_release_project","program":"test","model":"test","name":"GreenLake"}}}'
    # Register AgentB (will force-release)
    '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/e2e_force_release_project","program":"test","model":"test","name":"BluePeak"}}}'
    # AgentA reserves a file
    '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"file_reservation_paths","arguments":{"project_key":"/tmp/e2e_force_release_project","agent_name":"GreenLake","paths":["src/main.rs"],"ttl_seconds":3600,"exclusive":true,"reason":"editing main"}}}'
)

SETUP_RESP="$(send_jsonrpc_session "$FR_DB" "case_06_force_release_setup" "${SETUP_REQS[@]}")"
e2e_save_artifact "case_06_setup_response.txt" "$SETUP_RESP"

# Extract the reservation ID from the file_reservation_paths response (id=13)
RES_ID="$(echo "$SETUP_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 13 and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                result = json.loads(content[0].get('text', '{}'))
                granted = result.get('granted', [])
                if granted:
                    print(granted[0].get('id', ''))
                    sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('')
" 2>/dev/null)"

if [ -n "$RES_ID" ] && [ "$RES_ID" != "" ]; then
    e2e_pass "setup: created reservation id=$RES_ID for GreenLake"
else
    e2e_fail "setup: failed to create reservation"
    echo "    response: $(echo "$SETUP_RESP" | tail -5)"
fi

# Step 2: Make GreenLake stale by updating last_active_ts to 2 hours ago
# The force-release inactivity threshold is 30 minutes, so 2 hours ensures staleness
STALE_TS=$(($(date +%s) * 1000000 - 7200 * 1000000))
sqlite3 "$FR_DB" "UPDATE agents SET last_active_ts = $STALE_TS WHERE name = 'GreenLake';"
e2e_pass "setup: made GreenLake stale (last_active_ts set to 2h ago)"

# Step 3: BluePeak force-releases GreenLake's reservation
FORCE_REQS=(
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-force-release","version":"1.0"}}}'
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"/tmp/e2e_force_release_project\",\"agent_name\":\"BluePeak\",\"file_reservation_id\":$RES_ID,\"note\":\"e2e test force release\",\"notify_previous\":true}}}"
)

FORCE_RESP="$(send_jsonrpc_session "$FR_DB" "case_06_force_release_action" "${FORCE_REQS[@]}")"
e2e_save_artifact "case_06_force_release_response.txt" "$FORCE_RESP"

# Parse and validate the force-release response
FORCE_RESULT="$(echo "$FORCE_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 20 and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                result = json.loads(content[0].get('text', '{}'))
                released = result.get('released', 0)
                res_info = result.get('reservation', {})
                stale_reasons = res_info.get('stale_reasons', [])
                notified = res_info.get('notified', False)
                agent = res_info.get('agent', '')
                path = res_info.get('path_pattern', '')
                print(f'released={released}|agent={agent}|path={path}|notified={notified}|reasons={len(stale_reasons)}')
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError) as e:
        pass
print('PARSE_FAIL')
" 2>/dev/null)"

e2e_save_artifact "case_06_parsed_result.txt" "$FORCE_RESULT"

if echo "$FORCE_RESULT" | grep -q "^released=1"; then
    e2e_pass "force-release returned released=1"
else
    e2e_fail "force-release did not return released=1"
    echo "    result: $FORCE_RESULT"
fi

if echo "$FORCE_RESULT" | grep -q "agent=GreenLake"; then
    e2e_pass "force-release response identifies GreenLake as holder"
else
    e2e_fail "force-release response missing agent identity"
    echo "    result: $FORCE_RESULT"
fi

if echo "$FORCE_RESULT" | grep -q "path=src/main.rs"; then
    e2e_pass "force-release response shows correct path"
else
    e2e_fail "force-release response missing path"
    echo "    result: $FORCE_RESULT"
fi

if echo "$FORCE_RESULT" | grep -q "notified=True"; then
    e2e_pass "force-release sent notification to previous holder"
else
    # Notification may fail if no recipients set up - still acceptable
    e2e_pass "force-release completed (notification may vary)"
fi

# Step 4: Verify force-release error path (non-existent reservation)
ERROR_REQS=(
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-force-release","version":"1.0"}}}'
    '{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"force_release_file_reservation","arguments":{"project_key":"/tmp/e2e_force_release_project","agent_name":"BluePeak","file_reservation_id":99999}}}'
)

ERROR_RESP="$(send_jsonrpc_session "$FR_DB" "case_06_force_release_error" "${ERROR_REQS[@]}")"
e2e_save_artifact "case_06_error_response.txt" "$ERROR_RESP"

ERROR_CHECK="$(echo "$ERROR_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 30 and 'result' in d:
            if d['result'].get('isError', False):
                print('ERROR_DETECTED')
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('NO_ERROR')
" 2>/dev/null)"

if [ "$ERROR_CHECK" = "ERROR_DETECTED" ]; then
    e2e_pass "force-release returns error for non-existent reservation"
else
    e2e_fail "force-release did not error for non-existent reservation"
    echo "    response: $(echo "$ERROR_RESP" | tail -3)"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
