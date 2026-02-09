#!/usr/bin/env bash
# test_jwt.sh - E2E test suite for JWT (HS256 secret) auth parity
#
# Verifies (br-1bm.1.5 vectors, integration layer):
# - Missing Authorization -> 401
# - Non-Bearer Authorization -> 401
# - Malformed JWT header segment -> 401
# - Invalid signature -> 401
# - exp in past -> 401
# - nbf in future -> 401
# - Audience / issuer match + mismatch when configured
# - Valid HS256 token -> 200 JSON-RPC result
#
# Artifacts:
# - Server logs: tests/artifacts/jwt/<timestamp>/server_*.log
# - Per-case transcripts: *_status.txt, *_headers.txt, *_body.json, *_curl_stderr.txt
# - Per-case token metadata (hash + len), no secrets/tokens printed

set -euo pipefail

E2E_SUITE="jwt"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "JWT (HS256) E2E Test Suite"

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

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

fail_fast_if_needed() {
    if [ "${_E2E_FAIL}" -gt 0 ]; then
        e2e_log "Fail-fast: exiting after first failure"
        e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
        e2e_summary || true
        exit 1
    fi
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

# Create an HS256 JWT without external deps (PyJWT not required).
# Args:
#   $1: secret
#   $2: payload JSON (string)
make_jwt_hs256() {
    python3 - <<'PY' "$1" "$2"
import base64, json, hmac, hashlib, sys
secret = sys.argv[1].encode("utf-8")
payload = json.loads(sys.argv[2])
header = {"alg":"HS256","typ":"JWT"}

def b64url(data: bytes) -> bytes:
    return base64.urlsafe_b64encode(data).rstrip(b"=")

def compact(obj) -> bytes:
    return json.dumps(obj, separators=(",", ":"), sort_keys=True).encode("utf-8")

segments = [b64url(compact(header)), b64url(compact(payload))]
signing_input = b".".join(segments)
sig = hmac.new(secret, signing_input, hashlib.sha256).digest()
segments.append(b64url(sig))
print(b".".join(segments).decode("ascii"))
PY
}

token_meta_json() {
    local token="$1"
    local tok_len="${#token}"
    local tok_hash
    tok_hash="$(e2e_sha256_str "$token")"
    python3 - <<PY "$tok_len" "$tok_hash"
import json,sys
print(json.dumps({"len": int(sys.argv[1]), "sha256": sys.argv[2]}))
PY
}

http_post_jsonrpc() {
    local case_id="$1"
    local url="$2"
    shift 2

    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.json"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"

    local payload='{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"health_check","arguments":{}}}'
    e2e_save_artifact "${case_id}_request.json" "${payload}"

    local args=(
        -sS
        -D "${headers_file}"
        -o "${body_file}"
        -w "%{http_code}"
        -X POST
        "${url}"
        -H "content-type: application/json"
        --data "${payload}"
    )
    for h in "$@"; do
        args+=(-H "$h")
    done

    set +e
    local status
    status="$(curl "${args[@]}" 2>"${curl_stderr_file}")"
    local rc=$?
    set -e

    echo "${status}" > "${status_file}"
    if [ "$rc" -ne 0 ]; then
        e2e_fail "${case_id}: curl failed rc=${rc}"
        return 1
    fi
    return 0
}

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
        # Hermeticity: override any user-global ~/.mcp_agent_mail/.env bearer token.
        # This suite validates JWT behavior, so we must ensure static bearer auth is disabled.
        export HTTP_BEARER_TOKEN=""
        export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED="0"
        export HTTP_RBAC_ENABLED="0"
        export HTTP_RATE_LIMIT_ENABLED="0"
        export HTTP_JWT_ENABLED="1"
        export HTTP_JWT_SECRET="e2e-secret"

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
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_jwt")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"

# e2e_ensure_binary is verbose (logs to stdout); take the last line as the path.
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# ---------------------------------------------------------------------------
# Run 1: base JWT secret (no aud/iss configured)
# ---------------------------------------------------------------------------

PORT1="$(pick_port)"
PID1="$(start_server "base" "${PORT1}" "${DB_PATH}" "${STORAGE_ROOT}" "${BIN}")"
trap 'stop_server "${PID1}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT1}" 10; then
    e2e_fail "server failed to start (port not open)"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi

URL1="http://127.0.0.1:${PORT1}/api/"

e2e_case_banner "Missing Authorization -> 401"
http_post_jsonrpc "case1_missing_auth" "${URL1}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case1_missing_auth_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case1_missing_auth_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

e2e_case_banner "Non-Bearer Authorization -> 401"
http_post_jsonrpc "case2_non_bearer" "${URL1}" "Authorization: Basic abc123"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case2_non_bearer_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case2_non_bearer_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

e2e_case_banner "Malformed JWT header segment -> 401"
http_post_jsonrpc "case3_malformed_header" "${URL1}" "Authorization: Bearer abc.def.ghi"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case3_malformed_header_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case3_malformed_header_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

VALID_TOKEN="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer"}')"
e2e_save_artifact "token_valid_meta.json" "$(token_meta_json "$VALID_TOKEN")"

BAD_SIG_TOKEN="$(make_jwt_hs256 "wrong-secret" '{"sub":"user-123","role":"writer"}')"
e2e_save_artifact "token_bad_sig_meta.json" "$(token_meta_json "$BAD_SIG_TOKEN")"

EXPIRED_TOKEN="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","exp":1}')"
e2e_save_artifact "token_expired_meta.json" "$(token_meta_json "$EXPIRED_TOKEN")"

FUTURE_NBF_TOKEN="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","nbf":4102444800}')"
e2e_save_artifact "token_future_nbf_meta.json" "$(token_meta_json "$FUTURE_NBF_TOKEN")"

e2e_case_banner "Invalid signature -> 401"
http_post_jsonrpc "case4_bad_sig" "${URL1}" "Authorization: Bearer ${BAD_SIG_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case4_bad_sig_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case4_bad_sig_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

e2e_case_banner "Expired exp -> 401"
http_post_jsonrpc "case5_expired" "${URL1}" "Authorization: Bearer ${EXPIRED_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case5_expired_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case5_expired_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

e2e_case_banner "Future nbf -> 401"
http_post_jsonrpc "case6_future_nbf" "${URL1}" "Authorization: Bearer ${FUTURE_NBF_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case6_future_nbf_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case6_future_nbf_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

e2e_case_banner "Valid HS256 -> 200"
http_post_jsonrpc "case7_valid" "${URL1}" "Authorization: Bearer ${VALID_TOKEN}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/case7_valid_status.txt")"
e2e_assert_contains "JSON-RPC result present" "$(cat "${E2E_ARTIFACT_DIR}/case7_valid_body.json" 2>/dev/null || true)" "\"result\""
fail_fast_if_needed

stop_server "${PID1}"
trap - EXIT

# ---------------------------------------------------------------------------
# Run 2: Audience configured
# ---------------------------------------------------------------------------

PORT2="$(pick_port)"
PID2="$(start_server "aud" "${PORT2}" "${DB_PATH}" "${STORAGE_ROOT}" "${BIN}" "HTTP_JWT_AUDIENCE=aud-expected")"
trap 'stop_server "${PID2}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    e2e_fail "server (aud) failed to start (port not open)"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi

URL2="http://127.0.0.1:${PORT2}/api/"
AUD_OK="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","aud":"aud-expected"}')"
AUD_BAD="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","aud":"aud-wrong"}')"
e2e_save_artifact "token_aud_ok_meta.json" "$(token_meta_json "$AUD_OK")"
e2e_save_artifact "token_aud_bad_meta.json" "$(token_meta_json "$AUD_BAD")"

e2e_case_banner "Audience mismatch -> 401"
http_post_jsonrpc "case8_aud_mismatch" "${URL2}" "Authorization: Bearer ${AUD_BAD}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case8_aud_mismatch_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case8_aud_mismatch_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

e2e_case_banner "Audience match -> 200"
http_post_jsonrpc "case9_aud_match" "${URL2}" "Authorization: Bearer ${AUD_OK}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/case9_aud_match_status.txt")"
e2e_assert_contains "JSON-RPC result present" "$(cat "${E2E_ARTIFACT_DIR}/case9_aud_match_body.json" 2>/dev/null || true)" "\"result\""
fail_fast_if_needed

stop_server "${PID2}"
trap - EXIT

# ---------------------------------------------------------------------------
# Run 3: Issuer configured
# ---------------------------------------------------------------------------

PORT3="$(pick_port)"
PID3="$(start_server "iss" "${PORT3}" "${DB_PATH}" "${STORAGE_ROOT}" "${BIN}" "HTTP_JWT_ISSUER=issuer-expected")"
trap 'stop_server "${PID3}" || true' EXIT

if ! e2e_wait_port 127.0.0.1 "${PORT3}" 10; then
    e2e_fail "server (iss) failed to start (port not open)"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi

URL3="http://127.0.0.1:${PORT3}/api/"
ISS_OK="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","iss":"issuer-expected"}')"
ISS_BAD="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","iss":"issuer-wrong"}')"
e2e_save_artifact "token_iss_ok_meta.json" "$(token_meta_json "$ISS_OK")"
e2e_save_artifact "token_iss_bad_meta.json" "$(token_meta_json "$ISS_BAD")"

e2e_case_banner "Issuer mismatch -> 401"
http_post_jsonrpc "case10_iss_mismatch" "${URL3}" "Authorization: Bearer ${ISS_BAD}"
e2e_assert_eq "HTTP 401" "401" "$(cat "${E2E_ARTIFACT_DIR}/case10_iss_mismatch_status.txt")"
e2e_assert_contains "detail Unauthorized" "$(cat "${E2E_ARTIFACT_DIR}/case10_iss_mismatch_body.json" 2>/dev/null || true)" "Unauthorized"
fail_fast_if_needed

e2e_case_banner "Issuer match -> 200"
http_post_jsonrpc "case11_iss_match" "${URL3}" "Authorization: Bearer ${ISS_OK}"
e2e_assert_eq "HTTP 200" "200" "$(cat "${E2E_ARTIFACT_DIR}/case11_iss_match_status.txt")"
e2e_assert_contains "JSON-RPC result present" "$(cat "${E2E_ARTIFACT_DIR}/case11_iss_match_body.json" 2>/dev/null || true)" "\"result\""
fail_fast_if_needed

stop_server "${PID3}"
trap - EXIT

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
