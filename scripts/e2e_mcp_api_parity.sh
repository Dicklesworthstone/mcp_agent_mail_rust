#!/usr/bin/env bash
# e2e_mcp_api_parity.sh - PTY E2E suite for MCP/API mode switching and parity.
#
# Run via:
#   ./scripts/e2e_test.sh mcp_api_parity
#   # or directly:
#   bash scripts/e2e_mcp_api_parity.sh
#
# Validates:
#   - MCP and API transport modes return identical results for critical tools.
#   - Path alias behaviour: /mcp/ server also responds on /api/ and vice versa.
#   - Explicit --transport and --path flags are respected.
#   - Bootstrap banner shows correct mode information.
#   - Diagnostics are clear when misconfigured.
#   - resources/list parity across modes.
#
# Artifacts:
#   tests/artifacts/mcp_api_parity/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-mcp_api_parity}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "MCP/API Mode Switching Parity E2E Test Suite"

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in curl python3; do
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

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

startup_case_dir() {
    local label="$1"
    printf '%s\n' "${E2E_ARTIFACT_DIR}/server_startup_${label}"
}

startup_write_start_artifacts() {
    local label="$1"
    local started_ms="$2"
    local pid="$3"
    local log_path="$4"
    local mode="$5"
    local command_text="$6"
    local startup_timeout_s="${E2E_SERVER_STARTUP_TIMEOUT_SECONDS:-10}"

    local case_id="server_startup_${label}"
    local case_dir
    case_dir="$(startup_case_dir "${label}")"
    mkdir -p "${case_dir}"

    printf '%s\n' "${command_text}" > "${case_dir}/command.txt"
    printf '%s\n' "${started_ms}" > "${case_dir}/start_ms.txt"
    printf '%s\n' "${pid}" > "${case_dir}/pid.txt"
    printf '%s\n' "${log_path}" > "${case_dir}/log_path.txt"
    printf '%s\n' "${mode}" > "${case_dir}/mode.txt"
    printf '%s\n' "${startup_timeout_s}" > "${case_dir}/startup_timeout_seconds.txt"

    e2e_save_artifact "${case_id}_command.txt" "${command_text}"
    e2e_save_artifact "${case_id}_pid.txt" "${pid}"
    e2e_save_artifact "${case_id}_log_path.txt" "${log_path}"
    e2e_save_artifact "${case_id}_mode.txt" "${mode}"
    e2e_save_artifact "${case_id}_startup_timeout_seconds.txt" "${startup_timeout_s}"
}

startup_finalize_artifacts() {
    local label="$1"
    local status="$2"
    local detail="${3:-}"

    local case_id="server_startup_${label}"
    local case_dir
    case_dir="$(startup_case_dir "${label}")"
    mkdir -p "${case_dir}"

    local finished_ms elapsed_ms started_ms
    finished_ms="$(_e2e_now_ms)"
    elapsed_ms=0
    started_ms=0

    if [ -f "${case_dir}/start_ms.txt" ]; then
        started_ms="$(cat "${case_dir}/start_ms.txt" 2>/dev/null || echo 0)"
    fi
    if [[ "${started_ms}" =~ ^[0-9]+$ ]] && [ "${started_ms}" -gt 0 ]; then
        elapsed_ms=$(( finished_ms - started_ms ))
    fi

    printf '%s\n' "${status}" > "${case_dir}/status.txt"
    printf '%s\n' "${detail}" > "${case_dir}/detail.txt"
    printf '%s\n' "${finished_ms}" > "${case_dir}/finished_ms.txt"
    printf '%s\n' "${elapsed_ms}" > "${case_dir}/startup_elapsed_ms.txt"

    e2e_save_artifact "${case_id}_status.txt" "${status}"
    e2e_save_artifact "${case_id}_detail.txt" "${detail}"
    e2e_save_artifact "${case_id}_startup_elapsed_ms.txt" "${elapsed_ms}"
}

startup_write_failure_diagnostics() {
    local label="$1"
    local pid="$2"
    local port="$3"
    local startup_timeout_s="$4"

    local case_id="server_startup_${label}"
    local case_dir diag_file log_path
    case_dir="$(startup_case_dir "${label}")"
    mkdir -p "${case_dir}"
    diag_file="${case_dir}/startup_failure_diagnostics.txt"
    log_path=""
    if [ -f "${case_dir}/log_path.txt" ]; then
        log_path="$(cat "${case_dir}/log_path.txt" 2>/dev/null || true)"
    fi

    {
        echo "MCP/API parity server startup failure diagnostics"
        echo "==============================================="
        echo "timestamp: $(_e2e_now_rfc3339)"
        echo "label: ${label}"
        echo "port: ${port}"
        echo "startup_timeout_seconds: ${startup_timeout_s}"
        echo "pid: ${pid}"
        echo "log_path: ${log_path}"
        echo ""
        echo "=== startup command ==="
        if [ -f "${case_dir}/command.txt" ]; then
            cat "${case_dir}/command.txt"
        else
            echo "(command file missing)"
        fi
        echo ""
        echo "=== process status ==="
        if [ -n "${pid}" ]; then
            ps -p "${pid}" -o pid=,ppid=,etime=,stat=,args= 2>/dev/null || echo "(process not running)"
        else
            echo "(no pid)"
        fi
        echo ""
        echo "=== server log tail (last 200 lines) ==="
        if [ -n "${log_path}" ] && [ -f "${log_path}" ]; then
            tail -n 200 "${log_path}"
        else
            echo "(log path missing or unreadable)"
        fi
        echo ""
        echo "=== listeners ==="
        ss -tlnp 2>/dev/null | head -40 || netstat -tlnp 2>/dev/null | head -40 || echo "(unable to inspect listeners)"
    } > "${diag_file}"

    e2e_save_artifact "${case_id}_startup_failure_diagnostics.txt" "$(cat "${diag_file}" 2>/dev/null || true)"
}

start_server() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5
    local -a env_overrides=("$@")
    local timeout_s=20

    local server_log="${E2E_ARTIFACT_DIR}/server_${label}.log"
    e2e_log "Starting server (${label}): 127.0.0.1:${port}"
    local started_ms="$(_e2e_now_ms)"
    local -a cmd_parts=(
        env
        "DATABASE_URL=sqlite:////${db_path}"
        "STORAGE_ROOT=${storage_root}"
        "HTTP_HOST=127.0.0.1"
        "HTTP_PORT=${port}"
        "HTTP_RBAC_ENABLED=0"
        "HTTP_RATE_LIMIT_ENABLED=0"
        "HTTP_JWT_ENABLED=0"
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1"
    )
    local override
    for override in "${env_overrides[@]}"; do
        cmd_parts+=("${override}")
    done
    cmd_parts+=(timeout "${timeout_s}s" "${bin}" serve --host 127.0.0.1 --port "${port}" --no-tui)
    local server_cmd=""
    local part
    for part in "${cmd_parts[@]}"; do
        printf -v server_cmd '%s %q' "${server_cmd}" "${part}"
    done
    server_cmd="${server_cmd# }"

    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"
        export HTTP_RBAC_ENABLED=0
        export HTTP_RATE_LIMIT_ENABLED=0
        export HTTP_JWT_ENABLED=0
        export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1

        for override in "${env_overrides[@]}"; do
            export "${override}"
        done

        timeout "${timeout_s}s" "${bin}" serve --host 127.0.0.1 --port "${port}" --no-tui
    ) >"${server_log}" 2>&1 &
    local pid="$!"
    startup_write_start_artifacts "${label}" "${started_ms}" "${pid}" "${server_log}" "headless" "${server_cmd}"
    echo "${pid}"
}

stop_server() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

wait_for_server_start_or_fail() {
    local label="$1"
    local pid="$2"
    local port="$3"
    local fatal_msg="$4"
    shift 4 || true
    local startup_timeout_s="${E2E_SERVER_STARTUP_TIMEOUT_SECONDS:-10}"

    if ! e2e_wait_port 127.0.0.1 "${port}" "${startup_timeout_s}"; then
        startup_finalize_artifacts "${label}" "failed" "port did not open within ${startup_timeout_s}s"
        startup_write_failure_diagnostics "${label}" "${pid}" "${port}" "${startup_timeout_s}"
        stop_server "${pid}"
        while [ "$#" -gt 0 ]; do
            stop_server "$1"
            shift
        done
        e2e_fatal "${fatal_msg}"
    fi

    startup_finalize_artifacts "${label}" "ready" "port opened at http://127.0.0.1:${port}"
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

    local start_ns end_ns elapsed_ms
    start_ns="$(date +%s%N)"
    set +e
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

    if [ "${rc}" -ne 0 ]; then
        return 1
    fi
    return 0
}

http_post_json() {
    local case_id="$1"
    local url="$2"
    local payload="$3"
    shift 3

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local request_file="${E2E_ARTIFACT_DIR}/${case_id}_request.json"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.json"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local timing_file="${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call_raw "${case_id}" "${url}" "${payload}" "$@"; then
        :
    fi

    cp "${case_dir}/request.json" "${request_file}" 2>/dev/null || e2e_save_artifact "${case_id}_request.json" "${payload}"
    cp "${case_dir}/response.json" "${body_file}" 2>/dev/null || true
    cp "${case_dir}/status.txt" "${status_file}" 2>/dev/null || true
    cp "${case_dir}/headers.txt" "${headers_file}" 2>/dev/null || true
    cp "${case_dir}/timing.txt" "${timing_file}" 2>/dev/null || true
    cp "${case_dir}/curl_stderr.txt" "${curl_stderr_file}" 2>/dev/null || true

    local status
    status="$(cat "${status_file}" 2>/dev/null || echo "")"
    if [ -z "${status}" ]; then
        echo "000" > "${status_file}"
    fi
}

jsonrpc_tools_list_payload() {
    echo '{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}'
}

jsonrpc_resources_list_payload() {
    echo '{"jsonrpc":"2.0","method":"resources/list","id":1,"params":{}}'
}

jsonrpc_tools_call_payload() {
    local tool_name="$1"
    local args_json="$2"
    [ -z "${args_json}" ] && args_json="{}"
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

count_tools_in_response() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
res = data.get("result") or {}
tools = res.get("tools") or []
print(len(tools) if isinstance(tools, list) else 0)
PY
}

count_resources_in_response() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
res = data.get("result") or {}
resources = res.get("resources") or []
print(len(resources) if isinstance(resources, list) else 0)
PY
}

extract_tool_text() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
res = data.get("result") or {}
content = res.get("content") or []
if content and isinstance(content[0], dict) and content[0].get("type") == "text":
    print(content[0].get("text") or "")
else:
    print(json.dumps(res))
PY
}

has_jsonrpc_result() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
try:
    data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
    print("1" if "result" in data else "0")
except Exception:
    print("0")
PY
}

has_jsonrpc_error() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
try:
    data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
    print("1" if "error" in data else "0")
except Exception:
    print("0")
PY
}

# Compare two JSON response files structurally (field-level parity).
# Returns 0 if the critical fields match, 1 otherwise.
compare_tool_call_results() {
    local file_a="$1"
    local file_b="$2"
    python3 - <<'PY' "$file_a" "$file_b"
import json, sys

a = json.load(open(sys.argv[1], "r", encoding="utf-8"))
b = json.load(open(sys.argv[2], "r", encoding="utf-8"))

def extract_text(d):
    res = d.get("result") or {}
    content = res.get("content") or []
    if content and isinstance(content[0], dict) and content[0].get("type") == "text":
        return content[0].get("text") or ""
    return json.dumps(res, sort_keys=True)

ta = extract_text(a)
tb = extract_text(b)

# Parse inner JSON if present (tool results are often JSON strings).
try:
    ja = json.loads(ta)
    jb = json.loads(tb)
    # Compare structurally: same keys at top level
    if isinstance(ja, dict) and isinstance(jb, dict):
        keys_a = set(ja.keys())
        keys_b = set(jb.keys())
        if keys_a == keys_b:
            print("MATCH")
            sys.exit(0)
        else:
            print(f"KEY_DIFF: a_only={keys_a-keys_b}, b_only={keys_b-keys_a}")
            sys.exit(1)
    elif isinstance(ja, list) and isinstance(jb, list):
        if len(ja) == len(jb):
            print("MATCH")
            sys.exit(0)
        else:
            print(f"LEN_DIFF: {len(ja)} vs {len(jb)}")
            sys.exit(1)
except (json.JSONDecodeError, TypeError):
    pass

# Fallback: compare text directly
if ta == tb:
    print("MATCH")
    sys.exit(0)
else:
    print(f"TEXT_DIFF: len(a)={len(ta)}, len(b)={len(tb)}")
    sys.exit(1)
PY
}

e2e_assert_file_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq -- "${needle}" "${path}" 2>/dev/null; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        e2e_log "missing needle: ${needle}"
        e2e_log "in file: ${path}"
        tail -n 40 "${path}" 2>/dev/null || true
    fi
}

e2e_assert_file_not_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq -- "${needle}" "${path}" 2>/dev/null; then
        e2e_fail "${label}"
        e2e_log "unexpected needle: ${needle}"
    else
        e2e_pass "${label}"
    fi
}

# ---------------------------------------------------------------------------
# Build binary
# ---------------------------------------------------------------------------

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# ══════════════════════════════════════════════════════════════════════
# Case 1: tools/list parity — MCP and API mode return same tool set
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "tools_list_parity_across_modes"

WORK1="$(e2e_mktemp "e2e_parity_tl")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage"
mkdir -p "${STORAGE1}"

PORT_MCP="$(pick_port)"
PORT_API="$(pick_port)"

PID_MCP="$(start_server "mcp_mode" "${PORT_MCP}" "${DB1}" "${STORAGE1}" "${BIN}" "HTTP_PATH=/mcp/")"
PID_API="$(start_server "api_mode" "${PORT_API}" "${DB1}" "${STORAGE1}" "${BIN}" "HTTP_PATH=/api/")"

wait_for_server_start_or_fail "mcp_mode" "${PID_MCP}" "${PORT_MCP}" "MCP server failed to start" "${PID_API}"
wait_for_server_start_or_fail "api_mode" "${PID_API}" "${PORT_API}" "API server failed to start" "${PID_MCP}"

PAYLOAD_TL="$(jsonrpc_tools_list_payload)"
http_post_json "c1_mcp_tools_list" "http://127.0.0.1:${PORT_MCP}/mcp/" "${PAYLOAD_TL}"
http_post_json "c1_api_tools_list" "http://127.0.0.1:${PORT_API}/api/" "${PAYLOAD_TL}"

MCP_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c1_mcp_tools_list_status.txt")"
API_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c1_api_tools_list_status.txt")"
e2e_assert_eq "MCP tools/list HTTP 200" "200" "${MCP_STATUS}"
e2e_assert_eq "API tools/list HTTP 200" "200" "${API_STATUS}"

MCP_COUNT="$(count_tools_in_response "${E2E_ARTIFACT_DIR}/c1_mcp_tools_list_body.json")"
API_COUNT="$(count_tools_in_response "${E2E_ARTIFACT_DIR}/c1_api_tools_list_body.json")"
e2e_assert_eq "tool count parity (MCP=${MCP_COUNT} API=${API_COUNT})" "${MCP_COUNT}" "${API_COUNT}"

MCP_HAS_RESULT="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c1_mcp_tools_list_body.json")"
API_HAS_RESULT="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c1_api_tools_list_body.json")"
e2e_assert_eq "MCP has result field" "1" "${MCP_HAS_RESULT}"
e2e_assert_eq "API has result field" "1" "${API_HAS_RESULT}"

stop_server "${PID_MCP}"
stop_server "${PID_API}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 2: resources/list parity
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "resources_list_parity_across_modes"

WORK2="$(e2e_mktemp "e2e_parity_rl")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
mkdir -p "${STORAGE2}"

PORT_MCP2="$(pick_port)"
PORT_API2="$(pick_port)"

PID_MCP2="$(start_server "mcp_res" "${PORT_MCP2}" "${DB2}" "${STORAGE2}" "${BIN}" "HTTP_PATH=/mcp/")"
PID_API2="$(start_server "api_res" "${PORT_API2}" "${DB2}" "${STORAGE2}" "${BIN}" "HTTP_PATH=/api/")"

wait_for_server_start_or_fail "mcp_res" "${PID_MCP2}" "${PORT_MCP2}" "MCP res server failed" "${PID_API2}"
wait_for_server_start_or_fail "api_res" "${PID_API2}" "${PORT_API2}" "API res server failed" "${PID_MCP2}"

PAYLOAD_RL="$(jsonrpc_resources_list_payload)"
http_post_json "c2_mcp_res_list" "http://127.0.0.1:${PORT_MCP2}/mcp/" "${PAYLOAD_RL}"
http_post_json "c2_api_res_list" "http://127.0.0.1:${PORT_API2}/api/" "${PAYLOAD_RL}"

MCP_R_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c2_mcp_res_list_status.txt")"
API_R_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c2_api_res_list_status.txt")"
e2e_assert_eq "MCP resources/list HTTP 200" "200" "${MCP_R_STATUS}"
e2e_assert_eq "API resources/list HTTP 200" "200" "${API_R_STATUS}"

MCP_R_COUNT="$(count_resources_in_response "${E2E_ARTIFACT_DIR}/c2_mcp_res_list_body.json")"
API_R_COUNT="$(count_resources_in_response "${E2E_ARTIFACT_DIR}/c2_api_res_list_body.json")"
e2e_assert_eq "resource count parity (MCP=${MCP_R_COUNT} API=${API_R_COUNT})" "${MCP_R_COUNT}" "${API_R_COUNT}"

stop_server "${PID_MCP2}"
stop_server "${PID_API2}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 3: ensure_project + register_agent tool call parity
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "tool_call_parity_ensure_project_register_agent"

WORK3="$(e2e_mktemp "e2e_parity_tools")"
DB3_MCP="${WORK3}/db_mcp.sqlite3"
DB3_API="${WORK3}/db_api.sqlite3"
STORAGE3_MCP="${WORK3}/storage_mcp"
STORAGE3_API="${WORK3}/storage_api"
mkdir -p "${STORAGE3_MCP}" "${STORAGE3_API}"

PORT_MCP3="$(pick_port)"
PORT_API3="$(pick_port)"

PID_MCP3="$(start_server "mcp_tools" "${PORT_MCP3}" "${DB3_MCP}" "${STORAGE3_MCP}" "${BIN}" "HTTP_PATH=/mcp/")"
PID_API3="$(start_server "api_tools" "${PORT_API3}" "${DB3_API}" "${STORAGE3_API}" "${BIN}" "HTTP_PATH=/api/")"

wait_for_server_start_or_fail "mcp_tools" "${PID_MCP3}" "${PORT_MCP3}" "MCP tools server failed" "${PID_API3}"
wait_for_server_start_or_fail "api_tools" "${PID_API3}" "${PORT_API3}" "API tools server failed" "${PID_MCP3}"

# ensure_project
EP_ARGS='{"human_key":"/tmp/e2e_parity_project"}'
PAYLOAD_EP="$(jsonrpc_tools_call_payload "ensure_project" "${EP_ARGS}")"
http_post_json "c3_mcp_ep" "http://127.0.0.1:${PORT_MCP3}/mcp/" "${PAYLOAD_EP}"
http_post_json "c3_api_ep" "http://127.0.0.1:${PORT_API3}/api/" "${PAYLOAD_EP}"

MCP_EP_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c3_mcp_ep_status.txt")"
API_EP_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c3_api_ep_status.txt")"
e2e_assert_eq "MCP ensure_project HTTP 200" "200" "${MCP_EP_STATUS}"
e2e_assert_eq "API ensure_project HTTP 200" "200" "${API_EP_STATUS}"

MCP_EP_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c3_mcp_ep_body.json")"
API_EP_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c3_api_ep_body.json")"
e2e_assert_eq "MCP ensure_project has result" "1" "${MCP_EP_OK}"
e2e_assert_eq "API ensure_project has result" "1" "${API_EP_OK}"

# Structural parity check
set +e
PARITY_EP="$(compare_tool_call_results "${E2E_ARTIFACT_DIR}/c3_mcp_ep_body.json" "${E2E_ARTIFACT_DIR}/c3_api_ep_body.json" 2>&1)"
PARITY_EP_RC=$?
set -e
if [ "${PARITY_EP_RC}" -eq 0 ]; then
    e2e_pass "ensure_project structural parity: ${PARITY_EP}"
else
    e2e_fail "ensure_project structural parity: ${PARITY_EP}"
fi

# register_agent
RA_ARGS='{"project_key":"/tmp/e2e_parity_project","program":"e2e-test","model":"test-1"}'
PAYLOAD_RA="$(jsonrpc_tools_call_payload "register_agent" "${RA_ARGS}")"
http_post_json "c3_mcp_ra" "http://127.0.0.1:${PORT_MCP3}/mcp/" "${PAYLOAD_RA}"
http_post_json "c3_api_ra" "http://127.0.0.1:${PORT_API3}/api/" "${PAYLOAD_RA}"

MCP_RA_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c3_mcp_ra_status.txt")"
API_RA_STATUS="$(cat "${E2E_ARTIFACT_DIR}/c3_api_ra_status.txt")"
e2e_assert_eq "MCP register_agent HTTP 200" "200" "${MCP_RA_STATUS}"
e2e_assert_eq "API register_agent HTTP 200" "200" "${API_RA_STATUS}"

MCP_RA_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c3_mcp_ra_body.json")"
API_RA_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c3_api_ra_body.json")"
e2e_assert_eq "MCP register_agent has result" "1" "${MCP_RA_OK}"
e2e_assert_eq "API register_agent has result" "1" "${API_RA_OK}"

set +e
PARITY_RA="$(compare_tool_call_results "${E2E_ARTIFACT_DIR}/c3_mcp_ra_body.json" "${E2E_ARTIFACT_DIR}/c3_api_ra_body.json" 2>&1)"
PARITY_RA_RC=$?
set -e
if [ "${PARITY_RA_RC}" -eq 0 ]; then
    e2e_pass "register_agent structural parity: ${PARITY_RA}"
else
    e2e_fail "register_agent structural parity: ${PARITY_RA}"
fi

stop_server "${PID_MCP3}"
stop_server "${PID_API3}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 4: Path alias — /mcp/ server also accepts /api/ and vice versa
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "path_alias_mcp_accepts_api_and_vice_versa"

WORK4="$(e2e_mktemp "e2e_parity_alias")"
DB4="${WORK4}/db.sqlite3"
STORAGE4="${WORK4}/storage"
mkdir -p "${STORAGE4}"

PORT_ALIAS="$(pick_port)"

# Start with MCP path
PID_ALIAS="$(start_server "alias_mcp" "${PORT_ALIAS}" "${DB4}" "${STORAGE4}" "${BIN}" "HTTP_PATH=/mcp/")"
wait_for_server_start_or_fail "alias_mcp" "${PID_ALIAS}" "${PORT_ALIAS}" "alias server failed"

# /mcp/ should respond
http_post_json "c4_primary_mcp" "http://127.0.0.1:${PORT_ALIAS}/mcp/" "${PAYLOAD_TL}"
S_PRIMARY="$(cat "${E2E_ARTIFACT_DIR}/c4_primary_mcp_status.txt")"
e2e_assert_eq "primary /mcp/ responds 200" "200" "${S_PRIMARY}"

# /api/ alias should also respond
http_post_json "c4_alias_api" "http://127.0.0.1:${PORT_ALIAS}/api/" "${PAYLOAD_TL}"
S_ALIAS="$(cat "${E2E_ARTIFACT_DIR}/c4_alias_api_status.txt")"
e2e_assert_eq "alias /api/ responds 200" "200" "${S_ALIAS}"

# Parity: both return same tool count
C_PRIMARY="$(count_tools_in_response "${E2E_ARTIFACT_DIR}/c4_primary_mcp_body.json")"
C_ALIAS="$(count_tools_in_response "${E2E_ARTIFACT_DIR}/c4_alias_api_body.json")"
e2e_assert_eq "alias tool count matches primary (${C_PRIMARY}=${C_ALIAS})" "${C_PRIMARY}" "${C_ALIAS}"

stop_server "${PID_ALIAS}"
sleep 0.3

# Now start with API path and verify /mcp/ alias works
PORT_ALIAS2="$(pick_port)"
PID_ALIAS2="$(start_server "alias_api" "${PORT_ALIAS2}" "${DB4}" "${STORAGE4}" "${BIN}" "HTTP_PATH=/api/")"
wait_for_server_start_or_fail "alias_api" "${PID_ALIAS2}" "${PORT_ALIAS2}" "alias api server failed"

# /api/ primary responds
http_post_json "c4_primary_api" "http://127.0.0.1:${PORT_ALIAS2}/api/" "${PAYLOAD_TL}"
S_PRIMARY2="$(cat "${E2E_ARTIFACT_DIR}/c4_primary_api_status.txt")"
e2e_assert_eq "primary /api/ responds 200" "200" "${S_PRIMARY2}"

# /mcp/ alias responds
http_post_json "c4_alias_mcp" "http://127.0.0.1:${PORT_ALIAS2}/mcp/" "${PAYLOAD_TL}"
S_ALIAS2="$(cat "${E2E_ARTIFACT_DIR}/c4_alias_mcp_status.txt")"
e2e_assert_eq "alias /mcp/ responds 200" "200" "${S_ALIAS2}"

C_PRIMARY2="$(count_tools_in_response "${E2E_ARTIFACT_DIR}/c4_primary_api_body.json")"
C_ALIAS2="$(count_tools_in_response "${E2E_ARTIFACT_DIR}/c4_alias_mcp_body.json")"
e2e_assert_eq "reverse alias tool count matches (${C_PRIMARY2}=${C_ALIAS2})" "${C_PRIMARY2}" "${C_ALIAS2}"

stop_server "${PID_ALIAS2}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 5: --transport flag sets correct base path
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "transport_flag_sets_base_path"

WORK5="$(e2e_mktemp "e2e_parity_transport")"
DB5="${WORK5}/db.sqlite3"
STORAGE5="${WORK5}/storage"
mkdir -p "${STORAGE5}"

PORT5="$(pick_port)"

# Start with --transport api (should set /api/ path)
LOG5="${E2E_ARTIFACT_DIR}/server_transport_api.log"
START5_MS="$(_e2e_now_ms)"
START5_CMD="env DATABASE_URL=sqlite:////${DB5} STORAGE_ROOT=${STORAGE5} HTTP_HOST=127.0.0.1 HTTP_PORT=${PORT5} HTTP_RBAC_ENABLED=0 HTTP_RATE_LIMIT_ENABLED=0 HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 timeout 15s ${BIN} serve --host 127.0.0.1 --port ${PORT5} --transport api --no-tui"
(
    export DATABASE_URL="sqlite:////${DB5}"
    export STORAGE_ROOT="${STORAGE5}"
    export HTTP_HOST="127.0.0.1"
    export HTTP_PORT="${PORT5}"
    export HTTP_RBAC_ENABLED=0
    export HTTP_RATE_LIMIT_ENABLED=0
    export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1
    timeout 15s "${BIN}" serve --host 127.0.0.1 --port "${PORT5}" --transport api --no-tui
) >"${LOG5}" 2>&1 &
PID5=$!
startup_write_start_artifacts "transport_api" "${START5_MS}" "${PID5}" "${LOG5}" "headless_manual" "${START5_CMD}"

wait_for_server_start_or_fail "transport_api" "${PID5}" "${PORT5}" "transport api server failed"

# /api/ should respond
http_post_json "c5_transport_api" "http://127.0.0.1:${PORT5}/api/" "${PAYLOAD_TL}"
S5_API="$(cat "${E2E_ARTIFACT_DIR}/c5_transport_api_status.txt")"
e2e_assert_eq "--transport api: /api/ responds 200" "200" "${S5_API}"

# Banner should show /api/ path
e2e_assert_file_contains "banner shows /api/ path" "${LOG5}" "/api/"

stop_server "${PID5}"
sleep 0.3

# Now test --transport mcp
PORT5M="$(pick_port)"
DB5M="${WORK5}/db_mcp.sqlite3"
LOG5M="${E2E_ARTIFACT_DIR}/server_transport_mcp.log"
START5M_MS="$(_e2e_now_ms)"
START5M_CMD="env DATABASE_URL=sqlite:////${DB5M} STORAGE_ROOT=${STORAGE5} HTTP_HOST=127.0.0.1 HTTP_PORT=${PORT5M} HTTP_RBAC_ENABLED=0 HTTP_RATE_LIMIT_ENABLED=0 HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 timeout 15s ${BIN} serve --host 127.0.0.1 --port ${PORT5M} --transport mcp --no-tui"
(
    export DATABASE_URL="sqlite:////${DB5M}"
    export STORAGE_ROOT="${STORAGE5}"
    export HTTP_HOST="127.0.0.1"
    export HTTP_PORT="${PORT5M}"
    export HTTP_RBAC_ENABLED=0
    export HTTP_RATE_LIMIT_ENABLED=0
    export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1
    timeout 15s "${BIN}" serve --host 127.0.0.1 --port "${PORT5M}" --transport mcp --no-tui
) >"${LOG5M}" 2>&1 &
PID5M=$!
startup_write_start_artifacts "transport_mcp" "${START5M_MS}" "${PID5M}" "${LOG5M}" "headless_manual" "${START5M_CMD}"

wait_for_server_start_or_fail "transport_mcp" "${PID5M}" "${PORT5M}" "transport mcp server failed"

http_post_json "c5_transport_mcp" "http://127.0.0.1:${PORT5M}/mcp/" "${PAYLOAD_TL}"
S5_MCP="$(cat "${E2E_ARTIFACT_DIR}/c5_transport_mcp_status.txt")"
e2e_assert_eq "--transport mcp: /mcp/ responds 200" "200" "${S5_MCP}"

e2e_assert_file_contains "banner shows /mcp/ path" "${LOG5M}" "/mcp/"

stop_server "${PID5M}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 6: --path override takes precedence over --transport
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "path_override_trumps_transport"

WORK6="$(e2e_mktemp "e2e_parity_path_override")"
DB6="${WORK6}/db.sqlite3"
STORAGE6="${WORK6}/storage"
mkdir -p "${STORAGE6}"

PORT6="$(pick_port)"
LOG6="${E2E_ARTIFACT_DIR}/server_path_override.log"
START6_MS="$(_e2e_now_ms)"
START6_CMD="env DATABASE_URL=sqlite:////${DB6} STORAGE_ROOT=${STORAGE6} HTTP_HOST=127.0.0.1 HTTP_PORT=${PORT6} HTTP_RBAC_ENABLED=0 HTTP_RATE_LIMIT_ENABLED=0 HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 timeout 15s ${BIN} serve --host 127.0.0.1 --port ${PORT6} --transport mcp --path /custom/ --no-tui"
(
    export DATABASE_URL="sqlite:////${DB6}"
    export STORAGE_ROOT="${STORAGE6}"
    export HTTP_HOST="127.0.0.1"
    export HTTP_PORT="${PORT6}"
    export HTTP_RBAC_ENABLED=0
    export HTTP_RATE_LIMIT_ENABLED=0
    export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1
    timeout 15s "${BIN}" serve --host 127.0.0.1 --port "${PORT6}" --transport mcp --path /custom/ --no-tui
) >"${LOG6}" 2>&1 &
PID6=$!
startup_write_start_artifacts "path_override" "${START6_MS}" "${PID6}" "${LOG6}" "headless_manual" "${START6_CMD}"

wait_for_server_start_or_fail "path_override" "${PID6}" "${PORT6}" "path override server failed"

# /custom/ should respond
http_post_json "c6_custom_path" "http://127.0.0.1:${PORT6}/custom/" "${PAYLOAD_TL}"
S6_CUSTOM="$(cat "${E2E_ARTIFACT_DIR}/c6_custom_path_status.txt")"
e2e_assert_eq "--path /custom/ responds 200" "200" "${S6_CUSTOM}"

C6_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c6_custom_path_body.json")"
e2e_assert_eq "--path /custom/ has result" "1" "${C6_OK}"

# Banner should show /custom/ (not /mcp/ despite --transport mcp)
e2e_assert_file_contains "banner shows /custom/ path" "${LOG6}" "/custom/"

stop_server "${PID6}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 7: health endpoints respond regardless of mode
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "health_endpoints_mode_independent"

WORK7="$(e2e_mktemp "e2e_parity_health")"
DB7="${WORK7}/db.sqlite3"
STORAGE7="${WORK7}/storage"
mkdir -p "${STORAGE7}"

PORT7="$(pick_port)"

PID7="$(start_server "health_test" "${PORT7}" "${DB7}" "${STORAGE7}" "${BIN}" "HTTP_PATH=/api/")"
wait_for_server_start_or_fail "health_test" "${PID7}" "${PORT7}" "health test server failed"

# /health/liveness
if http_request "c7_health_liveness" "GET" "http://127.0.0.1:${PORT7}/health/liveness"; then
    e2e_assert_eq "liveness HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/c7_health_liveness_status.txt")"
    e2e_assert_contains "liveness has alive" "$(cat "${E2E_ARTIFACT_DIR}/c7_health_liveness_body.txt" 2>/dev/null || true)" "alive"
else
    e2e_fail "liveness curl failed"
fi

# /health/readiness
if http_request "c7_health_readiness" "GET" "http://127.0.0.1:${PORT7}/health/readiness"; then
    e2e_assert_eq "readiness HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/c7_health_readiness_status.txt")"
    e2e_assert_contains "readiness has ready" "$(cat "${E2E_ARTIFACT_DIR}/c7_health_readiness_body.txt" 2>/dev/null || true)" "ready"
else
    e2e_fail "readiness curl failed"
fi

stop_server "${PID7}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 8: send_message + fetch_inbox parity across modes
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "message_send_fetch_parity"

WORK8="$(e2e_mktemp "e2e_parity_msg")"
DB8_MCP="${WORK8}/db_mcp.sqlite3"
DB8_API="${WORK8}/db_api.sqlite3"
STORAGE8_MCP="${WORK8}/storage_mcp"
STORAGE8_API="${WORK8}/storage_api"
mkdir -p "${STORAGE8_MCP}" "${STORAGE8_API}"

PORT_MCP8="$(pick_port)"
PORT_API8="$(pick_port)"

PID_MCP8="$(start_server "msg_mcp" "${PORT_MCP8}" "${DB8_MCP}" "${STORAGE8_MCP}" "${BIN}" "HTTP_PATH=/mcp/")"
PID_API8="$(start_server "msg_api" "${PORT_API8}" "${DB8_API}" "${STORAGE8_API}" "${BIN}" "HTTP_PATH=/api/")"

wait_for_server_start_or_fail "msg_mcp" "${PID_MCP8}" "${PORT_MCP8}" "msg mcp server failed" "${PID_API8}"
wait_for_server_start_or_fail "msg_api" "${PID_API8}" "${PORT_API8}" "msg api server failed" "${PID_MCP8}"

# Setup: ensure project + register agents on both
PROJ_KEY="/tmp/e2e_parity_msg_proj"
EP8='{"human_key":"'"${PROJ_KEY}"'"}'
PAYLOAD_EP8="$(jsonrpc_tools_call_payload "ensure_project" "${EP8}")"
http_post_json "c8_mcp_ep" "http://127.0.0.1:${PORT_MCP8}/mcp/" "${PAYLOAD_EP8}"
http_post_json "c8_api_ep" "http://127.0.0.1:${PORT_API8}/api/" "${PAYLOAD_EP8}"

RA8='{"project_key":"'"${PROJ_KEY}"'","program":"e2e","model":"test","name":"RedLake"}'
PAYLOAD_RA8="$(jsonrpc_tools_call_payload "register_agent" "${RA8}")"
http_post_json "c8_mcp_ra1" "http://127.0.0.1:${PORT_MCP8}/mcp/" "${PAYLOAD_RA8}"
http_post_json "c8_api_ra1" "http://127.0.0.1:${PORT_API8}/api/" "${PAYLOAD_RA8}"

RA8B='{"project_key":"'"${PROJ_KEY}"'","program":"e2e","model":"test","name":"BluePeak"}'
PAYLOAD_RA8B="$(jsonrpc_tools_call_payload "register_agent" "${RA8B}")"
http_post_json "c8_mcp_ra2" "http://127.0.0.1:${PORT_MCP8}/mcp/" "${PAYLOAD_RA8B}"
http_post_json "c8_api_ra2" "http://127.0.0.1:${PORT_API8}/api/" "${PAYLOAD_RA8B}"

# Send message on both
SM8='{"project_key":"'"${PROJ_KEY}"'","sender_name":"RedLake","to":["BluePeak"],"subject":"parity test","body_md":"hello from parity"}'
PAYLOAD_SM8="$(jsonrpc_tools_call_payload "send_message" "${SM8}")"
http_post_json "c8_mcp_sm" "http://127.0.0.1:${PORT_MCP8}/mcp/" "${PAYLOAD_SM8}"
http_post_json "c8_api_sm" "http://127.0.0.1:${PORT_API8}/api/" "${PAYLOAD_SM8}"

MCP_SM_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c8_mcp_sm_body.json")"
API_SM_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c8_api_sm_body.json")"
e2e_assert_eq "MCP send_message has result" "1" "${MCP_SM_OK}"
e2e_assert_eq "API send_message has result" "1" "${API_SM_OK}"

# Fetch inbox on both
FI8='{"project_key":"'"${PROJ_KEY}"'","agent_name":"BluePeak","include_bodies":true}'
PAYLOAD_FI8="$(jsonrpc_tools_call_payload "fetch_inbox" "${FI8}")"
http_post_json "c8_mcp_fi" "http://127.0.0.1:${PORT_MCP8}/mcp/" "${PAYLOAD_FI8}"
http_post_json "c8_api_fi" "http://127.0.0.1:${PORT_API8}/api/" "${PAYLOAD_FI8}"

MCP_FI_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c8_mcp_fi_body.json")"
API_FI_OK="$(has_jsonrpc_result "${E2E_ARTIFACT_DIR}/c8_api_fi_body.json")"
e2e_assert_eq "MCP fetch_inbox has result" "1" "${MCP_FI_OK}"
e2e_assert_eq "API fetch_inbox has result" "1" "${API_FI_OK}"

# Both inboxes should contain the message body
MCP_FI_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/c8_mcp_fi_body.json")"
API_FI_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/c8_api_fi_body.json")"
e2e_assert_contains "MCP inbox has message" "${MCP_FI_TEXT}" "parity test"
e2e_assert_contains "API inbox has message" "${API_FI_TEXT}" "parity test"

stop_server "${PID_MCP8}"
stop_server "${PID_API8}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 9: wrong path returns 404 (not a silent mismatch)
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "wrong_path_returns_404"

WORK9="$(e2e_mktemp "e2e_parity_404")"
DB9="${WORK9}/db.sqlite3"
STORAGE9="${WORK9}/storage"
mkdir -p "${STORAGE9}"

PORT9="$(pick_port)"

PID9="$(start_server "wrong_path" "${PORT9}" "${DB9}" "${STORAGE9}" "${BIN}" "HTTP_PATH=/custom/")"
wait_for_server_start_or_fail "wrong_path" "${PID9}" "${PORT9}" "wrong path server failed"

# /badpath/ should fail (not 200)
http_post_json "c9_badpath" "http://127.0.0.1:${PORT9}/badpath/" "${PAYLOAD_TL}"
S9_BAD="$(cat "${E2E_ARTIFACT_DIR}/c9_badpath_status.txt")"
if [ "${S9_BAD}" = "404" ] || [ "${S9_BAD}" = "405" ]; then
    e2e_pass "wrong path returns ${S9_BAD} (not 200)"
else
    e2e_fail "expected 404/405 for wrong path, got ${S9_BAD}"
fi

# /custom/ should still work
http_post_json "c9_goodpath" "http://127.0.0.1:${PORT9}/custom/" "${PAYLOAD_TL}"
S9_GOOD="$(cat "${E2E_ARTIFACT_DIR}/c9_goodpath_status.txt")"
e2e_assert_eq "correct path /custom/ responds 200" "200" "${S9_GOOD}"

stop_server "${PID9}"
sleep 0.3

# ══════════════════════════════════════════════════════════════════════
# Case 10: bootstrap banner diagnostics clarity
# ══════════════════════════════════════════════════════════════════════
e2e_case_banner "bootstrap_diagnostics_clarity"

# MCP mode banner
LOG_MCP_DIAG="${E2E_ARTIFACT_DIR}/server_mcp_mode.log"
e2e_assert_file_contains "MCP banner shows host" "${LOG_MCP_DIAG}" "host:" || true
e2e_assert_file_contains "MCP banner shows mode" "${LOG_MCP_DIAG}" "mode:" || true

# API mode banner
LOG_API_DIAG="${E2E_ARTIFACT_DIR}/server_api_mode.log"
e2e_assert_file_contains "API banner shows host" "${LOG_API_DIAG}" "host:" || true
e2e_assert_file_contains "API banner shows mode" "${LOG_API_DIAG}" "mode:" || true

# ══════════════════════════════════════════════════════════════════════
e2e_summary
