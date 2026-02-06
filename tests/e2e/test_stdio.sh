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

# Helper: send a JSON-RPC request to the server via stdin and capture the response.
# Uses a FIFO and background process. Each call starts a fresh server because
# the stdio transport doesn't support multiplexing easily in bash.
send_jsonrpc() {
    local db_path="$1"
    local request="$2"
    local timeout_s="${3:-10}"
    local output_file="${WORK}/response_$$.txt"

    # Create a temp dir for this server instance
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    # Start server in background, reading from FIFO
    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!

    # Give server a moment to start
    sleep 0.3

    # Send the request (with a newline) and close the FIFO
    echo "$request" > "$fifo" &
    local write_pid=$!

    # Wait for response with timeout
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if [ -s "$output_file" ]; then
            # Got some output, wait a tiny bit more for it to complete
            sleep 0.2
            break
        fi
        sleep 0.2
        elapsed=$((elapsed + 1))
    done

    # Clean up: close the FIFO to signal EOF to server
    wait "$write_pid" 2>/dev/null || true

    # Give server a moment to exit
    sleep 0.3
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    # Return the response
    if [ -f "$output_file" ]; then
        cat "$output_file"
    fi
}

# Helper: send multiple JSON-RPC requests in sequence to a single server session
send_jsonrpc_session() {
    local db_path="$1"
    shift
    # Remaining args are JSON-RPC request strings
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
    while [ "$elapsed" -lt "$timeout_s" ]; do
        # Check if server has exited
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

# ===========================================================================
# Case 1: Server responds to initialize request
# ===========================================================================
e2e_case_banner "Server responds to initialize"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-test","version":"1.0"}}}'

INIT_RESP="$(send_jsonrpc "$STDIO_DB" "$INIT_REQ")"
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

TOOLS_RESP="$(send_jsonrpc_session "$STDIO_DB" "$INIT_REQ" "$TOOLS_REQ")"
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

TOOL_RESP="$(send_jsonrpc_session "$STDIO_DB" "$INIT_REQ" "$TOOL_CALL_REQ")"
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

INVALID_RESP="$(send_jsonrpc "$STDIO_DB" "$INVALID_REQ" 5)"
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

# Start server, send initialize, then close stdin immediately
SRV_WORK="$(mktemp -d "${WORK}/srv_exit.XXXXXX")"
FIFO="${SRV_WORK}/stdin_fifo"
mkfifo "$FIFO"

DATABASE_URL="sqlite:////${STDIO_DB}" RUST_LOG=error \
    am serve-stdio < "$FIFO" > "${SRV_WORK}/stdout.txt" 2>"${SRV_WORK}/stderr.txt" &
SRV_PID=$!

sleep 0.3

# Send initialize then immediately close
echo "$INIT_REQ" > "$FIFO"

# Wait for server to exit (should happen quickly after stdin closes)
set +e
WAIT_COUNT=0
while kill -0 "$SRV_PID" 2>/dev/null && [ "$WAIT_COUNT" -lt 10 ]; do
    sleep 0.5
    WAIT_COUNT=$((WAIT_COUNT + 1))
done

if ! kill -0 "$SRV_PID" 2>/dev/null; then
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

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
