#!/usr/bin/env bash
# test_http_streamable.sh - E2E test suite for stateless Streamable HTTP parity
#
# Covers (br-1bm.4.6):
# - Base path mount semantics: POST /api and /api/ both work (no redirect)
# - Passthrough parity: POST /api behaves identically to POST /api/
# - Notification semantics: notification returns 202 Accepted with empty body
# - Header normalization: missing Content-Type is tolerated (server injects application/json)
#
# Artifacts:
# - Server logs: tests/artifacts/http_streamable/<timestamp>/logs/server_*.log
# - Per-case directories: <case_id>/{request,response,headers,status,timing}.*
# - Raw HTTP transcripts for missing Content-Type case

set -euo pipefail

# Safety: this repo forbids destructive cleanup by default. Keep tmp unless caller opts out.
AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP:-1}"

E2E_SUITE="http_streamable"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "HTTP Streamable (Stateless JSON) E2E Test Suite"

if ! command -v curl >/dev/null 2>&1; then
    e2e_log "curl not found; skipping suite"
    e2e_skip "curl required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

fail_fast_if_needed() {
    if [ "${_E2E_FAIL}" -gt 0 ]; then
        e2e_log "Fail-fast: exiting after first failure"
        e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
        e2e_summary || true
        exit 1
    fi
}

http_post_json() {
    local case_id="$1"
    local url="$2"
    local payload="$3"
    shift 3
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local flat_curl_args="${E2E_ARTIFACT_DIR}/${case_id}_curl_args.txt"

    if ! e2e_rpc_call_raw "${case_id}" "${url}" "${payload}" "$@"; then
        :
    fi

    # Preserve legacy flat artifact path by mirroring helper-generated args.
    if [ -f "${case_dir}/curl_args.txt" ]; then
        cp "${case_dir}/curl_args.txt" "${flat_curl_args}" 2>/dev/null || true
    else
        e2e_save_artifact "${case_id}_curl_args.txt" "curl -X POST ${url} -H 'content-type: application/json' --data '<payload>'"
    fi

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
    if [ -z "${status}" ] || [ "${status}" = "000" ]; then
        e2e_fail "${case_id}: curl failed (status=${status:-missing})"
        return 1
    fi
    return 0
}

raw_http_post_missing_content_type() {
    local case_id="$1"
    local host="$2"
    local port="$3"
    local path="$4"
    local payload="$5"

    local out_json="${E2E_ARTIFACT_DIR}/${case_id}_raw_result.json"
    local request_txt="${E2E_ARTIFACT_DIR}/${case_id}_raw_request.txt"
    local response_txt="${E2E_ARTIFACT_DIR}/${case_id}_raw_response.txt"

    python3 - <<'PY' "$host" "$port" "$path" "$payload" "$out_json" "$request_txt" "$response_txt"
import json
import socket
import sys

host = sys.argv[1]
port = int(sys.argv[2])
path = sys.argv[3]
payload = sys.argv[4].encode("utf-8")
out_json = sys.argv[5]
req_path = sys.argv[6]
resp_path = sys.argv[7]

req = (
    f"POST {path} HTTP/1.1\r\n"
    f"Host: {host}:{port}\r\n"
    f"Content-Length: {len(payload)}\r\n"
    f"Connection: close\r\n"
    f"\r\n"
).encode("ascii") + payload

with open(req_path, "wb") as f:
    f.write(req)

sock = socket.create_connection((host, port), timeout=5)
sock.sendall(req)

chunks = []
recv_calls = 0
while True:
    b = sock.recv(16)  # tiny buffer: should observe multiple reads for any non-trivial response
    if not b:
        break
    recv_calls += 1
    chunks.append(b)
sock.close()

raw = b"".join(chunks)

with open(resp_path, "wb") as f:
    f.write(raw)

text = raw.decode("latin1", errors="replace")
lines = text.split("\r\n")
status_line = lines[0] if lines else ""
status = 0
try:
    status = int(status_line.split(" ", 2)[1])
except Exception:
    status = 0

headers = {}
i = 1
while i < len(lines):
    line = lines[i]
    i += 1
    if line == "":
        break
    if ":" in line:
        k, v = line.split(":", 1)
        headers[k.strip().lower()] = v.strip()

body = "\r\n".join(lines[i:])
result = {
    "status": status,
    "recv_calls": recv_calls,
    "headers": headers,
    "body_preview": body[:200],
}
with open(out_json, "w", encoding="utf-8") as f:
    json.dump(result, f, indent=2, sort_keys=True)
print(status)
PY
}

json_canonical_sha() {
    local input_file="$1"
    python3 - <<'PY' "${input_file}"
import hashlib
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    obj = json.load(f)

canonical = json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
print(hashlib.sha256(canonical).hexdigest())
PY
}

case_headers() {
    local case_id="$1"
    cat "${E2E_ARTIFACT_DIR}/${case_id}/headers.txt" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Setup: temp workspace + server
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_http_streamable")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
TOKEN="e2e-token"

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "http_streamable" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "HTTP_JWT_ENABLED=0"; then
    e2e_fail "server failed to start"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# e2e_start_server_with_logs sets /mcp/ by default; this suite targets /api and /api/.
URL_BASE="${E2E_SERVER_URL%/mcp/}/api"
URL_SLASH="${URL_BASE}/"
PORT="${URL_BASE#http://127.0.0.1:}"
PORT="${PORT%%/*}"
TOOLS_LIST='{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}'
NOTIF='{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}'

# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------

e2e_case_banner "POST /api/ tools/list returns JSON-RPC result"
http_post_json "case1_post_slash" "${URL_SLASH}" "${TOOLS_LIST}"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case1_post_slash")"
e2e_assert_contains "content-type application/json" "$(case_headers "case1_post_slash")" "application/json"
e2e_assert_contains "response contains result" "$(e2e_rpc_read_response "case1_post_slash")" "\"result\""
fail_fast_if_needed

e2e_case_banner "POST /api tools/list works (no redirect) and matches /api/"
http_post_json "case2_post_base" "${URL_BASE}" "${TOOLS_LIST}"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case2_post_base")"
e2e_assert_contains "content-type application/json" "$(case_headers "case2_post_base")" "application/json"

SHA_BASE="$(json_canonical_sha "${E2E_ARTIFACT_DIR}/case2_post_base/response.json")"
SHA_SLASH="$(json_canonical_sha "${E2E_ARTIFACT_DIR}/case1_post_slash/response.json")"
e2e_assert_eq "canonical JSON sha256 matches" "${SHA_SLASH}" "${SHA_BASE}"
fail_fast_if_needed

e2e_case_banner "Notification returns 202 Accepted with empty body"
http_post_json "case3_notification" "${URL_BASE}" "${NOTIF}"
e2e_assert_eq "HTTP 202" "202" "$(e2e_rpc_read_status "case3_notification")"
e2e_assert_contains "content-type application/json" "$(case_headers "case3_notification")" "application/json"
BODY_SZ="$(stat --format='%s' "${E2E_ARTIFACT_DIR}/case3_notification/response.json" 2>/dev/null || stat -f '%z' "${E2E_ARTIFACT_DIR}/case3_notification/response.json" 2>/dev/null || echo "?")"
e2e_assert_eq "notification body is empty" "0" "${BODY_SZ}"
fail_fast_if_needed

e2e_case_banner "POST missing Content-Type header is tolerated (raw socket)"
RAW_STATUS="$(raw_http_post_missing_content_type "case4_missing_content_type" "127.0.0.1" "${PORT}" "/api/" "${TOOLS_LIST}")"
e2e_assert_eq "HTTP 200" "200" "${RAW_STATUS}"
e2e_assert_file_exists "raw request saved" "${E2E_ARTIFACT_DIR}/case4_missing_content_type_raw_request.txt"
e2e_assert_file_exists "raw response saved" "${E2E_ARTIFACT_DIR}/case4_missing_content_type_raw_response.txt"
e2e_assert_file_exists "raw result saved" "${E2E_ARTIFACT_DIR}/case4_missing_content_type_raw_result.json"

if grep -qi "^content-type:" "${E2E_ARTIFACT_DIR}/case4_missing_content_type_raw_request.txt" 2>/dev/null; then
    e2e_fail "raw request must omit Content-Type header"
else
    e2e_pass "raw request omits Content-Type header"
fi

RAW_RESULT_BODY="$(cat "${E2E_ARTIFACT_DIR}/case4_missing_content_type_raw_result.json" 2>/dev/null || true)"
e2e_assert_contains "raw response body preview contains result" "${RAW_RESULT_BODY}" "result"
e2e_assert_contains "raw response captured recv_calls" "${RAW_RESULT_BODY}" "\"recv_calls\""

RECV_CALLS="$(
python3 - <<'PY' "${E2E_ARTIFACT_DIR}/case4_missing_content_type_raw_result.json"
import json,sys
v=json.load(open(sys.argv[1], "r", encoding="utf-8"))
print(int(v.get("recv_calls", 0)))
PY
)"
if [ "${RECV_CALLS}" -ge 2 ]; then
    e2e_pass "observed multiple recv() calls (recv_calls=${RECV_CALLS})"
else
    e2e_fail "expected multiple recv() calls (recv_calls=${RECV_CALLS})"
fi
fail_fast_if_needed

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
