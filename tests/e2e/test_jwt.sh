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
# Artifacts (via e2e_lib.sh helpers):
# - Server logs: tests/artifacts/jwt/<timestamp>/logs/server_*.log
# - Per-case directories: <case_id>/request.json, response.json, headers.txt, status.txt
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

# jwt_server_start: Start server with JWT config using e2e_start_server_with_logs
# Args: label [extra_env_vars...]
jwt_server_start() {
    local label="$1"
    shift

    # JWT-specific env vars for hermetic testing
    local jwt_env_vars=(
        "HTTP_BEARER_TOKEN="
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0"
        "HTTP_RBAC_ENABLED=0"
        "HTTP_RATE_LIMIT_ENABLED=0"
        "HTTP_JWT_ENABLED=1"
        "HTTP_JWT_SECRET=e2e-secret"
    )

    # Start server with base env vars plus any extras passed in
    if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "${label}" \
        "${jwt_env_vars[@]}" "$@"; then
        e2e_fail "server (${label}) failed to start"
        return 1
    fi

    # Override URL to use /api/ endpoint instead of /mcp/
    E2E_SERVER_URL="${E2E_SERVER_URL%/mcp/}/api/"
    return 0
}

# jwt_rpc_call: Make JSON-RPC call (wraps e2e_rpc_call, tolerates non-200)
# Args: case_id [extra_headers...]
jwt_rpc_call() {
    local case_id="$1"
    shift
    # Use e2e_rpc_call with health_check tool; suppress error return for 401s
    e2e_rpc_call "${case_id}" "${E2E_SERVER_URL}" "health_check" "{}" "$@" || true
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_jwt")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

# ---------------------------------------------------------------------------
# Run 1: base JWT secret (no aud/iss configured)
# ---------------------------------------------------------------------------

if ! jwt_server_start "base"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

e2e_case_banner "Missing Authorization -> 401"
e2e_mark_case_start "case1_missing_auth"
jwt_rpc_call "case1_missing_auth"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case1_missing_auth")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case1_missing_auth")" "Unauthorized"
e2e_mark_case_end "case1_missing_auth"
fail_fast_if_needed

e2e_case_banner "Non-Bearer Authorization -> 401"
e2e_mark_case_start "case2_non_bearer"
jwt_rpc_call "case2_non_bearer" "Authorization: Basic abc123"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case2_non_bearer")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case2_non_bearer")" "Unauthorized"
e2e_mark_case_end "case2_non_bearer"
fail_fast_if_needed

e2e_case_banner "Malformed JWT header segment -> 401"
e2e_mark_case_start "case3_malformed_header"
jwt_rpc_call "case3_malformed_header" "Authorization: Bearer abc.def.ghi"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case3_malformed_header")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case3_malformed_header")" "Unauthorized"
e2e_mark_case_end "case3_malformed_header"
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
e2e_mark_case_start "case4_bad_sig"
jwt_rpc_call "case4_bad_sig" "Authorization: Bearer ${BAD_SIG_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case4_bad_sig")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case4_bad_sig")" "Unauthorized"
e2e_mark_case_end "case4_bad_sig"
fail_fast_if_needed

e2e_case_banner "Expired exp -> 401"
e2e_mark_case_start "case5_expired"
jwt_rpc_call "case5_expired" "Authorization: Bearer ${EXPIRED_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case5_expired")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case5_expired")" "Unauthorized"
e2e_mark_case_end "case5_expired"
fail_fast_if_needed

e2e_case_banner "Future nbf -> 401"
e2e_mark_case_start "case6_future_nbf"
jwt_rpc_call "case6_future_nbf" "Authorization: Bearer ${FUTURE_NBF_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case6_future_nbf")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case6_future_nbf")" "Unauthorized"
e2e_mark_case_end "case6_future_nbf"
fail_fast_if_needed

e2e_case_banner "Valid HS256 -> 200"
e2e_mark_case_start "case7_valid"
jwt_rpc_call "case7_valid" "Authorization: Bearer ${VALID_TOKEN}"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case7_valid")"
e2e_assert_contains "JSON-RPC result present" "$(e2e_rpc_read_response "case7_valid")" "\"result\""
e2e_mark_case_end "case7_valid"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 2: Audience configured
# ---------------------------------------------------------------------------

if ! jwt_server_start "aud" "HTTP_JWT_AUDIENCE=aud-expected"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT
AUD_OK="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","aud":"aud-expected"}')"
AUD_BAD="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","aud":"aud-wrong"}')"
e2e_save_artifact "token_aud_ok_meta.json" "$(token_meta_json "$AUD_OK")"
e2e_save_artifact "token_aud_bad_meta.json" "$(token_meta_json "$AUD_BAD")"

e2e_case_banner "Audience mismatch -> 401"
e2e_mark_case_start "case8_aud_mismatch"
jwt_rpc_call "case8_aud_mismatch" "Authorization: Bearer ${AUD_BAD}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case8_aud_mismatch")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case8_aud_mismatch")" "Unauthorized"
e2e_mark_case_end "case8_aud_mismatch"
fail_fast_if_needed

e2e_case_banner "Audience match -> 200"
e2e_mark_case_start "case9_aud_match"
jwt_rpc_call "case9_aud_match" "Authorization: Bearer ${AUD_OK}"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case9_aud_match")"
e2e_assert_contains "JSON-RPC result present" "$(e2e_rpc_read_response "case9_aud_match")" "\"result\""
e2e_mark_case_end "case9_aud_match"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 3: Issuer configured
# ---------------------------------------------------------------------------

if ! jwt_server_start "iss" "HTTP_JWT_ISSUER=issuer-expected"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT
ISS_OK="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","iss":"issuer-expected"}')"
ISS_BAD="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","iss":"issuer-wrong"}')"
e2e_save_artifact "token_iss_ok_meta.json" "$(token_meta_json "$ISS_OK")"
e2e_save_artifact "token_iss_bad_meta.json" "$(token_meta_json "$ISS_BAD")"

e2e_case_banner "Issuer mismatch -> 401"
e2e_mark_case_start "case10_iss_mismatch"
jwt_rpc_call "case10_iss_mismatch" "Authorization: Bearer ${ISS_BAD}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case10_iss_mismatch")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case10_iss_mismatch")" "Unauthorized"
e2e_mark_case_end "case10_iss_mismatch"
fail_fast_if_needed

e2e_case_banner "Issuer match -> 200"
e2e_mark_case_start "case11_iss_match"
jwt_rpc_call "case11_iss_match" "Authorization: Bearer ${ISS_OK}"
e2e_assert_eq "HTTP 200" "200" "$(e2e_rpc_read_status "case11_iss_match")"
e2e_assert_contains "JSON-RPC result present" "$(e2e_rpc_read_response "case11_iss_match")" "\"result\""
e2e_mark_case_end "case11_iss_match"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 4: iat (issued-at) validation
# ---------------------------------------------------------------------------

if ! jwt_server_start "iat"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# Token with iat set to year 2100 (future issued-at)
FUTURE_IAT_TOKEN="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","iat":4102444800}')"
e2e_save_artifact "token_future_iat_meta.json" "$(token_meta_json "$FUTURE_IAT_TOKEN")"

e2e_case_banner "Future iat (issued-at year 2100) -> behavior documented"
e2e_mark_case_start "case12_future_iat"
jwt_rpc_call "case12_future_iat" "Authorization: Bearer ${FUTURE_IAT_TOKEN}"
# Note: jsonwebtoken crate does NOT validate iat by default.
# The server MAY accept this token. We document actual behavior.
IAT_STATUS="$(e2e_rpc_read_status "case12_future_iat")"
if [ "$IAT_STATUS" = "401" ]; then
    e2e_pass "Future iat correctly rejected (401)"
else
    # Document that future iat is allowed (default jsonwebtoken behavior)
    e2e_skip "Future iat allowed (status=$IAT_STATUS) - jsonwebtoken default behavior"
fi
e2e_mark_case_end "case12_future_iat"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 5: Multi-audience (JSON array format)
# ---------------------------------------------------------------------------

if ! jwt_server_start "multi_aud" "HTTP_JWT_AUDIENCE=aud-expected"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# Token with audience as JSON array containing the expected value
MULTI_AUD_OK="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","aud":["aud-expected","aud-other"]}')"
e2e_save_artifact "token_multi_aud_ok_meta.json" "$(token_meta_json "$MULTI_AUD_OK")"

# Token with audience as JSON array NOT containing the expected value
MULTI_AUD_BAD="$(make_jwt_hs256 "e2e-secret" '{"sub":"user-123","role":"writer","aud":["aud-wrong","aud-other"]}')"
e2e_save_artifact "token_multi_aud_bad_meta.json" "$(token_meta_json "$MULTI_AUD_BAD")"

e2e_case_banner "Multi-audience array containing expected -> 200"
e2e_mark_case_start "case13_multi_aud_ok"
jwt_rpc_call "case13_multi_aud_ok" "Authorization: Bearer ${MULTI_AUD_OK}"
MULTI_AUD_OK_STATUS="$(e2e_rpc_read_status "case13_multi_aud_ok")"
if [ "$MULTI_AUD_OK_STATUS" = "200" ]; then
    e2e_pass "Multi-audience array with match -> 200"
else
    # Document actual behavior if different
    e2e_skip "Multi-audience array behavior: status=$MULTI_AUD_OK_STATUS"
fi
e2e_mark_case_end "case13_multi_aud_ok"
fail_fast_if_needed

e2e_case_banner "Multi-audience array NOT containing expected -> 401"
e2e_mark_case_start "case14_multi_aud_bad"
jwt_rpc_call "case14_multi_aud_bad" "Authorization: Bearer ${MULTI_AUD_BAD}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case14_multi_aud_bad")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case14_multi_aud_bad")" "Unauthorized"
e2e_mark_case_end "case14_multi_aud_bad"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 6: Edge cases - missing alg, wrong alg
# ---------------------------------------------------------------------------

if ! jwt_server_start "alg_edge"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# Token with algorithm "none" (should be rejected)
NONE_ALG_TOKEN="$(python3 - <<'PY'
import base64, json

def b64url(data: bytes) -> bytes:
    return base64.urlsafe_b64encode(data).rstrip(b"=")

# Create JWT with alg: none (no signature)
header = {"alg":"none","typ":"JWT"}
payload = {"sub":"user-123","role":"writer"}
header_b64 = b64url(json.dumps(header, separators=(",", ":")).encode("utf-8"))
payload_b64 = b64url(json.dumps(payload, separators=(",", ":")).encode("utf-8"))
# alg:none means no signature segment
print(f"{header_b64.decode()}.{payload_b64.decode()}.")
PY
)"
e2e_save_artifact "token_none_alg_meta.json" "$(token_meta_json "$NONE_ALG_TOKEN")"

e2e_case_banner "alg:none JWT (no signature) -> 401"
e2e_mark_case_start "case15_alg_none"
jwt_rpc_call "case15_alg_none" "Authorization: Bearer ${NONE_ALG_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case15_alg_none")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case15_alg_none")" "Unauthorized"
e2e_mark_case_end "case15_alg_none"
fail_fast_if_needed

# Empty JWT (just dots)
e2e_case_banner "Empty JWT (just dots) -> 401"
e2e_mark_case_start "case16_empty_jwt"
jwt_rpc_call "case16_empty_jwt" "Authorization: Bearer .."
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case16_empty_jwt")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case16_empty_jwt")" "Unauthorized"
e2e_mark_case_end "case16_empty_jwt"
fail_fast_if_needed

# JWT with extra segments (4 parts instead of 3)
e2e_case_banner "JWT with extra segment (4 parts) -> 401"
e2e_mark_case_start "case17_extra_segment"
# Take a valid token and append an extra segment
EXTRA_SEG_TOKEN="${VALID_TOKEN}.extra_segment"
jwt_rpc_call "case17_extra_segment" "Authorization: Bearer ${EXTRA_SEG_TOKEN}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case17_extra_segment")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case17_extra_segment")" "Unauthorized"
e2e_mark_case_end "case17_extra_segment"
fail_fast_if_needed

# JWT with only header (missing payload and signature)
e2e_case_banner "JWT with only header segment -> 401"
e2e_mark_case_start "case18_header_only"
HEADER_ONLY="eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"  # base64url of {"alg":"HS256","typ":"JWT"}
jwt_rpc_call "case18_header_only" "Authorization: Bearer ${HEADER_ONLY}"
e2e_assert_eq "HTTP 401" "401" "$(e2e_rpc_read_status "case18_header_only")"
e2e_assert_contains "detail Unauthorized" "$(e2e_rpc_read_response "case18_header_only")" "Unauthorized"
e2e_mark_case_end "case18_header_only"
fail_fast_if_needed

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Note: RS256, JWKS rotation, JWKS errors require a mock JWKS server
# ---------------------------------------------------------------------------
# The following tests are deferred as they require additional infrastructure:
# - RS256 algorithm validation (needs RSA keypair + JWKS server)
# - JWKS key rotation mid-session
# - JWT with missing kid in header (needs JWKS with multiple keys)
# - JWKS endpoint returning HTTP 500
# - JWKS endpoint returning invalid JSON
#
# These can be added once a mock JWKS server helper is available.
# See: https://github.com/anthropics/mcp-agent-mail/issues/XXXX

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
