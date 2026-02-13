#!/usr/bin/env bash
# e2e_serve.sh - E2E suite for native serve/start ergonomics (br-17c93)
#
# Run via:
#   ./scripts/e2e_test.sh serve
#
# Coverage:
#   - cold start with path normalization
#   - default reuse of already-running Agent Mail server
#   - disabled reuse via flag/env
#   - foreign process conflict behavior
#   - no-auth mode behavior
#   - token loading from ~/.mcp_agent_mail/.env
#
# Artifacts:
#   tests/artifacts/serve/<timestamp>/*

set -euo pipefail

# Safety: default to keeping temp dirs so shared harness cleanup avoids rm -rf.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-serve}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Serve Enhancements E2E Suite"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in curl python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

stop_pid() {
    local pid="${1:-}"
    if [ -z "${pid}" ]; then
        return 0
    fi
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

declare -a _SERVE_E2E_PIDS=()

register_pid() {
    local pid="$1"
    _SERVE_E2E_PIDS+=("${pid}")
}

cleanup_pids() {
    for pid in "${_SERVE_E2E_PIDS[@]}"; do
        stop_pid "${pid}"
    done
}
trap cleanup_pids EXIT

start_am_server() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local home_dir="$5"
    shift 5

    local server_log="${E2E_ARTIFACT_DIR}/server_${label}.log"
    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HOME="${home_dir}"
        export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED="0"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"
        "${AM_BIN}" start --host 127.0.0.1 --port "${port}" --no-tui "$@"
    ) >"${server_log}" 2>&1 &
    echo $!
}

# e2e_ensure_binary logs progress to stdout; keep final line (binary path).
AM_BIN="$(e2e_ensure_binary "am" | tail -n 1)"
e2e_assert_file_exists "am binary exists" "${AM_BIN}"

# ---------------------------------------------------------------------------
# Case 1: Cold start + path normalization + no-auth
# ---------------------------------------------------------------------------

e2e_case_banner "cold start works with --path api normalization"
WORK1="$(e2e_mktemp "serve_case1")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage"
HOME1="${WORK1}/home"
mkdir -p "${STORAGE1}" "${HOME1}"
PORT1="$(pick_port)"

PID1="$(start_am_server "case1" "${PORT1}" "${DB1}" "${STORAGE1}" "${HOME1}" --no-auth --path api)"
register_pid "${PID1}"

if e2e_wait_port "127.0.0.1" "${PORT1}" 10; then
    e2e_pass "case1 server started"
else
    e2e_fail "case1 server failed to start"
fi

if e2e_rpc_call "case1_rpc_api_health" "http://127.0.0.1:${PORT1}/api/" "health_check" "{}"; then
    e2e_rpc_assert_success "case1_rpc_api_health" "RPC health_check works at normalized /api/ path"
else
    e2e_fail "case1 RPC call to /api/ failed"
fi

LOG1="$(cat "${E2E_ARTIFACT_DIR}/server_case1.log" 2>/dev/null || true)"
e2e_assert_contains "case1 startup summary includes normalized /api/ path" "${LOG1}" "/api/"
stop_pid "${PID1}"

# ---------------------------------------------------------------------------
# Case 2: Existing Agent Mail server is reused by default
# ---------------------------------------------------------------------------

e2e_case_banner "second start reuses existing Agent Mail server by default"
WORK2="$(e2e_mktemp "serve_case2")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
HOME2="${WORK2}/home"
mkdir -p "${STORAGE2}" "${HOME2}"
PORT2="$(pick_port)"

PID2="$(start_am_server "case2" "${PORT2}" "${DB2}" "${STORAGE2}" "${HOME2}" --no-auth)"
register_pid "${PID2}"
if e2e_wait_port "127.0.0.1" "${PORT2}" 10; then
    e2e_pass "case2 primary server started"
else
    e2e_fail "case2 primary server failed to start"
fi

set +e
CASE2_OUTPUT="$("${AM_BIN}" start --host 127.0.0.1 --port "${PORT2}" --no-tui --no-auth 2>&1)"
CASE2_RC=$?
set -e
e2e_save_artifact "case2_reuse_output.txt" "${CASE2_OUTPUT}"
e2e_save_artifact "case2_reuse_exit.txt" "${CASE2_RC}"
e2e_assert_exit_code "case2 second start exits 0 (reuse)" 0 "${CASE2_RC}"
e2e_assert_contains "case2 output signals reuse" "${CASE2_OUTPUT}" "reusing existing Agent Mail server"
stop_pid "${PID2}"

# ---------------------------------------------------------------------------
# Case 3: --no-reuse-running blocks second start (exit 2)
# ---------------------------------------------------------------------------

e2e_case_banner "--no-reuse-running prevents reuse when server already running"
WORK3="$(e2e_mktemp "serve_case3")"
DB3="${WORK3}/db.sqlite3"
STORAGE3="${WORK3}/storage"
HOME3="${WORK3}/home"
mkdir -p "${STORAGE3}" "${HOME3}"
PORT3="$(pick_port)"

PID3="$(start_am_server "case3" "${PORT3}" "${DB3}" "${STORAGE3}" "${HOME3}" --no-auth)"
register_pid "${PID3}"
if e2e_wait_port "127.0.0.1" "${PORT3}" 10; then
    e2e_pass "case3 primary server started"
else
    e2e_fail "case3 primary server failed to start"
fi

set +e
CASE3_OUTPUT="$("${AM_BIN}" start --host 127.0.0.1 --port "${PORT3}" --no-tui --no-auth --no-reuse-running 2>&1)"
CASE3_RC=$?
set -e
e2e_save_artifact "case3_no_reuse_output.txt" "${CASE3_OUTPUT}"
e2e_save_artifact "case3_no_reuse_exit.txt" "${CASE3_RC}"
e2e_assert_exit_code "case3 second start exits 2 when reuse disabled" 2 "${CASE3_RC}"
e2e_assert_contains "case3 output mentions existing server" "${CASE3_OUTPUT}" "already running"
stop_pid "${PID3}"

# ---------------------------------------------------------------------------
# Case 4: AM_REUSE_RUNNING=0 disables reuse without flags
# ---------------------------------------------------------------------------

e2e_case_banner "AM_REUSE_RUNNING=0 disables reuse"
WORK4="$(e2e_mktemp "serve_case4")"
DB4="${WORK4}/db.sqlite3"
STORAGE4="${WORK4}/storage"
HOME4="${WORK4}/home"
mkdir -p "${STORAGE4}" "${HOME4}"
PORT4="$(pick_port)"

PID4="$(start_am_server "case4" "${PORT4}" "${DB4}" "${STORAGE4}" "${HOME4}" --no-auth)"
register_pid "${PID4}"
if e2e_wait_port "127.0.0.1" "${PORT4}" 10; then
    e2e_pass "case4 primary server started"
else
    e2e_fail "case4 primary server failed to start"
fi

set +e
CASE4_OUTPUT="$(AM_REUSE_RUNNING=0 "${AM_BIN}" start --host 127.0.0.1 --port "${PORT4}" --no-tui --no-auth 2>&1)"
CASE4_RC=$?
set -e
e2e_save_artifact "case4_env_no_reuse_output.txt" "${CASE4_OUTPUT}"
e2e_save_artifact "case4_env_no_reuse_exit.txt" "${CASE4_RC}"
e2e_assert_exit_code "case4 second start exits 2 when AM_REUSE_RUNNING=0" 2 "${CASE4_RC}"
e2e_assert_contains "case4 output mentions existing server" "${CASE4_OUTPUT}" "already running"
stop_pid "${PID4}"

# ---------------------------------------------------------------------------
# Case 5: Foreign process on port yields deterministic conflict
# ---------------------------------------------------------------------------

e2e_case_banner "foreign listener produces non-Agent-Mail conflict diagnostics"
PORT5="$(pick_port)"
FOREIGN_LOG="${E2E_ARTIFACT_DIR}/case5_foreign_process.log"
python3 -m http.server "${PORT5}" --bind 127.0.0.1 >"${FOREIGN_LOG}" 2>&1 &
FPID=$!
register_pid "${FPID}"

if e2e_wait_port "127.0.0.1" "${PORT5}" 10; then
    e2e_pass "case5 foreign process started"
else
    e2e_fail "case5 foreign process failed to start"
fi

set +e
CASE5_OUTPUT="$("${AM_BIN}" start --host 127.0.0.1 --port "${PORT5}" --no-tui --no-auth 2>&1)"
CASE5_RC=$?
set -e
e2e_save_artifact "case5_foreign_conflict_output.txt" "${CASE5_OUTPUT}"
e2e_save_artifact "case5_foreign_conflict_exit.txt" "${CASE5_RC}"
e2e_assert_exit_code "case5 start exits 2 with foreign listener" 2 "${CASE5_RC}"
e2e_assert_contains "case5 output marks non-Agent-Mail process" "${CASE5_OUTPUT}" "non-Agent-Mail process"
stop_pid "${FPID}"

# ---------------------------------------------------------------------------
# Case 6: Token loaded from ~/.mcp_agent_mail/.env when auth enabled
# ---------------------------------------------------------------------------

e2e_case_banner "auth token is loaded from ~/.mcp_agent_mail/.env"
WORK6="$(e2e_mktemp "serve_case6")"
DB6="${WORK6}/db.sqlite3"
STORAGE6="${WORK6}/storage"
HOME6="${WORK6}/home"
TOKEN6="serve-e2e-token-123"
mkdir -p "${STORAGE6}" "${HOME6}/.mcp_agent_mail"
cat >"${HOME6}/.mcp_agent_mail/.env" <<EOF
HTTP_BEARER_TOKEN=${TOKEN6}
EOF

PORT6="$(pick_port)"
PID6="$(start_am_server "case6" "${PORT6}" "${DB6}" "${STORAGE6}" "${HOME6}" --path mcp)"
register_pid "${PID6}"
if e2e_wait_port "127.0.0.1" "${PORT6}" 10; then
    e2e_pass "case6 auth-enabled server started"
else
    e2e_fail "case6 auth-enabled server failed to start"
fi

if e2e_rpc_call "case6_rpc_without_auth" "http://127.0.0.1:${PORT6}/mcp/" "health_check" "{}"; then
    e2e_fail "case6 unauthenticated call should fail when auth is enabled"
else
    e2e_pass "case6 unauthenticated call failed as expected"
fi
CASE6_STATUS_NOAUTH="$(e2e_rpc_read_status "case6_rpc_without_auth")"
e2e_assert_eq "case6 unauthenticated status is 401" "401" "${CASE6_STATUS_NOAUTH}"

if e2e_rpc_call "case6_rpc_with_auth" "http://127.0.0.1:${PORT6}/mcp/" "health_check" "{}" "Authorization: Bearer ${TOKEN6}"; then
    e2e_rpc_assert_success "case6_rpc_with_auth" "case6 authenticated call succeeds"
else
    e2e_fail "case6 authenticated call failed unexpectedly"
fi

LOG6="$(cat "${E2E_ARTIFACT_DIR}/server_case6.log" 2>/dev/null || true)"
e2e_assert_not_contains "case6 server log does not leak raw token" "${LOG6}" "${TOKEN6}"
stop_pid "${PID6}"

e2e_summary

