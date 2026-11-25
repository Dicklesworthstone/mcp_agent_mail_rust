#!/usr/bin/env bash
# Self-test for tests/e2e/lib helpers.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=structured_logging.sh
source "${SCRIPT_DIR}/structured_logging.sh"
# shellcheck source=spawn_server.sh
source "${SCRIPT_DIR}/spawn_server.sh"
# shellcheck source=seed_corpus.sh
source "${SCRIPT_DIR}/seed_corpus.sh"
# shellcheck source=wait_for.sh
source "${SCRIPT_DIR}/wait_for.sh"

PASS=0
FAIL=0
TOTAL=0
TMP_ROOT="${TMPDIR:-/data/tmp}"
if [ ! -d "${TMP_ROOT}" ]; then
    TMP_ROOT="/tmp"
fi
WORK="${TMP_ROOT%/}/am-e2e-lib-self-test.$(date -u +%Y%m%dT%H%M%SZ).$$"
mkdir -p "${WORK}"

record_pass() {
    TOTAL=$((TOTAL + 1))
    PASS=$((PASS + 1))
    log_pass "lib_self_test" "$1" "$2"
}

record_fail() {
    TOTAL=$((TOTAL + 1))
    FAIL=$((FAIL + 1))
    log_fail "lib_self_test" "$1" "$2"
}

assert_cmd() {
    local scenario="$1"
    local message="$2"
    shift 2
    if "$@"; then
        record_pass "${scenario}" "${message}"
    else
        record_fail "${scenario}" "${message}"
    fi
}

json_field_equals() {
    local json_line="$1"
    local path="$2"
    local expected="$3"
    python3 - "${json_line}" "${path}" "${expected}" <<'PY'
import json
import sys

doc = json.loads(sys.argv[1])
value = doc
for part in sys.argv[2].split("."):
    value = value[part]
if str(value) != sys.argv[3]:
    raise SystemExit(1)
PY
}

has_no_literal_newline() {
    [ "$(printf '%s' "$1" | wc -l)" = 0 ]
}

line="$(log_event info lib_self_test s1 "hello" '{"n":1}')"
assert_cmd "s1_log_event_emits_valid_json" "log_event emits parseable JSON" \
    json_field_equals "${line}" "data.n" "1"

special_message=$'quote " newline\nslash \\'
line="$(log_event info lib_self_test s2 "${special_message}" '{"ok":true}')"
assert_cmd "s2_log_event_escapes_special_chars" "log_event escapes special characters" \
    json_field_equals "${line}" "message" "${special_message}"

pretty_data=$'{\n  "nested": {\n    "ok": true\n  }\n}'
line="$(log_event info lib_self_test s2b "pretty data" "${pretty_data}")"
assert_cmd "s2b_log_event_keeps_one_json_line" "log_event compacts pretty data JSON" \
    has_no_literal_newline "${line}"
assert_cmd "s2b_log_event_keeps_one_json_line" "compacted data remains parseable" \
    json_field_equals "${line}" "data.nested.ok" "True"

summary_line="$(log_summary lib_self_test 3 2 1 "${WORK}" "bash tests/e2e/lib/lib_self_test.sh")"
assert_cmd "s3_log_summary_emits_summary_envelope" "log_summary emits summary envelope" \
    json_field_equals "${summary_line}" "summary.failed" "1"

SERVER_DIR="${WORK}/server"
STDOUT_LOG="${SERVER_DIR}/stdout.log"
STDERR_LOG="${SERVER_DIR}/stderr.log"
PID="$(spawn_server_with_storage \
    dummy \
    "${SERVER_DIR}" \
    "${SERVER_DIR}/storage" \
    "${SERVER_DIR}/db.sqlite3" \
    "${STDOUT_LOG}" \
    "${STDERR_LOG}" \
    -- \
    python3 -c 'import os,time; from pathlib import Path; Path(os.environ["STORAGE_ROOT"]).mkdir(parents=True, exist_ok=True); Path(os.environ["STORAGE_ROOT"], "marker.txt").write_text(os.environ["DATABASE_URL"], encoding="utf-8"); time.sleep(30)')"
sleep 0.2
assert_cmd "s4_spawn_server_isolates_storage_root" "spawned process is running" \
    kill -0 "${PID}"
assert_cmd "s4_spawn_server_isolates_storage_root" "storage root marker was written" \
    test -f "${SERVER_DIR}/storage/marker.txt"
stop_spawned_server "${PID}"

CORPUS_DIR="${WORK}/corpus"
seed_mixed_quality_corpus "${CORPUS_DIR}" 2 2
assert_cmd "s5_seed_corpus_writes_expected_files" "manifest exists" \
    test -f "${CORPUS_DIR}/manifest.jsonl"
assert_cmd "s5_seed_corpus_writes_expected_files" "manifest has four documents" \
    bash -c "[ \"$(wc -l <"${CORPUS_DIR}/manifest.jsonl")\" = 4 ]"

assert_cmd "s6_wait_for_with_satisfied_condition_returns_quickly" "satisfied condition returns success" \
    wait_for_condition 2 0.1 -- test -f "${CORPUS_DIR}/manifest.jsonl"

set +e
wait_for_condition 1 0.1 -- test -f "${WORK}/never-created"
timeout_rc=$?
set -e
if [ "${timeout_rc}" -ne 0 ]; then
    record_pass "s7_wait_for_timeout_returns_nonzero" "timeout condition returned non-zero"
else
    record_fail "s7_wait_for_timeout_returns_nonzero" "timeout condition unexpectedly succeeded"
fi

assert_cmd "s8_exit_code_evidence" "allowed exit sets preserve actual codes and reject malformed inputs" \
    python3 - "${SCRIPT_DIR}/../../../scripts/e2e_lib.sh" "${WORK}" <<'PY'
import json
import os
import pathlib
import subprocess
import sys

harness = pathlib.Path(sys.argv[1]).resolve()
work = pathlib.Path(sys.argv[2])
cases = [
    ("0", ["0", "1"], True),
    ("1", ["0", "1"], True),
    ("2", ["0", "1"], False),
    ("139", ["0", "1"], False),
    ("1", [], False),
    ("1", ["1", "oops"], False),
    ("1", [""], False),
    ("1", ["256"], False),
    ("01", ["0", "1"], False),
]
for index, (actual, allowed, passes) in enumerate(cases):
    trace = work / f"exit-code-{index}.jsonl"
    result = subprocess.run(
        ["bash", "-c", '''
source "$1"
_E2E_TRACE_FILE="$2"
actual="$3"
shift 3
e2e_assert_exit_code_in "observed command" "$actual" "$@"
printf 'COUNTERS=%s,%s\n' "$_E2E_PASS" "$_E2E_FAIL"
e2e_pass "ordinary assertion has no exit fields"
''', "exit-code-self-test", str(harness), str(trace), actual, *allowed],
        env={**os.environ, "AM_E2E_KEEP_TMP": "1"},
        capture_output=True, text=True, check=True,
    )
    events = [json.loads(line) for line in trace.read_text().splitlines()]
    assertion = next(e for e in events if e["kind"] in ("assert_pass", "assert_fail"))
    assert assertion["actual_exit_code"] == actual, assertion
    assert assertion["expected_exit_codes"] == allowed, assertion
    assert assertion["kind"] == ("assert_pass" if passes else "assert_fail"), assertion
    assert f"actual exit={actual}" in assertion["message"], assertion
    expected_counters = "1,0" if passes else "0,1"
    assert f"COUNTERS={expected_counters}" in result.stdout, result.stdout
    ordinary = events[-1]
    assert "actual_exit_code" not in ordinary and "expected_exit_codes" not in ordinary, ordinary
print("9 allowed-exit cases passed; actual=1 is preserved, signals and malformed sets fail")
PY

assert_cmd "s9_stdio_initialize_metadata" "stdio rejects malformed initialization before notifying the peer" \
    python3 - "${SCRIPT_DIR}/../../../scripts/e2e_lib.sh" "${WORK}" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys

harness = Path(sys.argv[1]).resolve()
work = Path(sys.argv[2])
peer = work / "initialize-peer.py"
with peer.open("x", encoding="utf-8") as script:
    script.write('''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

for line in sys.stdin:
    request = json.loads(line)
    with Path(os.environ["STDIO_INIT_SEEN"]).open("a", encoding="utf-8") as seen:
        seen.write(json.dumps(request) + "\\n")
    if "id" not in request:
        continue
    reply = {"jsonrpc": "2.0", "id": request["id"]}
    if request.get("method") != "initialize":
        reply["result"] = {"tools": []}
    elif os.environ["STDIO_INIT_CASE"] == "jsonrpc-error":
        reply["error"] = {"code": -32600, "message": "initialization refused"}
    else:
        reply["result"] = json.loads(os.environ["STDIO_INIT_RESULT"])
    print(json.dumps(reply), flush=True)
''')
peer.chmod(0o755)
valid = {
    "protocolVersion": "2024-11-05",
    "capabilities": {},
    "serverInfo": {"name": "initialize-control", "version": "1.0"},
}
cases = [
    ("valid", valid, True),
    ("valid-extension", {**valid, "instructions": "allowed extension"}, True),
    ("null-result", None, False),
    ("missing-protocol", {k: v for k, v in valid.items() if k != "protocolVersion"}, False),
    ("numeric-protocol", {**valid, "protocolVersion": 1}, False),
    ("array-capabilities", {**valid, "capabilities": []}, False),
    ("array-server-info", {**valid, "serverInfo": []}, False),
    ("missing-server-name", {**valid, "serverInfo": {"version": "1.0"}}, False),
    ("numeric-server-version", {**valid, "serverInfo": {"name": "control", "version": 1}}, False),
    ("jsonrpc-error", None, False),
]
initialize = json.dumps({
    "jsonrpc": "2.0", "id": 1, "method": "initialize",
    "params": {"protocolVersion": "2024-11-05", "capabilities": {},
               "clientInfo": {"name": "initialize-self-test", "version": "1.0"}},
})
list_tools = json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
failures = []
for name, result, accepted in cases:
    case = work / ("initialize-" + name)
    case.mkdir()
    seen = case / "seen.jsonl"
    outcome = subprocess.run(
        ["bash", "-c", '''
source "$1"
E2E_ARTIFACT_DIR="$2"
e2e_init_artifacts
e2e_stdio_session "$3" "$4" "$5"
''', "initialize-self-test", str(harness), str(case), str(case / "db.sqlite3"), initialize, list_tools],
        env={**os.environ, "AM_E2E_KEEP_TMP": "1", "E2E_STDIO_BINARY": str(peer),
             "E2E_STDIO_DEFAULT_ENTRYPOINT": "1", "STDIO_INIT_CASE": name,
             "STDIO_INIT_RESULT": json.dumps(result), "STDIO_INIT_SEEN": str(seen)},
        capture_output=True, text=True, timeout=25,
    )
    (case / "stdout.log").write_text(outcome.stdout, encoding="utf-8")
    (case / "stderr.log").write_text(outcome.stderr, encoding="utf-8")
    frames = [json.loads(line) for line in seen.read_text().splitlines()]
    assert frames[0]["method"] == "initialize", frames
    methods = [frame["method"] for frame in frames]
    notified = any(frame.get("method") == "notifications/initialized" for frame in frames)
    expected_methods = ["initialize", "notifications/initialized", "tools/list"] if accepted else ["initialize"]
    ok = (outcome.returncode == (0 if accepted else 1) and methods == expected_methods)
    if not accepted:
        diagnostic = "MCP initialize failed" if name == "jsonrpc-error" else "Invalid MCP initialize result metadata"
        ok = ok and diagnostic in outcome.stderr
    print(f"{name}: exit={outcome.returncode} notified={notified} expected_accept={accepted}")
    if not ok:
        failures.append(name)
if failures:
    raise SystemExit("incorrect initialization acceptance: " + ", ".join(failures))
print("10 initialization controls passed; malformed peers are negative controls only")
PY

assert_cmd "s10_remote_build_routing" "remote requirements reach RCH and reject forced local builds" \
    python3 - "${SCRIPT_DIR}/../../../scripts/e2e_lib.sh" "${WORK}" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys

harness = Path(sys.argv[1]).resolve()
work = Path(sys.argv[2])
binary_dir = work / "routing-bin"
binary_dir.mkdir()
for name in ("cargo", "rch"):
    with (binary_dir / name).open("x", encoding="utf-8") as script:
        script.write('''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
with Path(os.environ["ROUTING_SEEN"]).open("a", encoding="utf-8") as seen:
    seen.write(json.dumps({"program": name, "args": sys.argv[1:],
                           "mock": os.environ.get("RCH_MOCK_CIRCUIT_OPEN")}) + "\\n")
sys.exit(int(os.environ["ROUTING_RCH_EXIT"]) if name == "rch" else 0)
''')
    (binary_dir / name).chmod(0o755)

cases = []
for required in ("E2E_CARGO_REQUIRE_RCH", "RCH_REQUIRE_REMOTE"):
    for timeout in (True, False):
        for mock in (False, True):
            cases.append((f"{required}-timeout-{timeout}-mock-{mock}",
                          {required: "1", "ROUTING_NO_TIMEOUT": "0" if timeout else "1",
                           "E2E_RCH_MOCK_CIRCUIT_OPEN": "1" if mock else "0"},
                          0, "rch", True, mock))
    cases.append((required + "-force-local", {required: "1", "E2E_CARGO_FORCE_LOCAL": "1"},
                  64, None, False, False))
    cases.append((required + "-missing-rch", {required: "1", "ROUTING_NO_RCH": "1"},
                  127, None, False, False))
cases.extend([
    ("optional-rch", {}, 0, "rch", False, False),
    ("optional-force-local", {"E2E_CARGO_FORCE_LOCAL": "1"}, 0, "cargo", False, False),
    ("optional-missing-rch", {"ROUTING_NO_RCH": "1"}, 0, "cargo", False, False),
    ("remote-error-preserved", {"E2E_CARGO_REQUIRE_RCH": "1", "ROUTING_RCH_EXIT": "120"},
     120, "rch", True, False),
])
for required in ("E2E_CARGO_REQUIRE_RCH", "RCH_REQUIRE_REMOTE"):
    for value in ("true", "YES", "On", " enabled "):
        cases.extend([
            (f"{required}-{value}-remote", {required: value}, 0, "rch", True, False),
            (f"{required}-{value}-force-local", {required: value, "E2E_CARGO_FORCE_LOCAL": "1"},
             64, None, False, False),
            (f"{required}-{value}-missing-rch", {required: value, "ROUTING_NO_RCH": "1"},
             127, None, False, False),
        ])
    for value in ("false", "OFF", "disabled"):
        cases.append((f"{required}-{value}-optional", {required: value}, 0, "rch", False, False))
assert len(cases) == 46
failures = []
for index, (name, overrides, expected_exit, program, strict, mock) in enumerate(cases):
    case = work / f"routing-{index}"
    case.mkdir()
    seen = case / "seen.jsonl"
    result = subprocess.run(
        ["bash", "-c", '''
source "$1"
command() {
    if [ "$1" = "-v" ]; then
        if [ "$2" = "rch" ] && [ "$ROUTING_NO_RCH" = "1" ]; then return 1; fi
        if [ "$2" = "timeout" ] && [ "$ROUTING_NO_TIMEOUT" = "1" ]; then return 1; fi
    fi
    builtin command "$@"
}
e2e_run_cargo check --locked
''', "remote-routing-self-test", str(harness)],
        env={**os.environ, "PATH": str(binary_dir) + os.pathsep + os.environ["PATH"],
             "AM_E2E_KEEP_TMP": "1", "E2E_CARGO_REQUIRE_RCH": "0", "RCH_REQUIRE_REMOTE": "0",
             "E2E_CARGO_FORCE_LOCAL": "0", "E2E_RCH_MOCK_CIRCUIT_OPEN": "0",
             "ROUTING_NO_RCH": "0", "ROUTING_NO_TIMEOUT": "0", "ROUTING_RCH_EXIT": "0",
             "ROUTING_SEEN": str(seen), "RCH_MOCK_CIRCUIT_OPEN": "0", **overrides},
        capture_output=True, text=True, timeout=15,
    )
    (case / "stdout.log").write_text(result.stdout, encoding="utf-8")
    (case / "stderr.log").write_text(result.stderr, encoding="utf-8")
    calls = [json.loads(line) for line in seen.read_text().splitlines()] if seen.exists() else []
    expected = []
    if program is not None:
        args = ["check", "--locked"] if program == "cargo" else (
            ["exec"] + (["--require-remote"] if strict else [])
            + ["--", "env", "-u", "CARGO_TARGET_DIR", "cargo", "check", "--locked"]
        )
        expected = [{"program": program, "args": args, "mock": "1" if mock else "0"}]
    ok = result.returncode == expected_exit and calls == expected
    if expected_exit == 64:
        ok = ok and "conflicts with required remote execution" in result.stderr
    if expected_exit == 127:
        ok = ok and "rch is required but not available" in result.stderr
    print(f"{name}: exit={result.returncode} programs={[call['program'] for call in calls]}")
    if not ok:
        failures.append(name)
if failures:
    raise SystemExit("incorrect build routing: " + ", ".join(failures))
print("46 routing controls passed; recording commands never invoke a compiler")
PY

assert_cmd "s11_remote_artifact_guards" "truthy remote requirements reject stale binaries and local helper builds" \
    python3 - "${SCRIPT_DIR}/../../../scripts/e2e_lib.sh" "${WORK}" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys

harness = Path(sys.argv[1]).resolve()
work = Path(sys.argv[2])
failures = []
cases = [(required, value) for required in ("E2E_CARGO_REQUIRE_RCH", "RCH_REQUIRE_REMOTE")
         for value in ("1", "true", "YES", "On", " enabled ")]
assert len(cases) == 10
for index, (required, value) in enumerate(cases):
    case = work / f"artifact-guard-{index}"
    stale = case / "target/debug/am"
    stale.parent.mkdir(parents=True)
    stale.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    stale.chmod(0o700)
    result = subprocess.run(
        ["bash", "-c", '''
source "$1"
E2E_PROJECT_ROOT="$2"
_e2e_build_binary() { printf '%s\\n' "$E2E_CARGO_FORCE_LOCAL" >> build_modes; }
cargo() { printf unexpected-local-build >> forbidden; return 99; }
type() { return 1; }
ensure_status=0
compat_status=0
if e2e_ensure_binary am; then ensure_status=10; fi
if e2e_sqlite3_compat_bin; then compat_status=11; fi
test "$ensure_status" = 0
test "$compat_status" = 0
test ! -e forbidden
test "$(cat build_modes)" = 0
''', "artifact-guard-self-test", str(harness), str(case)],
        cwd=case,
        env={**os.environ, "AM_E2E_KEEP_TMP": "1", "CARGO_TARGET_DIR": str(case / "expected-target"),
             "E2E_CARGO_REQUIRE_RCH": "0", "RCH_REQUIRE_REMOTE": "0",
             "E2E_CARGO_FORCE_LOCAL": "0", "E2E_FORCE_BUILD": "0", required: value},
        capture_output=True, text=True, timeout=15,
    )
    (case / "stdout.log").write_text(result.stdout, encoding="utf-8")
    (case / "stderr.log").write_text(result.stderr, encoding="utf-8")
    ok = result.returncode == 0 and result.stderr.count("remote-required") == 2
    print(f"{required}={value!r}: exit={result.returncode} stale/local attempts refused={ok}")
    if not ok:
        failures.append((required, value))
if failures:
    raise SystemExit("incorrect artifact guards: " + json.dumps(failures))
print("10 artifact controls passed; no compiler or stale executable was run")
PY

assert_cmd "s12_forced_build_preserves_failure" "a failed forced rebuild cannot reuse an existing executable" \
    python3 - "${SCRIPT_DIR}/../../../scripts/e2e_lib.sh" "${WORK}" <<'PY'
import os
from pathlib import Path
import subprocess
import sys

harness = Path(sys.argv[1]).resolve()
work = Path(sys.argv[2])
cases = [(required, value) for required in ("E2E_CARGO_REQUIRE_RCH", "RCH_REQUIRE_REMOTE")
         for value in ("1", "true", "YES", "On", " enabled ")]
failures = []
for index, (required, value) in enumerate(cases):
    case = work / f"forced-build-{index}"
    stale = case / "target/debug/am"
    stale.parent.mkdir(parents=True)
    original = b"#!/bin/sh\nprintf unexpected-execution > executed\n"
    stale.write_bytes(original)
    stale.chmod(0o700)
    for force in ("0", "1"):
        result = subprocess.run(
            ["bash", "-c", '''
source "$1"
if e2e_ensure_binary am; then exit 0; else exit $?; fi
''', "forced-build-self-test", str(harness)],
            cwd=case,
            env={**os.environ, "AM_E2E_KEEP_TMP": "1", "CARGO_TARGET_DIR": str(case / "target"),
                 "E2E_CARGO_REQUIRE_RCH": "0", "RCH_REQUIRE_REMOTE": "0",
                 "E2E_CARGO_FORCE_LOCAL": "1", "E2E_FORCE_BUILD": force, required: value},
            capture_output=True, text=True, timeout=15,
        )
        (case / f"force-{force}.stdout.log").write_text(result.stdout, encoding="utf-8")
        (case / f"force-{force}.stderr.log").write_text(result.stderr, encoding="utf-8")
        expected_exit = 64 if force == "1" else 0
        ok = (result.returncode == expected_exit and stale.read_bytes() == original
              and not (case / "executed").exists())
        if force == "1":
            ok = ok and "conflicts with required remote execution" in result.stdout + result.stderr
        else:
            ok = ok and result.stdout.strip() == str(stale)
        print(f"{required}={value!r}, force={force}: exit={result.returncode}, expected={expected_exit}")
        if not ok:
            failures.append((required, value, force))
if failures:
    raise SystemExit("forced-build status lost: " + repr(failures))
print("10 failed forced rebuilds rejected; 10 prepared-artifact controls accepted; no compiler or stale executable ran")
PY

assert_cmd "s13_build_output_keeps_binary_path_clean" "successful build diagnostics stay off the binary-path stream" \
    python3 - "${SCRIPT_DIR}/../../../scripts/e2e_lib.sh" "${WORK}" <<'PY'
import os
from pathlib import Path
import subprocess
import sys

harness = Path(sys.argv[1]).resolve()
case = Path(sys.argv[2]) / "build-stdout"
binary = case / "target/debug/am"
binary.parent.mkdir(parents=True)
original = b"#!/bin/sh\nprintf unexpected-execution > executed\n"
binary.write_bytes(original)
binary.chmod(0o700)
result = subprocess.run(
    ["bash", "-c", '''
source "$1"
# Exercise the real helper and build-output capture with a recording producer.
# No compiler or application executable runs in this I/O control.
cargo() { printf 'recorded build progress\\n'; }
e2e_ensure_binary am
''', "build-stdout-self-test", str(harness)],
    cwd=case,
    env={**os.environ, "AM_E2E_KEEP_TMP": "1", "CARGO_TARGET_DIR": str(case / "target"),
         "E2E_CARGO_REQUIRE_RCH": "0", "RCH_REQUIRE_REMOTE": "0",
         "E2E_CARGO_FORCE_LOCAL": "1", "E2E_FORCE_BUILD": "1"},
    capture_output=True, text=True, timeout=15,
)
(case / "stdout.log").write_text(result.stdout, encoding="utf-8")
(case / "stderr.log").write_text(result.stderr, encoding="utf-8")
assert result.returncode == 0, (result.returncode, result.stderr)
assert result.stdout.strip() == str(binary), result.stdout
assert "recorded build progress" in result.stderr, result.stderr
assert binary.read_bytes() == original and not (case / "executed").exists()
print("binary path stays on stdout; recorded build diagnostics stay on stderr")
PY

log_summary "lib_self_test" "${TOTAL}" "${PASS}" "${FAIL}" "${WORK}" "bash tests/e2e/lib/lib_self_test.sh"
if [ "${FAIL}" -ne 0 ]; then
    exit 1
fi
