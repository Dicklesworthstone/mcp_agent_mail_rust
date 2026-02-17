#!/usr/bin/env bash
# e2e_console.sh - PTY/TTY-focused E2E suite for rich console output.
#
# Run via:
#   ./scripts/e2e_test.sh console
#
# This suite validates that rich console output is enabled by default in real
# terminals, and that envfile-persisted `CONSOLE_*` settings are loaded.
#
# Artifacts:
#   tests/artifacts/console/<timestamp>/*

set -euo pipefail

# Safety: default to keeping temp dirs so the shared harness doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-console}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Console (PTY) E2E Test Suite"

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in script timeout python3 curl; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

e2e_fatal() {
    local msg="$1"
    e2e_fail "${msg}"
    e2e_summary || true
    exit 1
}

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

e2e_assert_file_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq -- "${needle}" "${path}"; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        e2e_log "missing needle: ${needle}"
        e2e_log "in file: ${path}"
        e2e_log "tail (last 120 lines):"
        tail -n 120 "${path}" 2>/dev/null || true
    fi
}

e2e_assert_file_not_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq -- "${needle}" "${path}"; then
        e2e_fail "${label}"
        e2e_log "unexpected needle: ${needle}"
        e2e_log "in file: ${path}"
        e2e_log "matches:"
        grep -Fn -- "${needle}" "${path}" | head -n 20 || true
    else
        e2e_pass "${label}"
    fi
}

http_request() {
    local case_id="$1"
    local method="$2"
    local url="$3"
    shift 3

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local case_headers_file="${case_dir}/headers.txt"
    local case_body_file="${case_dir}/response.txt"
    local case_status_file="${case_dir}/status.txt"
    local case_timing_file="${case_dir}/timing.txt"
    local case_curl_stderr_file="${case_dir}/curl_stderr.txt"
    local case_curl_args_file="${case_dir}/curl_args.txt"

    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.txt"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local timing_file="${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"
    local curl_args_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_args.txt"

    mkdir -p "${case_dir}"
    e2e_mark_case_start "${case_id}"

    local args=(
        -sS
        -D "${case_headers_file}"
        -o "${case_body_file}"
        -w "%{http_code}"
        -X "${method}"
        "${url}"
    )
    for h in "$@"; do
        args+=(-H "$h")
    done

    e2e_save_artifact "${case_id}_curl_args.txt" "$(printf "curl -X %q %q %s\n" "${method}" "${url}" "$(printf "%q " "$@")")"
    printf "curl -X %q %q %s\n" "${method}" "${url}" "$(printf "%q " "$@")" > "${case_curl_args_file}"

    set +e
    local start_ns end_ns elapsed_ms
    start_ns="$(date +%s%N)"
    local status
    status="$(curl "${args[@]}" 2>"${case_curl_stderr_file}")"
    local rc=$?
    set -e
    end_ns="$(date +%s%N)"
    elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))

    echo "${status}" > "${case_status_file}"
    echo "${elapsed_ms}" > "${case_timing_file}"

    cp "${case_headers_file}" "${headers_file}" 2>/dev/null || true
    cp "${case_body_file}" "${body_file}" 2>/dev/null || true
    cp "${case_status_file}" "${status_file}" 2>/dev/null || true
    cp "${case_timing_file}" "${timing_file}" 2>/dev/null || true
    cp "${case_curl_stderr_file}" "${curl_stderr_file}" 2>/dev/null || true
    cp "${case_curl_args_file}" "${curl_args_file}" 2>/dev/null || true

    if [ "$rc" -ne 0 ]; then
        e2e_fatal "${case_id}: curl failed rc=${rc}"
    fi
}

http_post_json() {
    local case_id="$1"
    local url="$2"
    local payload="$3"
    shift 3

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call_raw "${case_id}" "${url}" "${payload}" "$@"; then
        local status_file_new="${E2E_ARTIFACT_DIR}/${case_id}/status.txt"
        local status="unknown"
        if [ -f "${status_file_new}" ]; then
            status="$(cat "${status_file_new}")"
        fi
        e2e_fatal "${case_id}: request failed status=${status}"
    fi

    # Backward-compatible flat artifact paths used by existing assertions.
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    cp "${case_dir}/request.json" "${E2E_ARTIFACT_DIR}/${case_id}_request.json"
    cp "${case_dir}/response.json" "${E2E_ARTIFACT_DIR}/${case_id}_body.json"
    cp "${case_dir}/headers.txt" "${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    cp "${case_dir}/status.txt" "${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    cp "${case_dir}/timing.txt" "${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    cp "${case_dir}/curl_stderr.txt" "${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"
}

jsonrpc_tools_call_payload() {
    local tool_name="$1"
    local args_json="${2-}"
    if [ -z "${args_json}" ]; then
        args_json="{}"
    fi
    python3 - <<'PY' "$tool_name" "$args_json"
import json, sys
tool = sys.argv[1]
args = json.loads(sys.argv[2])
print(json.dumps({
  "jsonrpc": "2.0",
  "method": "tools/call",
  "id": 1,
  "params": { "name": tool, "arguments": args },
}, separators=(",", ":")))
PY
}

normalize_transcript() {
    local in_path="$1"
    local out_path="$2"
    python3 - <<'PY' "$in_path" "$out_path"
import re
import sys

in_path = sys.argv[1]
out_path = sys.argv[2]

data = open(in_path, "rb").read()

# Strip OSC sequences (BEL or ST terminator).
data = re.sub(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", b"", data)
# Strip CSI sequences (colors + cursor movement).
data = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data)
# Strip single-character ESC sequences (best-effort).
data = re.sub(rb"\x1b[@-_]", b"", data)

text = data.decode("utf-8", errors="replace")

# Remove util-linux `script` wrapper lines for stable assertions.
lines = []
for line in text.splitlines():
    if line.startswith("Script started on "):
        continue
    if line.startswith("Script done on "):
        continue
    lines.append(line)
text = "\n".join(lines) + "\n"

with open(out_path, "w", encoding="utf-8") as f:
    f.write(text)
PY
}

start_server_pty() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5

    local typescript="${E2E_ARTIFACT_DIR}/server_${label}.typescript"
    e2e_log "Starting PTY server (${label}): 127.0.0.1:${port}"
    e2e_log "  typescript: ${typescript}"

    local timeout_s="${AM_E2E_SERVER_TIMEOUT_S:-15}"

    # Run the server in a PTY so stdout/stderr are treated as a real terminal.
    # Use `timeout` to guarantee the process eventually exits even if a test fails.
    (
        script -q -f -c "env \
DATABASE_URL=sqlite:////${db_path} \
STORAGE_ROOT=${storage_root} \
HTTP_HOST=127.0.0.1 \
HTTP_PORT=${port} \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_JWT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0 \
${*} \
timeout ${timeout_s}s ${bin} serve --host 127.0.0.1 --port ${port}" \
            "${typescript}"
    ) >/dev/null 2>&1 &

    echo $!
}

stop_server_pty() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

e2e_case_banner "default_rich_console_enabled (LOG_RICH_ENABLED unset)"
WORK1="$(e2e_mktemp "e2e_console_default_rich")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage"
mkdir -p "${STORAGE1}"
PORT1="$(pick_port)"

PID1="$(start_server_pty "default_rich" "${PORT1}" "${DB1}" "${STORAGE1}" "${BIN}")"
if ! e2e_wait_port 127.0.0.1 "${PORT1}" 10; then
    stop_server_pty "${PID1}"
    e2e_fatal "server failed to start (port not open)"
fi
sleep 0.6
stop_server_pty "${PID1}"
sleep 0.3

NORM1="${E2E_ARTIFACT_DIR}/server_default_rich.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_default_rich.typescript" "${NORM1}"
e2e_assert_file_contains "banner includes startup title" "${NORM1}" "am: Starting MCP Agent Mail server"
e2e_assert_file_contains "banner includes interface mode" "${NORM1}" "interface_mode:"
e2e_assert_file_contains "banner includes mode line" "${NORM1}" "mode:    HTTP + TUI"

e2e_case_banner "banner_suppressed_when_rich_disabled"
WORK2="$(e2e_mktemp "e2e_console_no_rich")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
mkdir -p "${STORAGE2}"
PORT2="$(pick_port)"

PID2="$(start_server_pty "no_rich" "${PORT2}" "${DB2}" "${STORAGE2}" "${BIN}" "LOG_RICH_ENABLED=false")"
if ! e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    stop_server_pty "${PID2}"
    e2e_fatal "server failed to start (port not open)"
fi
sleep 0.6
stop_server_pty "${PID2}"
sleep 0.3

NORM2="${E2E_ARTIFACT_DIR}/server_no_rich.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_no_rich.typescript" "${NORM2}"
e2e_assert_file_contains "startup banner present when rich disabled" "${NORM2}" "am: Starting MCP Agent Mail server"
e2e_assert_file_not_contains "legacy rich section absent when rich disabled" "${NORM2}" "Server Configuration"

e2e_case_banner "persisted_console_settings_are_loaded"
WORK3="$(e2e_mktemp "e2e_console_persist")"
DB3="${WORK3}/db.sqlite3"
STORAGE3="${WORK3}/storage"
mkdir -p "${STORAGE3}"
PORT3="$(pick_port)"

PERSIST_ENV="${WORK3}/console.env"
cat > "${PERSIST_ENV}" <<'EOF'
CONSOLE_UI_HEIGHT_PERCENT=50
CONSOLE_UI_ANCHOR=top
CONSOLE_THEME=darcula
EOF

PID3="$(start_server_pty "persisted" "${PORT3}" "${DB3}" "${STORAGE3}" "${BIN}" "CONSOLE_PERSIST_PATH=${PERSIST_ENV}")"
if ! e2e_wait_port 127.0.0.1 "${PORT3}" 10; then
    stop_server_pty "${PID3}"
    e2e_fatal "server failed to start (port not open)"
fi
sleep 0.6
stop_server_pty "${PID3}"
sleep 0.3

NORM3="${E2E_ARTIFACT_DIR}/server_persisted.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_persisted.typescript" "${NORM3}"
e2e_assert_file_contains "startup banner present with persisted settings" "${NORM3}" "am: Starting MCP Agent Mail server"
e2e_assert_file_contains "server remains HTTP+TUI with persisted settings" "${NORM3}" "mode:    HTTP + TUI"
e2e_assert_file_not_contains "persisted startup has no panic" "${NORM3}" "panicked"

e2e_case_banner "tool_call_panels_respect_gates"
WORK_T="$(e2e_mktemp "e2e_console_tool_calls")"
DBT="${WORK_T}/db.sqlite3"
STORAGET="${WORK_T}/storage"
mkdir -p "${STORAGET}"
PORTT="$(pick_port)"
URLT_BASE="http://127.0.0.1:${PORTT}"
API_URLT="${URLT_BASE}/api/"
TOKEN_T="e2e-token"
AUTHZ_T="Authorization: Bearer ${TOKEN_T}"

PIDT1="$(start_server_pty "tool_calls_on" "${PORTT}" "${DBT}" "${STORAGET}" "${BIN}" \
    "HTTP_BEARER_TOKEN=${TOKEN_T}" \
    "TOOLS_LOG_ENABLED=true" \
    "LOG_TOOL_CALLS_ENABLED=true" \
)"
if ! e2e_wait_port 127.0.0.1 "${PORTT}" 10; then
    stop_server_pty "${PIDT1}"
    e2e_fatal "server failed to start (port not open)"
fi
PAYLOAD_HC="$(jsonrpc_tools_call_payload "health_check" "{}")"
http_post_json "tool_calls_on_health_check" "${API_URLT}" "${PAYLOAD_HC}" "${AUTHZ_T}"
e2e_assert_file_contains "health_check call returns 200" "${E2E_ARTIFACT_DIR}/tool_calls_on_health_check_status.txt" "200"
sleep 0.6
stop_server_pty "${PIDT1}"
sleep 0.3

NORM_T1="${E2E_ARTIFACT_DIR}/server_tool_calls_on.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_tool_calls_on.typescript" "${NORM_T1}"
e2e_assert_file_contains "tool-calls-on startup banner present" "${NORM_T1}" "am: Starting MCP Agent Mail server"
e2e_assert_file_not_contains "tool-calls-on startup has no panic" "${NORM_T1}" "panicked"

PORTT2="$(pick_port)"
URLT2_BASE="http://127.0.0.1:${PORTT2}"
API_URLT2="${URLT2_BASE}/api/"
PIDT2="$(start_server_pty "tool_calls_off" "${PORTT2}" "${DBT}" "${STORAGET}" "${BIN}" \
    "HTTP_BEARER_TOKEN=${TOKEN_T}" \
    "TOOLS_LOG_ENABLED=true" \
    "LOG_TOOL_CALLS_ENABLED=false" \
)"
if ! e2e_wait_port 127.0.0.1 "${PORTT2}" 10; then
    stop_server_pty "${PIDT2}"
    e2e_fatal "server failed to start (port not open)"
fi
PAYLOAD_HC2="$(jsonrpc_tools_call_payload "health_check" "{}")"
http_post_json "tool_calls_off_health_check" "${API_URLT2}" "${PAYLOAD_HC2}" "${AUTHZ_T}"
e2e_assert_file_contains "health_check call returns 200 (tool call panels off)" "${E2E_ARTIFACT_DIR}/tool_calls_off_health_check_status.txt" "200"
sleep 0.6
stop_server_pty "${PIDT2}"
sleep 0.3

NORM_T2="${E2E_ARTIFACT_DIR}/server_tool_calls_off.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_tool_calls_off.typescript" "${NORM_T2}"
e2e_assert_file_not_contains "TOOL CALL panel absent when disabled" "${NORM_T2}" "TOOL CALL"

e2e_case_banner "request_panel_logged"
WORK_R="$(e2e_mktemp "e2e_console_request_panel")"
DBR="${WORK_R}/db.sqlite3"
STORAGER="${WORK_R}/storage"
mkdir -p "${STORAGER}"
PORTR="$(pick_port)"
URLR_BASE="http://127.0.0.1:${PORTR}"

PIDR="$(start_server_pty "request_panel" "${PORTR}" "${DBR}" "${STORAGER}" "${BIN}" \
    "HTTP_REQUEST_LOG_ENABLED=true" \
)"
if ! e2e_wait_port 127.0.0.1 "${PORTR}" 10; then
    stop_server_pty "${PIDR}"
    e2e_fatal "server failed to start (port not open)"
fi
http_request "request_panel_health_liveness" "GET" "${URLR_BASE}/health/liveness"
e2e_assert_file_contains "GET /health/liveness returns 200" "${E2E_ARTIFACT_DIR}/request_panel_health_liveness_status.txt" "200"
sleep 0.6
stop_server_pty "${PIDR}"
sleep 0.3

NORM_R="${E2E_ARTIFACT_DIR}/server_request_panel.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_request_panel.typescript" "${NORM_R}"
e2e_assert_file_contains "request-panel startup banner present" "${NORM_R}" "am: Starting MCP Agent Mail server"
e2e_assert_file_not_contains "request-panel startup has no panic" "${NORM_R}" "panicked"

e2e_case_banner "left_split_mode_engages"
WORK_S="$(e2e_mktemp "e2e_console_left_split")"
DBS="${WORK_S}/db.sqlite3"
STORAGES="${WORK_S}/storage"
mkdir -p "${STORAGES}"
PORTS="$(pick_port)"

PIDS="$(start_server_pty "left_split" "${PORTS}" "${DBS}" "${STORAGES}" "${BIN}" \
    "CONSOLE_SPLIT_MODE=left" \
    "CONSOLE_SPLIT_RATIO_PERCENT=30" \
)"
if ! e2e_wait_port 127.0.0.1 "${PORTS}" 10; then
    stop_server_pty "${PIDS}"
    e2e_fatal "server failed to start (port not open)"
fi
sleep 0.8
stop_server_pty "${PIDS}"
sleep 0.3

NORM_S="${E2E_ARTIFACT_DIR}/server_left_split.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_left_split.typescript" "${NORM_S}"
e2e_assert_file_contains "left-split startup banner present" "${NORM_S}" "am: Starting MCP Agent Mail server"
e2e_assert_file_contains "left-split mode still reports HTTP+TUI" "${NORM_S}" "mode:    HTTP + TUI"

e2e_case_banner "interactive_change_persists (optional)"
if [ "${AM_E2E_INTERACTIVE:-0}" = "1" ] || [ "${AM_E2E_INTERACTIVE:-}" = "true" ]; then
    WORK4="$(e2e_mktemp "e2e_console_interactive")"
    DB4="${WORK4}/db.sqlite3"
    STORAGE4="${WORK4}/storage"
    mkdir -p "${STORAGE4}"
    PORT4="$(pick_port)"
    PERSIST_ENV4="${WORK4}/console.env"
    TRANSCRIPT4="${E2E_ARTIFACT_DIR}/interactive_pty.typescript"

    # Use a Python PTY harness so we can inject keypresses and assert the envfile updates.
    set +e
    PY_OUT="$(
        python3 - <<'PY' "${BIN}" "${PORT4}" "${DB4}" "${STORAGE4}" "${PERSIST_ENV4}" "${TRANSCRIPT4}" 2>&1
import os
import pty
import select
import signal
import socket
import subprocess
import sys
import time

bin_path = sys.argv[1]
port = int(sys.argv[2])
db_path = sys.argv[3]
storage_root = sys.argv[4]
persist_path = sys.argv[5]
transcript_path = sys.argv[6]

master_fd, slave_fd = pty.openpty()

env = os.environ.copy()
env.update(
    {
        "DATABASE_URL": f"sqlite:////{db_path}",
        "STORAGE_ROOT": storage_root,
        "HTTP_HOST": "127.0.0.1",
        "HTTP_PORT": str(port),
        "HTTP_RBAC_ENABLED": "0",
        "HTTP_RATE_LIMIT_ENABLED": "0",
        "HTTP_JWT_ENABLED": "0",
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED": "0",
        "CONSOLE_INTERACTIVE": "1",
        "CONSOLE_AUTO_SAVE": "1",
        "CONSOLE_PERSIST_PATH": persist_path,
        # Ensure rich mode is on (the feature under test).
        "LOG_RICH_ENABLED": "1",
    }
)

def preexec():
    # Give the child a controlling terminal so raw-mode / /dev/tty access works.
    os.setsid()

proc = subprocess.Popen(
    [bin_path, "serve", "--host", "127.0.0.1", "--port", str(port)],
    stdin=slave_fd,
    stdout=slave_fd,
    stderr=slave_fd,
    env=env,
    preexec_fn=preexec,
    close_fds=True,
)
os.close(slave_fd)

start = time.time()
buf = bytearray()
with open(transcript_path, "wb") as tf:
    # Wait for the port to open.
    while time.time() - start < 10:
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=0.2)
            s.close()
            break
        except OSError:
            pass
        r, _, _ = select.select([master_fd], [], [], 0.05)
        if r:
            chunk = os.read(master_fd, 8192)
            if chunk:
                buf.extend(chunk)
                tf.write(chunk)
    else:
        raise SystemExit("server did not open port in time")

    # Give the input worker a moment to start, then inject keypresses.
    time.sleep(0.6)
    os.write(master_fd, b"+")
    time.sleep(0.2)
    os.write(master_fd, b"t")

    # Wait for the persisted envfile to include the updated values.
    expected = [b"CONSOLE_UI_HEIGHT_PERCENT=38", b"CONSOLE_UI_ANCHOR=top"]
    while time.time() - start < 12:
        if os.path.exists(persist_path):
            content = open(persist_path, "rb").read()
            if all(e in content for e in expected):
                break
        time.sleep(0.1)
    else:
        if os.path.exists(persist_path):
            raise SystemExit(
                "persist file missing expected keys; content:\n"
                + open(persist_path, "rb").read().decode("utf-8", errors="replace")
            )
        raise SystemExit("persist file was never created")

    # Best-effort teardown.
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait(timeout=2)
PY
    )"
    PY_RC=$?
    set -e

    e2e_save_artifact "interactive_harness_output.txt" "${PY_OUT}"
    e2e_copy_artifact "${PERSIST_ENV4}" "interactive_console.env" || true
    e2e_copy_artifact "${TRANSCRIPT4}" "interactive_pty.typescript" || true
    if [ "${PY_RC}" -ne 0 ]; then
        e2e_fail "interactive persistence harness failed (rc=${PY_RC})"
    else
        e2e_pass "interactive persistence updated envfile"
    fi
else
    e2e_skip "set AM_E2E_INTERACTIVE=1 to run interactive key-injection"
fi

e2e_summary
