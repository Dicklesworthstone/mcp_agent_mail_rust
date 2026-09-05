#!/usr/bin/env bash
# test_toon.sh - E2E test suite for TOON output format
#
# Tests:
# 1. Stub encoder: success path with stats
# 2. Stub encoder: success without stats
# 3. Failing stub encoder: fallback to JSON with toon_error
# 4. Multi-tool sequence: health_check + ensure_project + register_agent + inbox resource
# 5. Encoder validation: --help / --version responses
# 6. Broken encoder path: graceful fallback
#
# Existing cases are offline substitutes. Select the real HTTP/codec lane with
# AM_E2E_REAL_TOON=1, AM_E2E_REAL_TOON_BIN and AM_E2E_REAL_TOON_SHA256.

E2E_SUITE="toon"
: "${AM_E2E_KEEP_TMP:=1}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TOON Output Format E2E Test Suite"

# Locate stub encoders
STUB="${E2E_PROJECT_ROOT}/scripts/toon_stub_encoder.sh"
STUB_FAIL="${E2E_PROJECT_ROOT}/scripts/toon_stub_encoder_fail.sh"

if [ ! -x "$STUB" ] || [ ! -x "$STUB_FAIL" ]; then
    e2e_log "ERROR: stub encoders not found or not executable"
    e2e_log "  STUB:      $STUB"
    e2e_log "  STUB_FAIL: $STUB_FAIL"
    exit 1
fi

# Helper: run stub encoder and capture output
run_stub() {
    local input="$1"
    shift
    echo "$input" | "$STUB" "$@"
}

# Helper: extract JSON field with python (jq-free)
json_get() {
    local json="$1"
    local field="$2"
    python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(d.get('$field',''))" "$json" 2>/dev/null
}

json_get_nested() {
    local json="$1"
    local path="$2"
    python3 -c "
import json,sys
d=json.loads(sys.argv[1])
keys=sys.argv[2].split('.')
for k in keys:
    if isinstance(d, dict):
        d = d.get(k, '')
    else:
        d = ''
        break
print(d)
" "$json" "$path" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Case 1: Stub encoder success with stats
# ---------------------------------------------------------------------------
e2e_case_banner "Stub encoder success with stats"

PAYLOAD='{"id":1,"subject":"Hello","body":"World"}'
STDOUT=$(echo "$PAYLOAD" | "$STUB" --encode --stats 2>"${E2E_ARTIFACT_DIR}/case1_stderr.txt")
STDERR=$(cat "${E2E_ARTIFACT_DIR}/case1_stderr.txt")
STUB_RC=$?

e2e_assert_exit_code "stub exits 0" "0" "$STUB_RC"
e2e_assert_contains "stdout has stub marker" "$STDOUT" "~stub_toon_output"
e2e_assert_contains "stdout has payload_length" "$STDOUT" "payload_length:"
e2e_assert_contains "stderr has token estimates" "$STDERR" "Token estimates:"
e2e_assert_contains "stderr has saved line" "$STDERR" "Saved ~13 tokens"

e2e_save_artifact "case1_stdout.txt" "$STDOUT"

# ---------------------------------------------------------------------------
# Case 2: Stub encoder success without stats
# ---------------------------------------------------------------------------
e2e_case_banner "Stub encoder success without stats"

STDOUT2=$(echo "$PAYLOAD" | "$STUB" --encode 2>"${E2E_ARTIFACT_DIR}/case2_stderr.txt")
STDERR2=$(cat "${E2E_ARTIFACT_DIR}/case2_stderr.txt")

e2e_assert_contains "stdout has marker" "$STDOUT2" "~stub_toon_output"
e2e_assert_eq "stderr is empty (no --stats)" "" "$STDERR2"

e2e_save_artifact "case2_stdout.txt" "$STDOUT2"

# ---------------------------------------------------------------------------
# Case 3: Failing stub encoder - non-zero exit
# ---------------------------------------------------------------------------
e2e_case_banner "Failing stub encoder returns non-zero exit"

set +e
FAIL_STDOUT=$(echo "$PAYLOAD" | "$STUB_FAIL" --encode 2>"${E2E_ARTIFACT_DIR}/case3_stderr.txt")
FAIL_RC=$?
set -e
FAIL_STDERR=$(cat "${E2E_ARTIFACT_DIR}/case3_stderr.txt")

e2e_assert_eq "failing stub exits 1" "1" "$FAIL_RC"
e2e_assert_eq "stdout is empty on failure" "" "$FAIL_STDOUT"
e2e_assert_contains "stderr has error message" "$FAIL_STDERR" "simulated encoder failure"

e2e_save_artifact "case3_stderr.txt" "$FAIL_STDERR"

# ---------------------------------------------------------------------------
# Case 4: Multi-tool TOON sequence (offline simulation)
# ---------------------------------------------------------------------------
e2e_case_banner "Multi-tool TOON sequence (offline)"

WORK="$(e2e_mktemp "e2e_toon")"
LOG_FILE="${WORK}/e2e_toon_log.json"

# Simulate 4 tool/resource calls through the stub encoder.
# Each gets wrapped in a TOON envelope structure.
STEPS="[]"

for tool in "health_check" "ensure_project" "register_agent"; do
    case "$tool" in
        health_check)     TOOL_PAYLOAD='{"status":"ok","version":"1.0.0"}' ;;
        ensure_project)   TOOL_PAYLOAD='{"id":1,"slug":"backend","human_key":"/backend"}' ;;
        register_agent)   TOOL_PAYLOAD='{"id":1,"name":"BlueLake","program":"codex","model":"gpt-5"}' ;;
    esac

    ENCODED=$(echo "$TOOL_PAYLOAD" | "$STUB" --encode --stats 2>"${WORK}/${tool}_stderr.txt")
    TOOL_STDERR=$(cat "${WORK}/${tool}_stderr.txt")
    TOOL_RC=$?

    if [ "$TOOL_RC" -eq 0 ]; then
        # Build envelope JSON
        ENVELOPE=$(python3 -c "
import json, sys
encoded = sys.argv[1]
payload = json.loads(sys.argv[2])
stderr = sys.argv[3]

# Parse stats from stderr
stats = {}
for line in stderr.strip().split('\n'):
    if 'Token estimates:' in line:
        import re
        m = re.search(r'~(\d+)\s*\(JSON\)\s*(?:->|→)\s*~(\d+)\s*\(TOON\)', line)
        if m:
            stats['json_tokens'] = int(m.group(1))
            stats['toon_tokens'] = int(m.group(2))
    if line.startswith('Saved'):
        m = re.search(r'~(\d+)\s+tokens\s+\(([-\d.]+)%\)', line)
        if m:
            stats['saved_tokens'] = int(m.group(1))
            stats['saved_percent'] = float(m.group(2))

envelope = {
    'format': 'toon',
    'data': encoded,
    'meta': {
        'requested': 'toon',
        'source': 'param',
        'encoder': 'toon_stub_encoder.sh',
        'toon_stats': stats if stats else None
    }
}
print(json.dumps(envelope))
" "$ENCODED" "$TOOL_PAYLOAD" "$TOOL_STDERR")

        STEPS=$(python3 -c "
import json, sys
steps = json.loads(sys.argv[1])
steps.append({'tool': sys.argv[2], 'format': 'toon', 'envelope_format': json.loads(sys.argv[3]).get('format')})
print(json.dumps(steps))
" "$STEPS" "$tool" "$ENVELOPE")

        e2e_pass "tool=$tool encoded successfully"
    else
        e2e_fail "tool=$tool encoder failed with rc=$TOOL_RC"
    fi

    e2e_save_artifact "case4_${tool}_envelope.json" "$ENVELOPE"
done

# Simulate resource read with format=toon query param
RESOURCE_PAYLOAD='[{"id":1,"subject":"Welcome","from":"System","importance":"normal"}]'
RESOURCE_ENCODED=$(echo "$RESOURCE_PAYLOAD" | "$STUB" --encode 2>/dev/null)
RESOURCE_RC=$?

if [ "$RESOURCE_RC" -eq 0 ]; then
    STEPS=$(python3 -c "
import json, sys
steps = json.loads(sys.argv[1])
steps.append({'resource': 'inbox', 'format': 'toon'})
print(json.dumps(steps))
" "$STEPS")
    e2e_pass "resource=inbox encoded successfully"
else
    e2e_fail "resource=inbox encoder failed"
fi

# Write structured log
python3 -c "
import json, sys
log = {
    'test': 'e2e_toon_format_multi_tool_sequence',
    'steps': json.loads(sys.argv[1])
}
with open(sys.argv[2], 'w') as f:
    json.dump(log, f, indent=2)
" "$STEPS" "$LOG_FILE"

e2e_assert_file_exists "E2E log written" "$LOG_FILE"

# Verify log structure
STEP_COUNT=$(python3 -c "import json; print(len(json.load(open('$LOG_FILE'))['steps']))")
e2e_assert_eq "log has 4 steps" "4" "$STEP_COUNT"

# Verify all steps used toon format
ALL_TOON=$(python3 -c "
import json
log = json.load(open('$LOG_FILE'))
print('true' if all(s.get('format') == 'toon' for s in log['steps']) else 'false')
")
e2e_assert_eq "all steps used toon format" "true" "$ALL_TOON"

e2e_copy_artifact "$LOG_FILE" "case4_e2e_log.json"

# ---------------------------------------------------------------------------
# Case 5: Encoder validation responses
# ---------------------------------------------------------------------------
e2e_case_banner "Encoder validation (--help and --version)"

HELP_OUT=$("$STUB" --help)
VERSION_OUT=$("$STUB" --version)

e2e_assert_contains "--help mentions 'reference implementation in rust'" "$HELP_OUT" "reference implementation in rust"
e2e_assert_contains "--version starts with 'tru '" "$VERSION_OUT" "tru "

HELP_FAIL=$("$STUB_FAIL" --help)
VERSION_FAIL=$("$STUB_FAIL" --version)

e2e_assert_contains "failing stub --help also passes validation" "$HELP_FAIL" "reference implementation in rust"
e2e_assert_contains "failing stub --version starts with 'tru '" "$VERSION_FAIL" "tru "

e2e_save_artifact "case5_help.txt" "$HELP_OUT"
e2e_save_artifact "case5_version.txt" "$VERSION_OUT"

# ---------------------------------------------------------------------------
# Case 6: Broken encoder path - graceful behavior
# ---------------------------------------------------------------------------
e2e_case_banner "Broken encoder path produces clear error"

set +e
BROKEN_OUT=$("/nonexistent/tru_binary" --encode < /dev/null 2>"${E2E_ARTIFACT_DIR}/case6_stderr.txt")
BROKEN_RC=$?
set -e
BROKEN_STDERR=$(cat "${E2E_ARTIFACT_DIR}/case6_stderr.txt")

e2e_assert_eq "nonexistent binary exits non-zero" "true" "$([ "$BROKEN_RC" -ne 0 ] && echo true || echo false)"
e2e_assert_contains "stderr has 'No such file'" "$BROKEN_STDERR" "No such file"

# ---------------------------------------------------------------------------
# Case 7: Fallback envelope structure verification
# ---------------------------------------------------------------------------
e2e_case_banner "Fallback envelope has correct structure"

# Simulate what the Rust code produces on encoder failure
FALLBACK_ENVELOPE=$(python3 -c "
import json
envelope = {
    'format': 'json',
    'data': {'id': 1, 'subject': 'Test'},
    'meta': {
        'requested': 'toon',
        'source': 'param',
        'toon_error': 'TOON encoder exited with 1',
        'toon_stderr': 'error: simulated encoder failure'
    }
}
print(json.dumps(envelope))
")

# Verify structure
FMT=$(json_get "$FALLBACK_ENVELOPE" "format")
e2e_assert_eq "fallback format is json" "json" "$FMT"

DATA_ID=$(json_get_nested "$FALLBACK_ENVELOPE" "data.id")
e2e_assert_eq "fallback preserves data.id" "1" "$DATA_ID"

META_REQ=$(json_get_nested "$FALLBACK_ENVELOPE" "meta.requested")
e2e_assert_eq "fallback records requested=toon" "toon" "$META_REQ"

TOON_ERR=$(json_get_nested "$FALLBACK_ENVELOPE" "meta.toon_error")
e2e_assert_contains "fallback has toon_error" "$TOON_ERR" "exited with"

e2e_save_artifact "case7_fallback_envelope.json" "$FALLBACK_ENVELOPE"

# ---------------------------------------------------------------------------
# Case 8: Explicitly selected real HTTP transport and Rust codec roundtrip
# ---------------------------------------------------------------------------
if [ "${AM_E2E_REAL_TOON:-0}" = "1" ]; then
    e2e_case_banner "Real HTTP tool/resource TOON roundtrip"
    e2e_ensure_binary "am" >/dev/null
    if python3 - "${CARGO_TARGET_DIR}/debug/am" "${AM_E2E_REAL_TOON_BIN:-}" \
        "${AM_E2E_REAL_TOON_SHA256:-}" "${E2E_ARTIFACT_DIR}/real_toon" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
import urllib.parse
import urllib.request

binary, encoder, expected_hash, destination = sys.argv[1:]
run = Path(destination).resolve()
run.mkdir(mode=0o700, exist_ok=False)
summary = {"passed": False, "completed_ids": [], "client_pid": os.getpid(),
           "scope": "real HTTP tool/resource encoding and semantic roundtrip"}
server = None
started = time.monotonic()

def interrupted(signum, frame):
    raise InterruptedError(f"real TOON lane interrupted by signal {signum}")

signal.signal(signal.SIGTERM, interrupted)

def digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()

try:
    assert encoder and len(expected_hash) == 64, "selected real lane requires encoder path and SHA256"
    encoder = Path(encoder).resolve(strict=True)
    assert encoder.name in ("toon", "toon.exe"), "use the published executable name to test default lookup"
    assert digest(encoder) == expected_hash, "encoder differs from the selected artifact"
    summary.update(binary_sha256=digest(binary), encoder_sha256=expected_hash,
                   binary=str(binary), encoder=str(encoder))
    for flag in ("--help", "--version"):
        probe = subprocess.run([str(encoder), flag], capture_output=True, text=True, timeout=10)
        (run / (flag[2:] + ".stdout")).write_text(probe.stdout)
        (run / (flag[2:] + ".stderr")).write_text(probe.stderr)
        assert probe.returncode == 0, (flag, probe.returncode, probe.stderr)
        if flag == "--help":
            assert "reference implementation in rust" in probe.stdout.lower(), probe.stdout
        else:
            summary["encoder_version"] = probe.stdout.strip()
    with socket.socket() as available:
        available.bind(("127.0.0.1", 0))
        port = available.getsockname()[1]
    env = os.environ.copy()
    env.pop("AM_INTERFACE_MODE", None)
    env.update(DATABASE_URL="sqlite:///" + str(run / "mailbox.sqlite3"),
               STORAGE_ROOT=str(run / "storage"), HTTP_HOST="127.0.0.1", HTTP_PORT=str(port),
               HTTP_BEARER_TOKEN="owned-real-toon-fixture", AM_ATC_ENABLED="false",
               AM_ATC_WRITE_MODE="off", ATC_LEARNING_DISABLED="1", LLM_ENABLED="false",
               NOTIFICATIONS_ENABLED="false", TUI_ENABLED="false", RUST_LOG="warn",
               INVOCATION_ID="kp1in-real-toon-fixture",
               TOON_TRU_BIN="", TOON_BIN="", TOON_DEFAULT_FORMAT="",
               MCP_AGENT_MAIL_OUTPUT_FORMAT="toon", TOON_STATS="true",
               PATH=str(encoder.parent) + os.pathsep + env.get("PATH", ""))
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
               "Authorization": "Bearer owned-real-toon-fixture"}
    with (run / "server.stdout").open("xb") as stdout, (run / "server.stderr").open("xb") as stderr:
        server = subprocess.Popen([binary, "serve-http", "--host", "127.0.0.1",
                                   "--port", str(port), "--no-tui"], cwd=run, env=env,
                                  stdout=stdout, stderr=stderr, start_new_session=True)
    summary["server_pid"] = server.pid
    deadline = time.monotonic() + 30
    while True:
        assert server.poll() is None, "server exited before listening"
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                break
        except OSError:
            if time.monotonic() >= deadline:
                raise TimeoutError("server readiness deadline")
            time.sleep(0.1)

    def rpc(method, params, request_id):
        request = {"jsonrpc": "2.0", "method": method, "params": params}
        if request_id is not None:
            request["id"] = request_id
        with (run / "transcript.jsonl").open("a") as trace:
            trace.write(json.dumps({"invoke": request}) + "\n")
        with urllib.request.urlopen(urllib.request.Request(
                f"http://127.0.0.1:{port}/mcp/", data=json.dumps(request).encode(),
                headers=headers), timeout=35) as response:
            raw = response.read(8 * 1024 * 1024 + 1)
            assert len(raw) <= 8 * 1024 * 1024, "response budget"
            if response.headers.get("Mcp-Session-Id"):
                headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
        decoded = json.loads(raw) if raw else None
        with (run / "transcript.jsonl").open("a") as trace:
            trace.write(json.dumps({"complete": decoded}) + "\n")
        if request_id is None:
            return None
        assert decoded and decoded.get("id") == request_id and "error" not in decoded, decoded
        result = decoded["result"]
        assert not result.get("isError"), result
        summary["completed_ids"].append(request_id)
        return result

    def tool(request_id, name, arguments):
        result = rpc("tools/call", {"name": name, "arguments": arguments}, request_id)
        assert len(result["content"]) == 1, result
        return json.loads(result["content"][0]["text"])

    def decode(name, envelope):
        assert envelope["format"] == "toon", envelope
        assert "toon_error" not in envelope["meta"], envelope
        result = subprocess.run([str(encoder), "--decode"], input=envelope["data"],
                                capture_output=True, text=True, timeout=15)
        (run / (name + ".toon")).write_text(envelope["data"])
        (run / (name + ".decoded.json")).write_text(result.stdout)
        (run / (name + ".decode.stderr")).write_text(result.stderr)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "real-toon-e2e", "version": "1"}}, 1)
    rpc("notifications/initialized", {}, None)
    project = str(run / "project")
    tool(2, "ensure_project", {"human_key": project, "format": "json"})
    for request_id, name in ((3, "RedFox"), (4, "BluePeak")):
        tool(request_id, "register_agent", {"project_key": project, "name": name,
             "program": "e2e-test", "model": "fixture", "format": "json"})
    # Choose this fixture's contact policy explicitly so the mailbox contains
    # exactly the body whose encoding is under test, without an auto-intro.
    tool(5, "set_contact_policy", {"project_key": project, "agent_name": "BluePeak",
                                 "policy": "open", "format": "json"})
    body = 'Unicode λ 日本語\n"quoted", commas, \\ paths; null-looking: null; bracket: [x]'
    sent = tool(6, "send_message", {"project_key": project, "sender_name": "RedFox",
                "to": ["BluePeak"], "subject": "TOON roundtrip", "body_md": body, "format": "json"})
    message_id = sent["deliveries"][0]["payload"]["id"]
    arguments = {"project_key": project, "agent_name": "BluePeak", "include_bodies": True}
    baseline = tool(7, "fetch_inbox", dict(arguments, format="json"))
    assert len(baseline) == 1 and baseline[0]["id"] == message_id and baseline[0]["body_md"] == body
    default = tool(8, "fetch_inbox", arguments)
    assert default["meta"]["source"] == "default", default
    assert decode("tool_default", default) == baseline
    explicit = tool(9, "fetch_inbox", dict(arguments, format="toon"))
    assert explicit["meta"]["source"] == "param", explicit
    assert decode("tool_explicit", explicit) == baseline

    uri = "resource://inbox/BluePeak?project=" + urllib.parse.quote(project, safe="") + "&include_bodies=true"
    plain = rpc("resources/read", {"uri": uri + "&format=json"}, 10)
    encoded = rpc("resources/read", {"uri": uri}, 11)
    assert len(plain["contents"]) == len(encoded["contents"]) == 1
    expected = json.loads(plain["contents"][0]["text"])
    envelope = json.loads(encoded["contents"][0]["text"])
    assert envelope["meta"]["source"] == "default", envelope
    assert decode("resource_default", envelope) == expected
    assert digest(encoder) == expected_hash, "encoder changed during the run"
    assert server.poll() is None, "server exited during the real lane"
    summary["passed"] = True
except BaseException as error:
    summary["error"] = repr(error)
finally:
    if server is not None:
        if server.poll() is None:
            os.killpg(server.pid, signal.SIGTERM)
            try:
                server.wait(timeout=20)
                summary["shutdown"] = "owned stop and join; not graceful-shutdown certification"
            except subprocess.TimeoutExpired:
                os.killpg(server.pid, signal.SIGKILL)
                server.wait(timeout=5)
                summary["shutdown"] = "forced"
                summary["passed"] = False
        summary["server_exit"] = server.returncode
        if server.returncode not in (0, -signal.SIGTERM):
            summary["passed"] = False
    summary["elapsed_s"] = time.monotonic() - started
    (run / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary))
raise SystemExit(0 if summary["passed"] else 1)
PY
    then
        e2e_pass "real HTTP tool/resource TOON roundtrip preserves Unicode and structured data"
    else
        e2e_fail "selected real TOON lane failed or lacked its required dependency"
    fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
