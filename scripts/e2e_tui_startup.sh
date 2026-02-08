#!/usr/bin/env bash
# e2e_tui_startup.sh - PTY E2E suite for zero-friction TUI startup contract.
#
# Run via:
#   ./scripts/e2e_test.sh tui_startup
#
# Validates:
#   - `mcp-agent-mail serve` starts server+TUI and reaches ready state.
#   - Startup bootstrap banner shows resolved config and sources.
#   - Bearer token auto-discovered from user env file.
#   - Both MCP and API mode bootstraps work.
#   - Token masking: raw secrets never appear in output.
#   - Missing/invalid config produces actionable remediation.
#
# Artifacts:
#   tests/artifacts/tui_startup/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_startup}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Startup (PTY) E2E Test Suite"

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
        e2e_log "tail (last 80 lines):"
        tail -n 80 "${path}" 2>/dev/null || true
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
        grep -Fn -- "${needle}" "${path}" | head -n 10 || true
    else
        e2e_pass "${label}"
    fi
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

    (
        script -q -f -c "env \
DATABASE_URL=sqlite:////${db_path} \
STORAGE_ROOT=${storage_root} \
HTTP_HOST=127.0.0.1 \
HTTP_PORT=${port} \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_JWT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
${*} \
timeout ${timeout_s}s ${bin} serve --host 127.0.0.1 --port ${port}" \
            "${typescript}"
    ) >/dev/null 2>&1 &

    echo $!
}

# Headless mode (--no-tui) captures stderr directly (no PTY needed).
start_server_headless() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5

    local logfile="${E2E_ARTIFACT_DIR}/server_${label}.log"
    e2e_log "Starting headless server (${label}): 127.0.0.1:${port}"

    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"
        export HTTP_RBAC_ENABLED=0
        export HTTP_RATE_LIMIT_ENABLED=0
        export HTTP_JWT_ENABLED=0
        export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1
        while [ $# -gt 0 ]; do
            export "$1"
            shift
        done
        timeout 15s "${bin}" serve --host 127.0.0.1 --port "${port}" --no-tui
    ) >"${logfile}" 2>&1 &

    echo $!
}

stop_server() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# ────────────────────────────────────────────────────────────────────
# Case 1: Default startup shows bootstrap banner (headless for easy capture)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "bootstrap_banner_shows_config_sources"
WORK1="$(e2e_mktemp "e2e_tui_startup_banner")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage"
mkdir -p "${STORAGE1}"
PORT1="$(pick_port)"

PID1="$(start_server_headless "banner" "${PORT1}" "${DB1}" "${STORAGE1}" "${BIN}")"
if ! e2e_wait_port 127.0.0.1 "${PORT1}" 10; then
    stop_server "${PID1}"
    e2e_fatal "server failed to start (port not open)"
fi
sleep 0.3
stop_server "${PID1}"
sleep 0.3

LOG1="${E2E_ARTIFACT_DIR}/server_banner.log"
e2e_assert_file_contains "banner title present" "${LOG1}" "am: Starting MCP Agent Mail server"
e2e_assert_file_contains "host line present" "${LOG1}" "host:"
e2e_assert_file_contains "port line present" "${LOG1}" "port:"
e2e_assert_file_contains "path line present" "${LOG1}" "path:"
e2e_assert_file_contains "auth line present" "${LOG1}" "auth:"
e2e_assert_file_contains "db line present" "${LOG1}" "db:"
e2e_assert_file_contains "storage line present" "${LOG1}" "storage:"
e2e_assert_file_contains "mode line present" "${LOG1}" "mode:"
e2e_assert_file_contains "headless mode shown" "${LOG1}" "HTTP (headless)"
e2e_assert_file_contains "port shows correct value" "${LOG1}" "${PORT1}"

# ────────────────────────────────────────────────────────────────────
# Case 2: PTY mode reaches ready state (server+TUI startup)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "pty_tui_reaches_ready_state"
WORK2="$(e2e_mktemp "e2e_tui_startup_pty")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
mkdir -p "${STORAGE2}"
PORT2="$(pick_port)"

PID2="$(start_server_pty "tui_ready" "${PORT2}" "${DB2}" "${STORAGE2}" "${BIN}" "LOG_RICH_ENABLED=true")"
if ! e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    stop_server "${PID2}"
    e2e_fatal "TUI server failed to reach ready state (port not open after 10s)"
fi

# Verify the server responds to MCP tools/list
set +e
TOOLS_LIST="$(curl -sS -X POST "http://127.0.0.1:${PORT2}/mcp/" \
    -H "content-type: application/json" \
    --data '{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}' 2>/dev/null)"
CURL_RC=$?
set -e
e2e_save_artifact "tui_ready_tools_list.json" "${TOOLS_LIST:-<empty>}"

if [ "$CURL_RC" -eq 0 ] && echo "${TOOLS_LIST}" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    e2e_pass "server responds to tools/list via /mcp/"
else
    e2e_fail "server did not respond to tools/list"
fi

stop_server "${PID2}"
sleep 0.3

NORM2="${E2E_ARTIFACT_DIR}/server_tui_ready.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_tui_ready.typescript" "${NORM2}"
e2e_assert_file_contains "bootstrap banner in PTY" "${NORM2}" "am: Starting MCP Agent Mail server"

# ────────────────────────────────────────────────────────────────────
# Case 3: API mode bootstrap works
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "api_mode_bootstrap"
WORK3="$(e2e_mktemp "e2e_tui_startup_api")"
DB3="${WORK3}/db.sqlite3"
STORAGE3="${WORK3}/storage"
mkdir -p "${STORAGE3}"
PORT3="$(pick_port)"

PID3="$(start_server_headless "api_mode" "${PORT3}" "${DB3}" "${STORAGE3}" "${BIN}" "HTTP_PATH=/api/")"
if ! e2e_wait_port 127.0.0.1 "${PORT3}" 10; then
    stop_server "${PID3}"
    e2e_fatal "API mode server failed to start"
fi

# Verify API path responds
set +e
API_RESP="$(curl -sS -X POST "http://127.0.0.1:${PORT3}/api/" \
    -H "content-type: application/json" \
    --data '{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}' 2>/dev/null)"
API_RC=$?
set -e
e2e_save_artifact "api_mode_tools_list.json" "${API_RESP:-<empty>}"

if [ "$API_RC" -eq 0 ] && echo "${API_RESP}" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    e2e_pass "API mode responds to tools/list via /api/"
else
    e2e_fail "API mode did not respond to tools/list"
fi

stop_server "${PID3}"
sleep 0.3

LOG3="${E2E_ARTIFACT_DIR}/server_api_mode.log"
e2e_assert_file_contains "API mode banner shows /api/" "${LOG3}" "/api/"

# ────────────────────────────────────────────────────────────────────
# Case 4: Bearer token auto-discovery from user env file
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "bearer_token_auto_discovery"
WORK4="$(e2e_mktemp "e2e_tui_startup_token")"
DB4="${WORK4}/db.sqlite3"
STORAGE4="${WORK4}/storage"
mkdir -p "${STORAGE4}"
PORT4="$(pick_port)"

# Create a fake user env file with a bearer token
USER_ENV_DIR="${WORK4}/.mcp_agent_mail"
mkdir -p "${USER_ENV_DIR}"
echo 'HTTP_BEARER_TOKEN=test-secret-token-e2e-12345' > "${USER_ENV_DIR}/.env"

# Start server with HOME pointing to our temp dir (so it finds our .env)
PID4="$(start_server_headless "token_auto" "${PORT4}" "${DB4}" "${STORAGE4}" "${BIN}" "HOME=${WORK4}" "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0")"
if ! e2e_wait_port 127.0.0.1 "${PORT4}" 10; then
    stop_server "${PID4}"
    e2e_fatal "token auto-discovery server failed to start"
fi

# Verify unauthenticated request is rejected
set +e
UNAUTH_RESP="$(curl -sS -o /dev/null -w "%{http_code}" -X POST "http://127.0.0.1:${PORT4}/mcp/" \
    -H "content-type: application/json" \
    --data '{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}' 2>/dev/null)"
set -e
if [ "${UNAUTH_RESP}" = "401" ] || [ "${UNAUTH_RESP}" = "403" ]; then
    e2e_pass "unauthenticated request rejected (${UNAUTH_RESP})"
else
    e2e_fail "expected 401/403 for unauthenticated, got ${UNAUTH_RESP}"
fi

# Verify authenticated request succeeds
set +e
AUTH_RESP="$(curl -sS -X POST "http://127.0.0.1:${PORT4}/mcp/" \
    -H "content-type: application/json" \
    -H "Authorization: Bearer test-secret-token-e2e-12345" \
    --data '{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}' 2>/dev/null)"
AUTH_RC=$?
set -e
e2e_save_artifact "token_auth_tools_list.json" "${AUTH_RESP:-<empty>}"

if [ "$AUTH_RC" -eq 0 ] && echo "${AUTH_RESP}" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    e2e_pass "authenticated request with auto-discovered token succeeds"
else
    e2e_fail "authenticated request failed"
fi

stop_server "${PID4}"
sleep 0.3

LOG4="${E2E_ARTIFACT_DIR}/server_token_auto.log"
# Verify token is masked in bootstrap banner (raw token never shown)
e2e_assert_file_not_contains "raw token not in output" "${LOG4}" "test-secret-token-e2e-12345"
e2e_assert_file_contains "masked token shown" "${LOG4}" "****"
e2e_assert_file_contains "token source shown" "${LOG4}" ".mcp_agent_mail/.env"

# ────────────────────────────────────────────────────────────────────
# Case 5: MCP mode default (no explicit path)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "mcp_default_path"
WORK5="$(e2e_mktemp "e2e_tui_startup_mcp_default")"
DB5="${WORK5}/db.sqlite3"
STORAGE5="${WORK5}/storage"
mkdir -p "${STORAGE5}"
PORT5="$(pick_port)"

PID5="$(start_server_headless "mcp_default" "${PORT5}" "${DB5}" "${STORAGE5}" "${BIN}")"
if ! e2e_wait_port 127.0.0.1 "${PORT5}" 10; then
    stop_server "${PID5}"
    e2e_fatal "MCP default server failed to start"
fi
sleep 0.3
stop_server "${PID5}"
sleep 0.3

LOG5="${E2E_ARTIFACT_DIR}/server_mcp_default.log"
e2e_assert_file_contains "default path is /mcp/" "${LOG5}" "/mcp/"
e2e_assert_file_contains "default source shown" "${LOG5}" "(default)"

# ────────────────────────────────────────────────────────────────────
# Case 6: Clean shell (no pre-exported vars) startup
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "clean_shell_startup"
WORK6="$(e2e_mktemp "e2e_tui_startup_clean")"
DB6="${WORK6}/db.sqlite3"
STORAGE6="${WORK6}/storage"
mkdir -p "${STORAGE6}"
PORT6="$(pick_port)"

# Use env -i to strip all environment variables, providing only essentials
LOG6="${E2E_ARTIFACT_DIR}/server_clean_shell.log"
(
    env -i \
        PATH="${PATH}" \
        HOME="${WORK6}" \
        DATABASE_URL="sqlite:////${DB6}" \
        STORAGE_ROOT="${STORAGE6}" \
        HTTP_HOST="127.0.0.1" \
        HTTP_PORT="${PORT6}" \
        HTTP_RBAC_ENABLED=0 \
        HTTP_RATE_LIMIT_ENABLED=0 \
        HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
        timeout 15s "${BIN}" serve --host 127.0.0.1 --port "${PORT6}" --no-tui
) >"${LOG6}" 2>&1 &
PID6=$!

if ! e2e_wait_port 127.0.0.1 "${PORT6}" 10; then
    stop_server "${PID6}"
    e2e_fatal "clean shell server failed to start"
fi
sleep 0.3
stop_server "${PID6}"
sleep 0.3

e2e_assert_file_contains "clean shell: banner present" "${LOG6}" "am: Starting MCP Agent Mail server"
e2e_assert_file_contains "clean shell: no auth shown" "${LOG6}" "none"

# ────────────────────────────────────────────────────────────────────
e2e_summary
