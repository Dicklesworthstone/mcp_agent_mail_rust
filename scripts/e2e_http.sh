#!/usr/bin/env bash
# e2e_http.sh - Unified HTTP-focused E2E parity suite
#
# Run via:
#   ./scripts/e2e_test.sh http
#
# This suite aims to cover the end-user visible HTTP server surface in one run,
# while reusing existing focused E2E suites where appropriate.
#
# Artifacts:
#   tests/artifacts/http/<timestamp>/*

set -euo pipefail

# Safety: default to keeping temp dirs so the shared harness doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-http}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "HTTP Unified E2E Test Suite"

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
# HTTP helpers (record full transcripts)
# ---------------------------------------------------------------------------

http_request() {
    local case_id="$1"
    local method="$2"
    local url="$3"
    shift 3

    e2e_mark_case_start "${case_id}"

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

    # Save a human-readable curl invocation (no secrets embedded).
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

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local request_file="${E2E_ARTIFACT_DIR}/${case_id}_request.json"
    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.json"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local timing_file="${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"

    e2e_mark_case_start "${case_id}"

    # Use shared RPC capture helper, then mirror legacy flat filenames for
    # existing assertions in this suite.
    if ! e2e_rpc_call_raw "${case_id}" "${url}" "${payload}" "$@"; then
        :
    fi

    cp "${case_dir}/request.json" "${request_file}" 2>/dev/null || e2e_save_artifact "${case_id}_request.json" "${payload}"
    cp "${case_dir}/headers.txt" "${headers_file}" 2>/dev/null || true
    cp "${case_dir}/response.json" "${body_file}" 2>/dev/null || true
    cp "${case_dir}/status.txt" "${status_file}" 2>/dev/null || true
    cp "${case_dir}/timing.txt" "${timing_file}" 2>/dev/null || true
    cp "${case_dir}/curl_stderr.txt" "${curl_stderr_file}" 2>/dev/null || true

    local status
    status="$(cat "${status_file}" 2>/dev/null || echo "")"
    if [ -z "${status}" ] || [ "${status}" = "000" ]; then
        e2e_fatal "${case_id}: curl failed (status=${status:-missing})"
    fi
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

jsonrpc_resources_read_payload() {
    local uri="$1"
    python3 - <<'PY' "$uri"
import json, sys
print(json.dumps({
  "jsonrpc": "2.0",
  "method": "resources/read",
  "id": 1,
  "params": { "uri": sys.argv[1] },
}))
PY
}

count_tools_in_tools_list_response() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
res = data.get("result") or {}
tools = res.get("tools") or []
print(len(tools) if isinstance(tools, list) else 0)
PY
}

tools_list_contains_name() {
    local resp_file="$1"
    local tool_name="$2"
    python3 - <<'PY' "$resp_file" "$tool_name"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
res = data.get("result") or {}
tools = res.get("tools") or []
name = sys.argv[2]
ok = False
if isinstance(tools, list):
  for t in tools:
    if isinstance(t, dict) and t.get("name") == name:
      ok = True
      break
print("1" if ok else "0")
PY
}

extract_tool_text_from_tools_call() {
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

extract_resource_text_from_read_response() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
res = data.get("result") or {}
contents = res.get("contents") or []
if isinstance(contents, list) and contents:
  first = contents[0]
  if isinstance(first, dict) and isinstance(first.get("text"), str):
    print(first["text"])
    raise SystemExit(0)
print("")
PY
}

tool_directory_contains_tool_name() {
    local directory_json="$1"
    local tool_name="$2"
    python3 - <<'PY' "$directory_json" "$tool_name"
import json, sys
d = json.loads(sys.argv[1])
name = sys.argv[2]
for cluster in d.get("clusters") or []:
  tools = cluster.get("tools") if isinstance(cluster, dict) else None
  if not isinstance(tools, list):
    continue
  for t in tools:
    if isinstance(t, dict) and t.get("name") == name:
      print("1")
      raise SystemExit(0)
print("0")
PY
}

json_get_field() {
    local json_str="$1"
    local field="$2"
    python3 - <<'PY' "$json_str" "$field"
import json, sys
obj = json.loads(sys.argv[1])
print(obj.get(sys.argv[2], ""))
PY
}

# ---------------------------------------------------------------------------
# Server runner (per-config)
# ---------------------------------------------------------------------------

start_server() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5

    local server_log="${E2E_ARTIFACT_DIR}/server_${label}.log"
    e2e_log "Starting server (${label}): 127.0.0.1:${port}"
    e2e_log "  log: ${server_log}"

    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"

        # Conservative defaults; suite cases override as needed.
        export HTTP_RBAC_ENABLED="0"
        export HTTP_RATE_LIMIT_ENABLED="0"
        export HTTP_JWT_ENABLED="0"
        export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED="0"
        # Ensure the suite is hermetic even if the developer has a token in
        # ~/.mcp_agent_mail/.env (full_env_value() would otherwise pick it up).
        export HTTP_BEARER_TOKEN=""

        # Optional overrides passed as KEY=VALUE pairs in remaining args.
        while [ $# -gt 0 ]; do
            export "$1"
            shift
        done

        "${bin}" serve --host 127.0.0.1 --port "${port}"
    ) >"${server_log}" 2>&1 &
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

# ---------------------------------------------------------------------------
# Subsuite runner (copy artifacts into this suite's artifact dir)
# ---------------------------------------------------------------------------

list_artifact_runs() {
    local suite="$1"
    ls -1 "${E2E_PROJECT_ROOT}/tests/artifacts/${suite}" 2>/dev/null | sort || true
}

run_subsuite_and_copy() {
    local suite="$1"
    local script="${E2E_PROJECT_ROOT}/tests/e2e/test_${suite}.sh"
    if [ ! -f "${script}" ]; then
        e2e_fail "missing subsuite script: ${script}"
        return 1
    fi

    e2e_case_banner "Subsuite: ${suite}"

    local before after rc
    before="$(list_artifact_runs "${suite}" | tail -n 1 || true)"
    set +e
    AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP}" bash "${script}"
    rc=$?
    set -e

    after="$(list_artifact_runs "${suite}" | tail -n 1 || true)"
    if [ -n "${after}" ] && [ "${after}" != "${before}" ]; then
        e2e_copy_artifact "${E2E_PROJECT_ROOT}/tests/artifacts/${suite}/${after}" "subsuite/${suite}/${after}"
        e2e_pass "copied subsuite artifacts: ${suite}/${after}"
    else
        e2e_log "no new subsuite artifact dir detected for ${suite} (before='${before}' after='${after}')"
    fi

    if [ "${rc}" -ne 0 ]; then
        e2e_fail "subsuite failed: ${suite} (rc=${rc})"
        return 1
    fi
    e2e_pass "subsuite passed: ${suite}"
    return 0
}

# ---------------------------------------------------------------------------
# Build binary (once)
# ---------------------------------------------------------------------------

# e2e_ensure_binary is verbose (logs to stdout); take the last line as the path.
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# ---------------------------------------------------------------------------
# Run 1: health + bearer auth + OPTIONS bypass + CORS + well-known
# ---------------------------------------------------------------------------

e2e_banner "Run 1: health + bearer auth + OPTIONS bypass + CORS + well-known"

WORK1="$(e2e_mktemp "e2e_http_run1")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage_root"
PORT1="$(pick_port)"

TOKEN="e2e-token"
URL_BASE="http://127.0.0.1:${PORT1}"
API_URL="${URL_BASE}/api/"

PID1="$(start_server "run1" "${PORT1}" "${DB1}" "${STORAGE1}" "${BIN}" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_CORS_ENABLED=1" \
    "HTTP_CORS_ORIGINS=*" \
    "HTTP_CORS_ALLOW_CREDENTIALS=0" \
    "HTTP_CORS_ALLOW_METHODS=GET,POST,OPTIONS" \
    "HTTP_CORS_ALLOW_HEADERS=Content-Type,Authorization" \
)"
trap 'stop_server "${PID1}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT1}" 10; then
    e2e_fatal "server run1 failed to start (port not open)"
fi

AUTHZ="Authorization: Bearer ${TOKEN}"

e2e_case_banner "GET /health/liveness bypasses auth"
http_request "run1_health_liveness" "GET" "${URL_BASE}/health/liveness"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run1_health_liveness_status.txt")"
e2e_assert_contains "body has alive" "$(cat "${E2E_ARTIFACT_DIR}/run1_health_liveness_body.txt" 2>/dev/null || true)" "alive"

e2e_case_banner "GET /health/readiness bypasses auth"
http_request "run1_health_readiness" "GET" "${URL_BASE}/health/readiness"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run1_health_readiness_status.txt")"
e2e_assert_contains "body has ready" "$(cat "${E2E_ARTIFACT_DIR}/run1_health_readiness_body.txt" 2>/dev/null || true)" "ready"

e2e_case_banner "GET /health bypasses auth (alias)"
http_request "run1_health_root" "GET" "${URL_BASE}/health"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run1_health_root_status.txt")"
e2e_assert_contains "body has ready" "$(cat "${E2E_ARTIFACT_DIR}/run1_health_root_body.txt" 2>/dev/null || true)" "ready"

e2e_case_banner "GET /healthz bypasses auth (alias)"
http_request "run1_healthz" "GET" "${URL_BASE}/healthz"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run1_healthz_status.txt")"
e2e_assert_contains "body has alive" "$(cat "${E2E_ARTIFACT_DIR}/run1_healthz_body.txt" 2>/dev/null || true)" "alive"

e2e_case_banner "OPTIONS /api/ bypasses auth and includes CORS headers"
http_request "run1_options_api" "OPTIONS" "${API_URL}" \
    "Origin: https://example.test" \
    "Access-Control-Request-Method: POST" \
    "Access-Control-Request-Headers: content-type,authorization"
S_OPT="$(cat "${E2E_ARTIFACT_DIR}/run1_options_api_status.txt")"
if [ "${S_OPT}" = "200" ] || [ "${S_OPT}" = "204" ]; then
    e2e_pass "HTTP ${S_OPT}"
else
    e2e_fail "expected 200 or 204 (got ${S_OPT})"
fi
e2e_assert_contains "has access-control-allow-origin" "$(cat "${E2E_ARTIFACT_DIR}/run1_options_api_headers.txt" 2>/dev/null || true)" "access-control-allow-origin"

e2e_case_banner "POST /api/ without Authorization returns 401"
PAYLOAD_HC="$(jsonrpc_tools_call_payload "health_check" "{}")"
http_post_json "run1_post_missing_auth" "${API_URL}" "${PAYLOAD_HC}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/run1_post_missing_auth_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/run1_post_missing_auth_body.json" 2>/dev/null || true)" "Unauthorized"

e2e_case_banner "POST /api/ with Authorization succeeds"
http_post_json "run1_post_with_auth" "${API_URL}" "${PAYLOAD_HC}" "${AUTHZ}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run1_post_with_auth_status.txt")"
e2e_assert_contains "jsonrpc result" "$(cat "${E2E_ARTIFACT_DIR}/run1_post_with_auth_body.json" 2>/dev/null || true)" "\"result\""

e2e_case_banner "Well-known endpoints require auth and return expected JSON"
http_request "run1_wk_no_auth" "GET" "${URL_BASE}/.well-known/oauth-authorization-server"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/run1_wk_no_auth_status.txt")"

http_request "run1_wk_with_auth" "GET" "${URL_BASE}/.well-known/oauth-authorization-server" "${AUTHZ}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run1_wk_with_auth_status.txt")"
e2e_assert_contains "body has mcp_oauth false" "$(cat "${E2E_ARTIFACT_DIR}/run1_wk_with_auth_body.txt" 2>/dev/null || true)" "\"mcp_oauth\""

stop_server "${PID1}"
trap - EXIT

# ---------------------------------------------------------------------------
# Run 2: request logging + OTEL no-op (misconfig must not crash)
# ---------------------------------------------------------------------------

e2e_banner "Run 2: request logging + OTEL no-op (misconfig)"

WORK2="$(e2e_mktemp "e2e_http_run2")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage_root"
PORT2="$(pick_port)"
URL2="http://127.0.0.1:${PORT2}"

PID2="$(start_server "run2" "${PORT2}" "${DB2}" "${STORAGE2}" "${BIN}" \
    "HTTP_REQUEST_LOG_ENABLED=1" \
    "LOG_JSON_ENABLED=0" \
    "HTTP_OTEL_ENABLED=1" \
    "OTEL_SERVICE_NAME=e2e" \
    "OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:1" \
)"
trap 'stop_server "${PID2}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    e2e_fatal "server run2 failed to start (port not open)"
fi

http_request "run2_health_liveness" "GET" "${URL2}/health/liveness"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run2_health_liveness_status.txt")"

SERVER2_LOG="${E2E_ARTIFACT_DIR}/server_run2.log"
if grep -F -q "event='request'" "${SERVER2_LOG}" 2>/dev/null; then
    e2e_pass "request log emitted (kv)"
else
    e2e_fail "expected request log line (missing event='request')"
fi

stop_server "${PID2}"
trap - EXIT

# ---------------------------------------------------------------------------
# Run 3: tool filtering (baseline vs minimal vs custom exclude)
# ---------------------------------------------------------------------------

e2e_banner "Run 3: tool filtering (baseline/minimal/custom)"

run_tools_list() {
    local case_id="$1"
    local api_url="$2"
    http_post_json "${case_id}" "${api_url}" '{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}'
    e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/${case_id}_status.txt")"
}

WORK3A="$(e2e_mktemp "e2e_http_run3a")"
DB3A="${WORK3A}/db.sqlite3"
STORAGE3A="${WORK3A}/storage_root"
PORT3A="$(pick_port)"
URL3A="http://127.0.0.1:${PORT3A}/api/"

PID3A="$(start_server "run3_full" "${PORT3A}" "${DB3A}" "${STORAGE3A}" "${BIN}" \
    "TOOLS_FILTER_ENABLED=0" \
)"
trap 'stop_server "${PID3A}" || true' EXIT
e2e_wait_port 127.0.0.1 "${PORT3A}" 10 || e2e_fatal "server run3_full failed to start"

e2e_case_banner "tools/list baseline (filter disabled)"
run_tools_list "run3_full_tools_list" "${URL3A}"
FULL_COUNT="$(count_tools_in_tools_list_response "${E2E_ARTIFACT_DIR}/run3_full_tools_list_body.json")"
e2e_save_artifact "run3_full_tool_count.txt" "${FULL_COUNT}"
e2e_pass "baseline tool count=${FULL_COUNT}"

stop_server "${PID3A}"
trap - EXIT

WORK3B="$(e2e_mktemp "e2e_http_run3b")"
DB3B="${WORK3B}/db.sqlite3"
STORAGE3B="${WORK3B}/storage_root"
PORT3B="$(pick_port)"
URL3B="http://127.0.0.1:${PORT3B}/api/"

PID3B="$(start_server "run3_minimal" "${PORT3B}" "${DB3B}" "${STORAGE3B}" "${BIN}" \
    "TOOLS_FILTER_ENABLED=1" \
    "TOOLS_FILTER_PROFILE=minimal" \
)"
trap 'stop_server "${PID3B}" || true' EXIT
e2e_wait_port 127.0.0.1 "${PORT3B}" 10 || e2e_fatal "server run3_minimal failed to start"

e2e_case_banner "tools/list minimal profile returns fewer tools"
run_tools_list "run3_min_tools_list" "${URL3B}"
MIN_COUNT="$(count_tools_in_tools_list_response "${E2E_ARTIFACT_DIR}/run3_min_tools_list_body.json")"
e2e_save_artifact "run3_min_tool_count.txt" "${MIN_COUNT}"
if [ "${MIN_COUNT}" -lt "${FULL_COUNT}" ]; then
    e2e_pass "minimal tool count=${MIN_COUNT} (< ${FULL_COUNT})"
else
    e2e_fail "expected minimal tool count < full (min=${MIN_COUNT} full=${FULL_COUNT})"
fi

e2e_case_banner "resource://tooling/directory reflects filtered set (minimal)"
PAYLOAD_DIR="$(jsonrpc_resources_read_payload "resource://tooling/directory")"
http_post_json "run3_min_tooling_dir" "${URL3B}" "${PAYLOAD_DIR}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run3_min_tooling_dir_status.txt")"
e2e_assert_contains "tooling directory returned" "$(cat "${E2E_ARTIFACT_DIR}/run3_min_tooling_dir_body.json" 2>/dev/null || true)" "\"contents\""

stop_server "${PID3B}"
trap - EXIT

WORK3C="$(e2e_mktemp "e2e_http_run3c")"
DB3C="${WORK3C}/db.sqlite3"
STORAGE3C="${WORK3C}/storage_root"
PORT3C="$(pick_port)"
URL3C="http://127.0.0.1:${PORT3C}/api/"

PID3C="$(start_server "run3_custom" "${PORT3C}" "${DB3C}" "${STORAGE3C}" "${BIN}" \
    "TOOLS_FILTER_ENABLED=1" \
    "TOOLS_FILTER_PROFILE=custom" \
    "TOOLS_FILTER_MODE=exclude" \
    "TOOLS_FILTER_TOOLS=health_check" \
)"
trap 'stop_server "${PID3C}" || true' EXIT
e2e_wait_port 127.0.0.1 "${PORT3C}" 10 || e2e_fatal "server run3_custom failed to start"

e2e_case_banner "custom exclude removes health_check tool"
run_tools_list "run3_custom_tools_list" "${URL3C}"
HAS_HC="$(tools_list_contains_name "${E2E_ARTIFACT_DIR}/run3_custom_tools_list_body.json" "health_check")"
if [ "${HAS_HC}" = "0" ]; then
    e2e_pass "health_check is excluded"
else
    e2e_fail "expected health_check excluded"
fi

PAYLOAD_DIR2="$(jsonrpc_resources_read_payload "resource://tooling/directory")"
http_post_json "run3_custom_tooling_dir" "${URL3C}" "${PAYLOAD_DIR2}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run3_custom_tooling_dir_status.txt")"
DIR_CUSTOM="$(extract_resource_text_from_read_response "${E2E_ARTIFACT_DIR}/run3_custom_tooling_dir_body.json")"
e2e_save_artifact "run3_custom_tooling_directory.json" "${DIR_CUSTOM}"
HAS_HC_DIR="$(tool_directory_contains_tool_name "${DIR_CUSTOM}" "health_check")"
if [ "${HAS_HC_DIR}" = "0" ]; then
    e2e_pass "tooling directory does not list excluded health_check"
else
    e2e_fail "expected tooling directory to exclude health_check tool entry"
fi

stop_server "${PID3C}"
trap - EXIT

# ---------------------------------------------------------------------------
# Run 4: instrumentation emits tool_query_stats
# ---------------------------------------------------------------------------

e2e_banner "Run 4: instrumentation emits tool_query_stats"

WORK4="$(e2e_mktemp "e2e_http_run4")"
DB4="${WORK4}/db.sqlite3"
STORAGE4="${WORK4}/storage_root"
PROJECT_DIR4="${WORK4}/proj"
mkdir -p "${PROJECT_DIR4}"
PORT4="$(pick_port)"
URL4="http://127.0.0.1:${PORT4}"
API4="${URL4}/api/"

PID4="$(start_server "run4" "${PORT4}" "${DB4}" "${STORAGE4}" "${BIN}" \
    "INSTRUMENTATION_ENABLED=1" \
    "INSTRUMENTATION_SLOW_QUERY_MS=0" \
    "LOG_JSON_ENABLED=0" \
)"
trap 'stop_server "${PID4}" || true' EXIT
e2e_wait_port 127.0.0.1 "${PORT4}" 10 || e2e_fatal "server run4 failed to start"

e2e_case_banner "ensure_project triggers DB-backed tool call"
PAYLOAD_EP="$(jsonrpc_tools_call_payload "ensure_project" "$(python3 -c "import json,sys; print(json.dumps({'human_key': sys.argv[1]}))" "${PROJECT_DIR4}")")"
http_post_json "run4_ensure_project" "${API4}" "${PAYLOAD_EP}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run4_ensure_project_status.txt")"

stop_server "${PID4}"
trap - EXIT

SERVER4_LOG="${E2E_ARTIFACT_DIR}/server_run4.log"
if grep -F -q "tool_query_stats" "${SERVER4_LOG}" 2>/dev/null; then
    e2e_pass "instrumentation emitted tool_query_stats"
else
    e2e_fail "expected tool_query_stats in logs"
fi

# ---------------------------------------------------------------------------
# Run 5: ACK TTL worker logs ack_overdue and (optional) escalates to file reservation
# ---------------------------------------------------------------------------

e2e_banner "Run 5: ACK TTL worker logs ack_overdue + escalation smoke"

WORK5="$(e2e_mktemp "e2e_http_run5")"
DB5="${WORK5}/db.sqlite3"
STORAGE5="${WORK5}/storage_root"
PROJECT_DIR5="${WORK5}/proj"
mkdir -p "${PROJECT_DIR5}"
PORT5="$(pick_port)"
URL5="http://127.0.0.1:${PORT5}"
API5="${URL5}/api/"

PID5="$(start_server "run5" "${PORT5}" "${DB5}" "${STORAGE5}" "${BIN}" \
    "ACK_TTL_ENABLED=1" \
    "ACK_TTL_SECONDS=0" \
    "ACK_TTL_SCAN_INTERVAL_SECONDS=1" \
    "ACK_ESCALATION_ENABLED=1" \
    "ACK_ESCALATION_MODE=file_reservation" \
    "ACK_ESCALATION_CLAIM_TTL_SECONDS=60" \
    "ACK_ESCALATION_CLAIM_EXCLUSIVE=1" \
    "ACK_ESCALATION_CLAIM_HOLDER_NAME=AckBot" \
)"
trap 'stop_server "${PID5}" || true' EXIT
e2e_wait_port 127.0.0.1 "${PORT5}" 10 || e2e_fatal "server run5 failed to start"

e2e_case_banner "Create ack_required message"
PAYLOAD_PROJ="$(jsonrpc_tools_call_payload "ensure_project" "$(python3 -c "import json,sys; print(json.dumps({'human_key': sys.argv[1]}))" "${PROJECT_DIR5}")")"
http_post_json "run5_ensure_project" "${API5}" "${PAYLOAD_PROJ}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run5_ensure_project_status.txt")"
PROJECT_JSON5="$(extract_tool_text_from_tools_call "${E2E_ARTIFACT_DIR}/run5_ensure_project_body.json")"
PROJECT_SLUG5="$(json_get_field "${PROJECT_JSON5}" "slug")"
e2e_save_artifact "run5_project.json" "${PROJECT_JSON5}"

PAYLOAD_REG_SENDER="$(jsonrpc_tools_call_payload "register_agent" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'program':'e2e','model':'test','name':'BlueLake','task_description':'e2e'}))" "${PROJECT_DIR5}")")"
http_post_json "run5_register_sender" "${API5}" "${PAYLOAD_REG_SENDER}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run5_register_sender_status.txt")"
e2e_assert_contains "register_agent sender returns JSON-RPC result" "$(cat "${E2E_ARTIFACT_DIR}/run5_register_sender_body.json" 2>/dev/null || true)" "\"result\""

PAYLOAD_REG_RECIP="$(jsonrpc_tools_call_payload "register_agent" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'program':'e2e','model':'test','name':'GreenCastle','task_description':'e2e'}))" "${PROJECT_DIR5}")")"
http_post_json "run5_register_recipient" "${API5}" "${PAYLOAD_REG_RECIP}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run5_register_recipient_status.txt")"
e2e_assert_contains "register_agent recipient returns JSON-RPC result" "$(cat "${E2E_ARTIFACT_DIR}/run5_register_recipient_body.json" 2>/dev/null || true)" "\"result\""

PAYLOAD_SEND="$(jsonrpc_tools_call_payload "send_message" "$(
python3 - <<PY "${PROJECT_DIR5}"
import json,sys
proj=sys.argv[1]
print(json.dumps({
  "project_key": proj,
  "sender_name": "BlueLake",
  "to": ["GreenCastle"],
  "subject": "ack ttl e2e",
  "body_md": "hello",
  "ack_required": True,
  "thread_id": "br-2ei.9.6",
}))
PY
)")"
http_post_json "run5_send_ack_required" "${API5}" "${PAYLOAD_SEND}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run5_send_ack_required_status.txt")"
e2e_assert_contains "send_message returns JSON-RPC result" "$(cat "${E2E_ARTIFACT_DIR}/run5_send_ack_required_body.json" 2>/dev/null || true)" "\"result\""

e2e_case_banner "Wait for ack_overdue log"
SERVER5_LOG="${E2E_ARTIFACT_DIR}/server_run5.log"
deadline=$(( $(date +%s) + 10 ))
found=0
while [ "$(date +%s)" -lt "$deadline" ]; do
    if grep -F -q "ack_overdue" "${SERVER5_LOG}" 2>/dev/null; then
        found=1
        break
    fi
    sleep 0.2
done
if [ "${found}" = "1" ]; then
    e2e_pass "ack_overdue observed in logs"
else
    e2e_fail "expected ack_overdue in logs"
fi

e2e_case_banner "Escalation creates a file reservation (smoke via resource read)"
if [ -n "${PROJECT_SLUG5}" ]; then
    PAYLOAD_LOCKS="$(jsonrpc_resources_read_payload "resource://file_reservations/${PROJECT_SLUG5}?active_only=true")"
    http_post_json "run5_file_reservations" "${API5}" "${PAYLOAD_LOCKS}"
    e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/run5_file_reservations_status.txt")"
    LOCKS_JSON="$(extract_resource_text_from_read_response "${E2E_ARTIFACT_DIR}/run5_file_reservations_body.json")"
    e2e_save_artifact "run5_file_reservations.json" "${LOCKS_JSON}"
    RES_COUNT="$(python3 - <<'PY' "$LOCKS_JSON"
import json, sys
try:
  data = json.loads(sys.argv[1])
except Exception:
  print("0")
  raise SystemExit(0)
print(len(data) if isinstance(data, list) else 0)
PY
)"
    if [ "${RES_COUNT}" -gt 0 ]; then
        e2e_pass "ack escalation created file reservation(s): count=${RES_COUNT}"
    else
        e2e_fail "expected ack escalation to create at least 1 file reservation (count=${RES_COUNT})"
    fi
else
    e2e_skip "project slug missing; skipping reservation resource read"
fi

stop_server "${PID5}"
trap - EXIT

# ---------------------------------------------------------------------------
# Focused subsuites (copied into this artifact dir)
# ---------------------------------------------------------------------------

if [ "${AM_E2E_HTTP_INCLUDE_FOCUSED_SUBSUITES:-1}" = "1" ]; then
    e2e_banner "Focused subsuites (copied)"

    for suite in jwt rate_limit peer_addr mail_ui http_streamable; do
        run_subsuite_and_copy "${suite}" || e2e_fatal "subsuite failed: ${suite}"
    done
else
    e2e_case_banner "Focused subsuites disabled"
    e2e_skip "focused subsuites disabled via AM_E2E_HTTP_INCLUDE_FOCUSED_SUBSUITES=0"
fi

e2e_summary
