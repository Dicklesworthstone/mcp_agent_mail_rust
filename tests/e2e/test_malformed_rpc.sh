#!/usr/bin/env bash
# test_malformed_rpc.sh - E2E malformed protocol message handling
#
# Sends various invalid/malformed JSON-RPC messages to the MCP server
# via stdio transport and verifies proper error responses (not crashes).
#
# Tests:
#   1. Completely invalid JSON
#   2. Valid JSON but not JSON-RPC (missing jsonrpc field)
#   3. Wrong JSON-RPC version
#   4. Missing method field
#   5. Unknown method
#   6. Missing id field (notification-style for non-notification)
#   7. Extra unexpected fields
#   8. Array batch (MCP stdio doesn't support batching)
#   9. Empty object
#  10. Null params
#  11. Params as array (should be object)
#  12. Extremely large id
#  13. Negative id
#  14. String id
#  15. Rapid-fire mixed valid+invalid

E2E_SUITE="malformed_rpc"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Malformed JSON-RPC Protocol E2E Suite"

AM_BIN="$(e2e_ensure_binary am | tail -n 1)"
e2e_log "am binary: $AM_BIN"

WORK="$(e2e_mktemp "e2e_malformed")"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-malformed","version":"1.0"}}}'

# Preserve raw wire bytes for parser cases. Operating requests wait for
# initialize and send the readiness notification before the fixture frames.
send_rpc_frames() {
    local mode="$1" stderr_path="$2" expected="$3"
    shift 3
    printf '%s\0' "$@" | python3 -c '
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import time

binary, case_dir, mode, stderr_path, initialize, expected = sys.argv[1:]
frames = sys.stdin.buffer.read().split(b"\0")
if frames.pop() != b"":
    sys.exit("Incomplete raw stdio fixture")
workspace = Path(case_dir).absolute()
env = dict(os.environ, DATABASE_URL="sqlite:///" + (workspace / "mail.sqlite3").as_posix(),
           STORAGE_ROOT=str(workspace / "archive"), AM_INTERFACE_MODE="cli",
           AM_ALLOW_EPHEMERAL_PROJECT_ROOTS="1", FASTMCP_NO_BANNER="0", RUST_LOG="error")
# The former fixture allowed 0.3 seconds for startup and ten 0.3-second polls.
deadline = time.monotonic() + 3.3
with open(stderr_path, "wb") as log:
    server = subprocess.Popen([binary, "serve-stdio"], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=log, bufsize=0, env=env)

def exchange(outgoing, reply_count):
    received = queue.Queue()

    def send_and_read():
        try:
            pending = memoryview(b"\n".join(outgoing) + b"\n")
            while pending:
                written = server.stdin.write(pending)
                if not written:
                    raise OSError("short raw fixture write")
                pending = pending[written:]
            for _ in range(max(1, reply_count)):
                line = server.stdout.readline()
                received.put(line)
                if not line:
                    break
        except OSError as error:
            received.put(error)

    threading.Thread(target=send_and_read, daemon=True).start()
    replies = []
    # The notification-shaped opening has no reply to time. It is the one
    # case that waits for stdout to close under the shutdown deadline.
    read_deadline = deadline if reply_count else time.monotonic() + 5
    for _ in range(max(1, reply_count)):
        remaining = read_deadline - time.monotonic()
        try:
            if remaining <= 0:
                raise queue.Empty
            line = received.get(timeout=remaining)
        except queue.Empty:
            limit = "3.3-second reply" if reply_count else "five-second shutdown"
            print(f"stdio {limit} deadline exceeded. See {stderr_path}", file=sys.stderr)
            sys.exit(124)
        if isinstance(line, OSError):
            raise line
        if not line:
            if not reply_count:
                break
            sys.exit(f"Server closed stdout while a reply was pending. See {stderr_path}")
        sys.stdout.buffer.write(line)
        replies.append(line)
    return replies

try:
    reply_count = len(json.loads(expected))
    if mode == "ready":
        first, = exchange([initialize.encode("utf-8")], 1)
        reply = json.loads(first)
        if (reply.get("jsonrpc") != "2.0" or reply.get("id") != 1 or "error" in reply
                or reply.get("result", {}).get("protocolVersion") != "2024-11-05"):
            sys.exit(f"Unexpected initialize reply. See {stderr_path}")
        frames.insert(0, b"""{"jsonrpc":"2.0","method":"notifications/initialized"}""")
        reply_count -= 1
    exchange(frames, reply_count)
    # Opening-frame rejection must end the session with stdin still open.
    # An initialized session shuts down when the client closes stdin.
    if mode == "reject":
        server.wait(timeout=5)
    stdout, _ = server.communicate(timeout=5)
    sys.stdout.buffer.write(stdout)
    expected_status = 1 if mode == "reject" else 0
    if server.returncode != expected_status:
        sys.exit(f"stdio server exited with status {server.returncode}, expected {expected_status}. See {stderr_path}")
    if mode == "reject" and not reply_count and b"Server ready." not in Path(stderr_path).read_bytes():
        sys.exit(f"Server exited before reaching the stdio transport. See {stderr_path}")
except subprocess.TimeoutExpired as error:
    sys.stdout.buffer.write(error.output or b"")
    print(f"stdio server exceeded the five-second shutdown deadline. See {stderr_path}", file=sys.stderr)
    sys.exit(124)
finally:
    if server.poll() is None:
        server.kill()
    server.wait()
' "$AM_BIN" "$WORK" "$mode" "$stderr_path" "$INIT_REQ" "$expected"
}

check_rpc_replies() {
    printf '%s' "$1" | python3 -c '
from collections import Counter
import json
import sys

def classify(frame):
    if frame.get("jsonrpc") != "2.0" or ("result" in frame) == ("error" in frame):
        sys.exit(f"Unexpected JSON-RPC reply {frame}")
    request_id = frame.get("id")
    if "error" in frame:
        error = frame["error"]
        return request_id, error["code"], error["message"]
    result = frame["result"]
    if result.get("protocolVersion") == "2024-11-05" and "serverInfo" in result:
        return request_id, "initialize"
    if "tools" in result:
        names = {tool["name"] for tool in result["tools"]}
        if {"ensure_project", "send_message", "fetch_inbox"} <= names:
            return request_id, "tools"
    if result.get("isError", False) is False and "content" in result:
        body = json.loads(result["content"][0]["text"])
        if body.get("human_key") == "/tmp/rapid_test" and type(body.get("id")) is int and body["id"] > 0:
            return request_id, "project"
    sys.exit(f"Unexpected successful result for request {request_id}")

# Compare whole reply sets. The server can report parser errors before an
# earlier valid request finishes. Python preserves the large integer ID.
actual = Counter(json.dumps(classify(json.loads(line))) for line in sys.stdin)
expected = Counter(json.dumps(reply) for reply in json.loads(sys.argv[1]))
if actual != expected:
    sys.exit(f"Unexpected replies. Expected {list(expected.elements())}, got {list(actual.elements())}")
' "$2"
}

run_rpc_case() {
    local artifact="$1" label="$2" mode="$3" expected="$4"
    shift 4
    local stderr_path="$E2E_ARTIFACT_DIR/logs/$artifact.stderr.log"
    local response rc=0 validation
    e2e_case_banner "$label"
    response="$(send_rpc_frames "$mode" "$stderr_path" "$expected" "$@")" || rc=$?
    e2e_save_artifact "$artifact.txt" "$response"
    if [ "$rc" -ne 0 ]; then
        e2e_fail "$label (session exited $rc)"
    elif validation="$(check_rpc_replies "$response" "$expected" 2>&1)"; then
        e2e_pass "$label returned the expected replies and exit status"
    else
        e2e_fail "$label ($validation)"
    fi
}

# ===========================================================================
# Replies are [id, error code, error message] or [id, successful result kind].
# Parser failures may omit the unknown ID on the wire. FastMCP rejects a
# malformed opening before negotiating an era, then closes with status 1.
run_rpc_case case_01_invalid_json "Invalid JSON" reject '[[null,-32700,"Parse error"]]' \
    '{{{{garbage not json at all}}}}'

run_rpc_case case_02_not_jsonrpc "Valid JSON, not JSON-RPC" reject '[[null,-32600,"Invalid Request"]]' \
    '{"hello":"world","foo":42}'

run_rpc_case case_03_wrong_version "Wrong JSON-RPC version" reject '[[1,-32600,"Invalid Request"]]' \
    '{"jsonrpc":"1.0","id":1,"method":"initialize","params":{}}'

run_rpc_case case_04_missing_method "Missing method field" reject '[[1,-32600,"Invalid Request"]]' \
    '{"jsonrpc":"2.0","id":1,"params":{}}'

run_rpc_case case_05_unknown_method "Unknown method after readiness" ready \
    '[[1,"initialize"],[2,-32601,"method is not part of exact MCP 2024-11-05"]]' \
    '{"jsonrpc":"2.0","id":2,"method":"totally/unknown/method","params":{}}'

run_rpc_case case_06_missing_id "Missing id field" reject '[]' \
    '{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'

run_rpc_case case_07_extra_fields "Extra fields in initialize envelope" reject '[[1,-32600,"Invalid Request"]]' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}},"extra_field":"surprise","another":42,"nested":{"deep":true}}'

run_rpc_case case_08_array_batch "Array batch request" reject '[[null,-32600,"Invalid Request"]]' \
    '[{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}},{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}]'

run_rpc_case case_09_empty_object "Empty JSON object" reject '[[null,-32600,"Invalid Request"]]' \
    '{}'

run_rpc_case case_10_null_params "Null params after readiness" ready '[[1,"initialize"],[2,"tools"]]' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":null}'

run_rpc_case case_11_params_array "Array params after readiness" ready \
    '[[1,"initialize"],[2,-32600,"invalid exact MCP 2024-11-05 envelope"]]' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":["ensure_project","/tmp/test"]}'

run_rpc_case case_12_large_id "Large numeric id" raw '[[99999999999999999,"initialize"]]' \
    '{"jsonrpc":"2.0","id":99999999999999999,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'

run_rpc_case case_13_negative_id "Negative id" raw '[[-1,"initialize"]]' \
    '{"jsonrpc":"2.0","id":-1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'

run_rpc_case case_14_string_id "String id" raw '[["string-id-123","initialize"]]' \
    '{"jsonrpc":"2.0","id":"string-id-123","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'

run_rpc_case case_15_rapid_fire "Rapid-fire mixed valid/invalid" ready \
    '[[1,"initialize"],[2,"tools"],[3,"project"],[null,-32700,"Parse error"],[null,-32600,"Invalid Request"],[null,-32700,"Parse error"]]' \
    'not json' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
    '{}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/rapid_test"}}}' \
    'also not json'

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
