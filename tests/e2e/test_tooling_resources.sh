#!/usr/bin/env bash
# test_tooling_resources.sh - E2E tests for tooling resources
#
# Verifies resource://tooling/* endpoints return fixture-aligned shapes and
# high-signal semantics:
#   1. tooling/directory
#   2. tooling/schemas
#   3. tooling/metrics
#   4. tooling/locks
#   5. tooling/capabilities/{agent}
#   6. tooling/recent/{window}

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
PROJECT_PATH="/tmp/e2e_tooling_$$"
PROJECT_SLUG="tmp-e2e-tooling-$$"
AGENT_NAME="BlueLake"
PY_FIXTURES="${SCRIPT_DIR}/../../crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-tooling","version":"1.0"}}}'

send_jsonrpc_session() {
    local db_path="$1"
    shift
    local requests=("$@")
    local output_file
    output_file="$(mktemp "${WORK}/session_response.XXXXXX.txt")"
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
            sleep 0.25
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

is_error_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == int('$req_id'):
            if 'error' in d:
                print('true')
                sys.exit(0)
            r = d.get('result', {})
            if isinstance(r, dict) and r.get('isError', False):
                print('true')
                sys.exit(0)
            print('false')
            sys.exit(0)
    except Exception:
        pass
print('true')
" 2>/dev/null
}

assert_no_error_result() {
    local label="$1"
    local response="$2"
    local req_id="$3"
    local check
    check="$(is_error_result "$response" "$req_id")"
    if [ "$check" = "false" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label → error/missing response for id=$req_id"
    fi
}

assert_python_ok() {
    local label="$1"
    local check="$2"
    if [[ "$check" == OK* ]]; then
        e2e_pass "$label"
    else
        e2e_fail "$label → $check"
    fi
}

assert_resource_shape_matches_fixture() {
    local label="$1"
    local response="$2"
    local req_id="$3"
    local fixture_uri="$4"
    local check
    check="$(
        RESP="$response" REQ_ID="$req_id" FIXTURE_URI="$fixture_uri" PY_FIXTURES="$PY_FIXTURES" \
            python3 - <<'PY'
import json
import os

resp = os.environ["RESP"]
req_id = int(os.environ["REQ_ID"])
fixture_uri = os.environ["FIXTURE_URI"]
fixture_path = os.environ["PY_FIXTURES"]

def parse_line_payload():
    for raw in resp.splitlines():
        raw = raw.strip()
        if not raw:
            continue
        try:
            d = json.loads(raw)
        except Exception:
            continue
        if d.get("id") != req_id:
            continue
        if "error" in d:
            return None, f"error response for id={req_id}"
        result = d.get("result", {})
        if isinstance(result, dict) and result.get("isError", False):
            return None, f"MCP isError for id={req_id}"
        contents = result.get("contents", result.get("content", []))
        if not contents:
            return None, f"no contents for id={req_id}"
        text = contents[0].get("text", "")
        try:
            payload = json.loads(text)
        except Exception as exc:
            return None, f"payload JSON parse failed: {exc}"
        return payload, None
    return None, f"no matching response for id={req_id}"

def shape(value):
    if isinstance(value, dict):
        keys = sorted(value.keys())
        return {
            "__kind__": "object",
            "keys": keys,
            "children": {k: shape(value[k]) for k in keys},
        }
    if isinstance(value, list):
        return {"__kind__": "array"}
    return "__scalar__"

actual, err = parse_line_payload()
if err:
    print(f"FAIL:{err}")
    raise SystemExit(0)

with open(fixture_path, "r", encoding="utf-8") as fh:
    fixture = json.load(fh)

resource = fixture.get("resources", {}).get(fixture_uri)
if not resource:
    print(f"FAIL:fixture uri not found: {fixture_uri}")
    raise SystemExit(0)

try:
    expected = resource["cases"][0]["expect"]["ok"]
except Exception as exc:
    print(f"FAIL:fixture parse error for {fixture_uri}: {exc}")
    raise SystemExit(0)

actual_shape = shape(actual)
expected_shape = shape(expected)
if actual_shape != expected_shape:
    ak = sorted(actual.keys()) if isinstance(actual, dict) else []
    ek = sorted(expected.keys()) if isinstance(expected, dict) else []
    print(f"FAIL:shape mismatch actual_keys={ak} expected_keys={ek}")
    raise SystemExit(0)

print("OK")
PY
    )"
    assert_python_ok "$label" "$check"
}

# ===========================================================================
# Setup: ensure temp project and sanity call
# ===========================================================================
e2e_case_banner "Setup (ensure_project + health_check)"
e2e_assert_file_exists "python_reference fixture present" "$PY_FIXTURES"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
)"
e2e_save_artifact "setup.txt" "$RESP"
assert_no_error_result "ensure project" "$RESP" 10
assert_no_error_result "health check" "$RESP" 11

# ===========================================================================
# Case 1: tooling/directory
# ===========================================================================
e2e_case_banner "tooling/directory"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":100,"method":"resources/read","params":{"uri":"resource://tooling/directory"}}' \
)"
e2e_save_artifact "case_01_directory.txt" "$RESP"
assert_no_error_result "tooling/directory read" "$RESP" 100
assert_resource_shape_matches_fixture \
    "tooling/directory shape matches fixture" \
    "$RESP" \
    100 \
    "resource://tooling/directory"

DIR_CHECK="$(
    RESP="$RESP" REQ_ID="100" PY_FIXTURES="$PY_FIXTURES" python3 - <<'PY'
import json
import os

resp = os.environ["RESP"]
req_id = int(os.environ["REQ_ID"])
fixture_path = os.environ["PY_FIXTURES"]

payload = None
for raw in resp.splitlines():
    raw = raw.strip()
    if not raw:
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    if d.get("id") != req_id:
        continue
    result = d.get("result", {})
    contents = result.get("contents", result.get("content", []))
    if contents:
        payload = json.loads(contents[0].get("text", "{}"))
    break

if payload is None:
    print("FAIL:no payload")
    raise SystemExit(0)

with open(fixture_path, "r", encoding="utf-8") as fh:
    fixture = json.load(fh)
expected = fixture["resources"]["resource://tooling/directory"]["cases"][0]["expect"]["ok"]
required_clusters = {entry["name"] for entry in expected["clusters"]}

cluster_names = {c.get("name", "") for c in payload.get("clusters", [])}
cluster_count = len(payload.get("clusters", []))
playbook_count = len(payload.get("playbooks", []))
output_formats = payload.get("output_formats", {})
values = output_formats.get("values", [])

if cluster_count < len(required_clusters):
    print(f"FAIL:cluster_count={cluster_count} expected_at_least={len(required_clusters)}")
    raise SystemExit(0)
if not required_clusters.issubset(cluster_names):
    missing = sorted(required_clusters - cluster_names)
    print(f"FAIL:missing cluster names {missing}")
    raise SystemExit(0)
if playbook_count < 5:
    print(f"FAIL:playbook_count={playbook_count} expected>=5")
    raise SystemExit(0)
if output_formats.get("default") != "json":
    print("FAIL:output_formats.default != json")
    raise SystemExit(0)
if "json" not in values or "toon" not in values:
    print(f"FAIL:output_formats.values missing json/toon: {values}")
    raise SystemExit(0)
print(f"OK:clusters={cluster_count}|playbooks={playbook_count}")
PY
)"
assert_python_ok "tooling/directory semantic checks" "$DIR_CHECK"

# ===========================================================================
# Case 2: tooling/schemas
# ===========================================================================
e2e_case_banner "tooling/schemas"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":200,"method":"resources/read","params":{"uri":"resource://tooling/schemas"}}' \
)"
e2e_save_artifact "case_02_schemas.txt" "$RESP"
assert_no_error_result "tooling/schemas read" "$RESP" 200
assert_resource_shape_matches_fixture \
    "tooling/schemas shape matches fixture" \
    "$RESP" \
    200 \
    "resource://tooling/schemas"

SCHEMA_CHECK="$(
    RESP="$RESP" REQ_ID="200" python3 - <<'PY'
import json
import os

resp = os.environ["RESP"]
req_id = int(os.environ["REQ_ID"])
payload = None

for raw in resp.splitlines():
    raw = raw.strip()
    if not raw:
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    if d.get("id") != req_id:
        continue
    result = d.get("result", {})
    contents = result.get("contents", result.get("content", []))
    if contents:
        payload = json.loads(contents[0].get("text", "{}"))
    break

if payload is None:
    print("FAIL:no payload")
    raise SystemExit(0)

tools = payload.get("tools")
if not isinstance(tools, dict):
    print("FAIL:tools is not an object map")
    raise SystemExit(0)

if "send_message" not in tools:
    print("FAIL:missing send_message schema")
    raise SystemExit(0)
if "macro_contact_handshake" not in tools:
    print("FAIL:missing macro_contact_handshake schema")
    raise SystemExit(0)

send = tools["send_message"]
macro = tools["macro_contact_handshake"]

if not {"required", "optional", "shapes"}.issubset(send.keys()):
    print(f"FAIL:send_message schema keys incomplete: {sorted(send.keys())}")
    raise SystemExit(0)
if not {"required", "optional", "aliases"}.issubset(macro.keys()):
    print(f"FAIL:macro_contact_handshake schema keys incomplete: {sorted(macro.keys())}")
    raise SystemExit(0)

if "format" not in payload.get("global_optional", []):
    print("FAIL:global_optional missing format")
    raise SystemExit(0)

output_formats = payload.get("output_formats", {})
values = output_formats.get("values", [])
if output_formats.get("default") != "json" or "toon" not in values:
    print(f"FAIL:unexpected output_formats: {output_formats}")
    raise SystemExit(0)

print("OK")
PY
)"
assert_python_ok "tooling/schemas semantic checks" "$SCHEMA_CHECK"

# ===========================================================================
# Case 3: tooling/metrics (after real tool calls in same session)
# ===========================================================================
e2e_case_banner "tooling/metrics"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":302,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":300,"method":"resources/read","params":{"uri":"resource://tooling/metrics"}}' \
)"
e2e_save_artifact "case_03_metrics.txt" "$RESP"
assert_no_error_result "metrics prep: health_check" "$RESP" 302
assert_no_error_result "tooling/metrics read" "$RESP" 300
assert_resource_shape_matches_fixture \
    "tooling/metrics shape matches fixture" \
    "$RESP" \
    300 \
    "resource://tooling/metrics"

METRICS_CHECK="$(
    RESP="$RESP" REQ_ID="300" python3 - <<'PY'
import json
import os

resp = os.environ["RESP"]
req_id = int(os.environ["REQ_ID"])
payload = None

for raw in resp.splitlines():
    raw = raw.strip()
    if not raw:
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    if d.get("id") != req_id:
        continue
    result = d.get("result", {})
    contents = result.get("contents", result.get("content", []))
    if contents:
        payload = json.loads(contents[0].get("text", "{}"))
    break

if payload is None:
    print("FAIL:no payload")
    raise SystemExit(0)

tools = payload.get("tools", [])
if not isinstance(tools, list) or not tools:
    print("FAIL:tools array empty")
    raise SystemExit(0)

required_tool_keys = {"name", "calls", "errors", "cluster", "capabilities", "complexity"}
for idx, tool in enumerate(tools):
    if not isinstance(tool, dict):
        print(f"FAIL:tool entry {idx} is not object")
        raise SystemExit(0)
    if set(tool.keys()) != required_tool_keys:
        print(f"FAIL:tool entry keys mismatch at idx={idx}: {sorted(tool.keys())}")
        raise SystemExit(0)

if max(int(t.get("calls", 0)) for t in tools) <= 0:
    print("FAIL:expected at least one tool calls>0 after prep calls")
    raise SystemExit(0)

health_level = payload.get("health_level")
if not isinstance(health_level, str) or not health_level:
    print("FAIL:health_level missing/invalid")
    raise SystemExit(0)

print("OK")
PY
)"
assert_python_ok "tooling/metrics semantic checks" "$METRICS_CHECK"

# ===========================================================================
# Case 4: tooling/locks
# ===========================================================================
e2e_case_banner "tooling/locks"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    '{"jsonrpc":"2.0","id":400,"method":"resources/read","params":{"uri":"resource://tooling/locks"}}' \
)"
e2e_save_artifact "case_04_locks.txt" "$RESP"
assert_no_error_result "tooling/locks read" "$RESP" 400
assert_resource_shape_matches_fixture \
    "tooling/locks shape matches fixture" \
    "$RESP" \
    400 \
    "resource://tooling/locks"

LOCKS_CHECK="$(
    RESP="$RESP" REQ_ID="400" python3 - <<'PY'
import json
import os

resp = os.environ["RESP"]
req_id = int(os.environ["REQ_ID"])
payload = None

for raw in resp.splitlines():
    raw = raw.strip()
    if not raw:
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    if d.get("id") != req_id:
        continue
    result = d.get("result", {})
    contents = result.get("contents", result.get("content", []))
    if contents:
        payload = json.loads(contents[0].get("text", "{}"))
    break

if payload is None:
    print("FAIL:no payload")
    raise SystemExit(0)

locks = payload.get("locks")
summary = payload.get("summary")
if not isinstance(locks, list):
    print("FAIL:locks is not array")
    raise SystemExit(0)
if not isinstance(summary, dict):
    print("FAIL:summary is not object")
    raise SystemExit(0)

expected_summary_keys = {"active", "metadata_missing", "stale", "total"}
if set(summary.keys()) != expected_summary_keys:
    print(f"FAIL:summary keys mismatch: {sorted(summary.keys())}")
    raise SystemExit(0)

total = int(summary.get("total", 0))
active = int(summary.get("active", 0))
if total < active:
    print(f"FAIL:summary total<{active}: total={total}")
    raise SystemExit(0)

print("OK")
PY
)"
assert_python_ok "tooling/locks semantic checks" "$LOCKS_CHECK"

# ===========================================================================
# Case 5: tooling/capabilities/{agent}
# ===========================================================================
e2e_case_banner "tooling/capabilities/{agent}"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":500,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://tooling/capabilities/${AGENT_NAME}?project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_05_capabilities.txt" "$RESP"
assert_no_error_result "tooling/capabilities read" "$RESP" 500
assert_resource_shape_matches_fixture \
    "tooling/capabilities shape matches fixture" \
    "$RESP" \
    500 \
    "resource://tooling/capabilities/BlueLake?project=abs-path-backend"

CAPS_CHECK="$(
    RESP="$RESP" REQ_ID="500" EXPECT_AGENT="$AGENT_NAME" EXPECT_PROJECT="$PROJECT_SLUG" python3 - <<'PY'
import json
import os

resp = os.environ["RESP"]
req_id = int(os.environ["REQ_ID"])
expect_agent = os.environ["EXPECT_AGENT"]
expect_project = os.environ["EXPECT_PROJECT"]
payload = None

for raw in resp.splitlines():
    raw = raw.strip()
    if not raw:
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    if d.get("id") != req_id:
        continue
    result = d.get("result", {})
    contents = result.get("contents", result.get("content", []))
    if contents:
        payload = json.loads(contents[0].get("text", "{}"))
    break

if payload is None:
    print("FAIL:no payload")
    raise SystemExit(0)

if payload.get("agent") != expect_agent:
    print(f"FAIL:agent mismatch actual={payload.get('agent')} expected={expect_agent}")
    raise SystemExit(0)
if payload.get("project") != expect_project:
    print(f"FAIL:project mismatch actual={payload.get('project')} expected={expect_project}")
    raise SystemExit(0)
if not isinstance(payload.get("capabilities"), list):
    print("FAIL:capabilities is not array")
    raise SystemExit(0)

print("OK")
PY
)"
assert_python_ok "tooling/capabilities semantic checks" "$CAPS_CHECK"

# ===========================================================================
# Case 6: tooling/recent/{window} (1h/6h/24h)
# ===========================================================================
e2e_case_banner "tooling/recent/{window}"

RESP="$(send_jsonrpc_session "$TR_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":600,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://tooling/recent/3600?agent=${AGENT_NAME}&project=${PROJECT_SLUG}\"}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":601,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://tooling/recent/21600?agent=${AGENT_NAME}&project=${PROJECT_SLUG}\"}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":602,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://tooling/recent/86400?agent=${AGENT_NAME}&project=${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_06_recent_windows.txt" "$RESP"
assert_no_error_result "tooling/recent 1h read" "$RESP" 600
assert_no_error_result "tooling/recent 6h read" "$RESP" 601
assert_no_error_result "tooling/recent 24h read" "$RESP" 602

assert_resource_shape_matches_fixture \
    "tooling/recent 1h shape matches fixture" \
    "$RESP" \
    600 \
    "resource://tooling/recent/60?agent=BlueLake&project=abs-path-backend"
assert_resource_shape_matches_fixture \
    "tooling/recent 6h shape matches fixture" \
    "$RESP" \
    601 \
    "resource://tooling/recent/60?agent=BlueLake&project=abs-path-backend"
assert_resource_shape_matches_fixture \
    "tooling/recent 24h shape matches fixture" \
    "$RESP" \
    602 \
    "resource://tooling/recent/60?agent=BlueLake&project=abs-path-backend"

RECENT_CHECK="$(
    RESP="$RESP" EXPECT_AGENT="$AGENT_NAME" EXPECT_PROJECT="$PROJECT_SLUG" python3 - <<'PY'
import json
import os

resp = os.environ["RESP"]
expect_agent = os.environ["EXPECT_AGENT"]
expect_project = os.environ["EXPECT_PROJECT"]
expected_windows = {600: 3600, 601: 21600, 602: 86400}
required_entry_keys = {"agent", "cluster", "project", "timestamp", "tool"}

payloads = {}
for raw in resp.splitlines():
    raw = raw.strip()
    if not raw:
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    msg_id = d.get("id")
    if msg_id not in expected_windows:
        continue
    result = d.get("result", {})
    contents = result.get("contents", result.get("content", []))
    if not contents:
        continue
    payloads[msg_id] = json.loads(contents[0].get("text", "{}"))

for msg_id, window_seconds in expected_windows.items():
    payload = payloads.get(msg_id)
    if payload is None:
        print(f"FAIL:missing payload for id={msg_id}")
        raise SystemExit(0)
    if int(payload.get("window_seconds", -1)) != window_seconds:
        print(
            f"FAIL:window mismatch id={msg_id} "
            f"actual={payload.get('window_seconds')} expected={window_seconds}"
        )
        raise SystemExit(0)
    entries = payload.get("entries")
    if not isinstance(entries, list):
        print(f"FAIL:entries not list for id={msg_id}")
        raise SystemExit(0)
    if int(payload.get("count", -1)) != len(entries):
        print(f"FAIL:count != len(entries) for id={msg_id}")
        raise SystemExit(0)
    for idx, entry in enumerate(entries):
        if not isinstance(entry, dict):
            print(f"FAIL:entry {idx} not object for id={msg_id}")
            raise SystemExit(0)
        if set(entry.keys()) != required_entry_keys:
            print(f"FAIL:entry keys mismatch for id={msg_id}, idx={idx}")
            raise SystemExit(0)
        if entry.get("agent") != expect_agent:
            print(
                f"FAIL:entry agent mismatch for id={msg_id}, idx={idx}, "
                f"actual={entry.get('agent')}, expected={expect_agent}"
            )
            raise SystemExit(0)
        if entry.get("project") != expect_project:
            print(
                f"FAIL:entry project mismatch for id={msg_id}, idx={idx}, "
                f"actual={entry.get('project')}, expected={expect_project}"
            )
            raise SystemExit(0)

c1 = int(payloads[600].get("count", 0))
c6 = int(payloads[601].get("count", 0))
c24 = int(payloads[602].get("count", 0))
if not (c1 <= c6 <= c24):
    print(f"FAIL:recency monotonicity failed counts={c1},{c6},{c24}")
    raise SystemExit(0)

print(f"OK:counts={c1},{c6},{c24}")
PY
)"
assert_python_ok "tooling/recent window checks" "$RECENT_CHECK"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
