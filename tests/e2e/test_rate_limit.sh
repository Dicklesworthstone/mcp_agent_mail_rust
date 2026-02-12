#!/usr/bin/env bash
# test_rate_limit.sh - E2E test suite for HTTP rate limiting parity
#
# Verifies (br-1bm.2.5):
# - Basic allow/deny sequences for tools rate limiting
# - Per-tool isolation (endpoint keying)
# - Forwarded headers do not affect identity key (peer addr wins)
# - JWT sub identity overrides peer addr for key derivation
# - Optional Redis backend smoke (best-effort, if REDIS_URL is set)
#
# Artifacts (via e2e_lib.sh helpers):
# - Server logs: tests/artifacts/rate_limit/<timestamp>/logs/server_*.log
# - Per-case directories: <case_id>/request.json, response.json, headers.txt, status.txt
# - Decision trace: decision_trace.json (+ trace.jsonl)

set -euo pipefail

E2E_SUITE="rate_limit"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "HTTP Rate Limiting E2E Test Suite"

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

TRACE_JSONL="${E2E_ARTIFACT_DIR}/trace.jsonl"
touch "$TRACE_JSONL"

trace_add() {
    local run_label="$1"
    local case_id="$2"
    local tool_name="$3"
    local expected_status="$4"
    local actual_status="$5"
    local note="${6:-}"
    python3 - <<'PY' "$run_label" "$case_id" "$tool_name" "$expected_status" "$actual_status" "$note" >>"$TRACE_JSONL"
import json, sys
run_label, case_id, tool_name, exp, act, note = sys.argv[1:]
out = {
  "run": run_label,
  "case": case_id,
  "tool": tool_name,
  "expected_status": int(exp),
  "actual_status": int(act),
}
if note:
  out["note"] = note
print(json.dumps(out))
PY
}

# rate_limit_server_start: Start server with rate limit config using e2e_start_server_with_logs
# Args: label db_path storage_root [extra_env_vars...]
rate_limit_server_start() {
    local label="$1"
    local db_path="$2"
    local storage_root="$3"
    shift 3

    # Rate limit-specific env vars for hermetic testing
    local base_env_vars=(
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0"
        "HTTP_RBAC_ENABLED=0"
        "HTTP_RATE_LIMIT_ENABLED=1"
        "HTTP_RATE_LIMIT_TOOLS_PER_MINUTE=1"
        "HTTP_RATE_LIMIT_TOOLS_BURST=1"
    )

    # Start server with base env vars plus any extras passed in
    if ! e2e_start_server_with_logs "${db_path}" "${storage_root}" "${label}" \
        "${base_env_vars[@]}" "$@"; then
        e2e_fail "server (${label}) failed to start"
        return 1
    fi

    # Override URL to use /api/ endpoint instead of /mcp/
    E2E_SERVER_URL="${E2E_SERVER_URL%/mcp/}/api/"
    return 0
}

# rate_limit_rpc_call: Make JSON-RPC tool call (wraps e2e_rpc_call, tolerates non-200)
# Args: case_id tool_name [extra_headers...]
rate_limit_rpc_call() {
    local case_id="$1"
    local tool_name="$2"
    shift 2
    # Use e2e_rpc_call; suppress error return for 429 codes
    e2e_rpc_call "${case_id}" "${E2E_SERVER_URL}" "${tool_name}" "{}" "$@" || true
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Run 1: Memory backend (Bearer token auth)
# ---------------------------------------------------------------------------

e2e_banner "Run 1: Memory backend (static bearer token)"

WORK_MEM="$(e2e_mktemp "e2e_rate_limit_mem")"
DB_MEM="${WORK_MEM}/db.sqlite3"
STORAGE_MEM="${WORK_MEM}/storage_root"
mkdir -p "${STORAGE_MEM}"
TOKEN="e2e-token"

if ! rate_limit_server_start "memory" "${DB_MEM}" "${STORAGE_MEM}" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_JWT_ENABLED=0" \
    "HTTP_RATE_LIMIT_BACKEND=memory"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

AUTHZ="Authorization: Bearer ${TOKEN}"

e2e_case_banner "Basic allow/deny on same tool (memory)"
e2e_mark_case_start "mem_case1_req1"
rate_limit_rpc_call "mem_case1_req1" "health_check" "${AUTHZ}"
S1="$(e2e_rpc_read_status "mem_case1_req1")"
trace_add "memory" "basic_1" "health_check" "200" "${S1}"
e2e_assert_eq "req1 HTTP 200" "200" "${S1}"
e2e_mark_case_end "mem_case1_req1"

e2e_mark_case_start "mem_case1_req2"
rate_limit_rpc_call "mem_case1_req2" "health_check" "${AUTHZ}"
S2="$(e2e_rpc_read_status "mem_case1_req2")"
trace_add "memory" "basic_2" "health_check" "429" "${S2}"
e2e_assert_eq "req2 HTTP 429" "429" "${S2}"
e2e_mark_case_end "mem_case1_req2"

e2e_case_banner "Per-tool isolation (memory)"
e2e_mark_case_start "mem_case2_tool_a_1"
rate_limit_rpc_call "mem_case2_tool_a_1" "tool_a" "${AUTHZ}"
SA1="$(e2e_rpc_read_status "mem_case2_tool_a_1")"
trace_add "memory" "per_tool_a_1" "tool_a" "200" "${SA1}"
e2e_assert_eq "tool_a first HTTP 200" "200" "${SA1}"
e2e_mark_case_end "mem_case2_tool_a_1"

e2e_mark_case_start "mem_case2_tool_b_1"
rate_limit_rpc_call "mem_case2_tool_b_1" "tool_b" "${AUTHZ}"
SB1="$(e2e_rpc_read_status "mem_case2_tool_b_1")"
trace_add "memory" "per_tool_b_1" "tool_b" "200" "${SB1}"
e2e_assert_eq "tool_b first HTTP 200" "200" "${SB1}"
e2e_mark_case_end "mem_case2_tool_b_1"

e2e_mark_case_start "mem_case2_tool_a_2"
rate_limit_rpc_call "mem_case2_tool_a_2" "tool_a" "${AUTHZ}"
SA2="$(e2e_rpc_read_status "mem_case2_tool_a_2")"
trace_add "memory" "per_tool_a_2" "tool_a" "429" "${SA2}"
e2e_assert_eq "tool_a second HTTP 429" "429" "${SA2}"
e2e_mark_case_end "mem_case2_tool_a_2"

e2e_mark_case_start "mem_case2_tool_b_2"
rate_limit_rpc_call "mem_case2_tool_b_2" "tool_b" "${AUTHZ}"
SB2="$(e2e_rpc_read_status "mem_case2_tool_b_2")"
trace_add "memory" "per_tool_b_2" "tool_b" "429" "${SB2}"
e2e_assert_eq "tool_b second HTTP 429" "429" "${SB2}"
e2e_mark_case_end "mem_case2_tool_b_2"

e2e_case_banner "Forwarded headers do not affect identity key (memory)"
e2e_mark_case_start "mem_case3_fwd_1"
rate_limit_rpc_call "mem_case3_fwd_1" "forwarded_test" "${AUTHZ}" "X-Forwarded-For: 1.2.3.4"
SF1="$(e2e_rpc_read_status "mem_case3_fwd_1")"
trace_add "memory" "forwarded_1" "forwarded_test" "200" "${SF1}" "X-Forwarded-For=1.2.3.4"
e2e_assert_eq "forwarded req1 HTTP 200" "200" "${SF1}"
e2e_mark_case_end "mem_case3_fwd_1"

e2e_mark_case_start "mem_case3_fwd_2"
rate_limit_rpc_call "mem_case3_fwd_2" "forwarded_test" "${AUTHZ}" "X-Forwarded-For: 5.6.7.8"
SF2="$(e2e_rpc_read_status "mem_case3_fwd_2")"
trace_add "memory" "forwarded_2" "forwarded_test" "429" "${SF2}" "X-Forwarded-For=5.6.7.8"
e2e_assert_eq "forwarded req2 HTTP 429" "429" "${SF2}"
e2e_mark_case_end "mem_case3_fwd_2"

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 2: JWT backend (sub identity)
# ---------------------------------------------------------------------------

e2e_banner "Run 2: JWT backend (sub identity)"

WORK_JWT="$(e2e_mktemp "e2e_rate_limit_jwt")"
DB_JWT="${WORK_JWT}/db.sqlite3"
STORAGE_JWT="${WORK_JWT}/storage_root"
mkdir -p "${STORAGE_JWT}"
JWT_SECRET="e2e-secret"

if ! rate_limit_server_start "jwt" "${DB_JWT}" "${STORAGE_JWT}" \
    "HTTP_BEARER_TOKEN=" \
    "HTTP_JWT_ENABLED=1" \
    "HTTP_JWT_SECRET=${JWT_SECRET}"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

JWT1="$(make_jwt_hs256 "${JWT_SECRET}" '{"sub":"user-123"}')"
JWT2="$(make_jwt_hs256 "${JWT_SECRET}" '{"sub":"user-456"}')"

AUTHZ1="Authorization: Bearer ${JWT1}"
AUTHZ2="Authorization: Bearer ${JWT2}"

e2e_case_banner "JWT sub identity isolates buckets"
e2e_mark_case_start "jwt_case1_user1_1"
rate_limit_rpc_call "jwt_case1_user1_1" "health_check" "${AUTHZ1}"
J1="$(e2e_rpc_read_status "jwt_case1_user1_1")"
trace_add "jwt" "sub_user1_1" "health_check" "200" "${J1}" "sub=user-123"
e2e_assert_eq "user1 first HTTP 200" "200" "${J1}"
e2e_mark_case_end "jwt_case1_user1_1"

e2e_mark_case_start "jwt_case1_user2_1"
rate_limit_rpc_call "jwt_case1_user2_1" "health_check" "${AUTHZ2}"
J2="$(e2e_rpc_read_status "jwt_case1_user2_1")"
trace_add "jwt" "sub_user2_1" "health_check" "200" "${J2}" "sub=user-456"
e2e_assert_eq "user2 first HTTP 200" "200" "${J2}"
e2e_mark_case_end "jwt_case1_user2_1"

e2e_mark_case_start "jwt_case1_user1_2"
rate_limit_rpc_call "jwt_case1_user1_2" "health_check" "${AUTHZ1}"
J3="$(e2e_rpc_read_status "jwt_case1_user1_2")"
trace_add "jwt" "sub_user1_2" "health_check" "429" "${J3}" "sub=user-123"
e2e_assert_eq "user1 second HTTP 429" "429" "${J3}"
e2e_mark_case_end "jwt_case1_user1_2"

e2e_stop_server
trap - EXIT

# ---------------------------------------------------------------------------
# Run 3: Optional Redis backend smoke
# ---------------------------------------------------------------------------

if [ -n "${REDIS_URL:-}" ]; then
    e2e_banner "Run 3: Redis backend smoke (REDIS_URL set)"

    WORK_REDIS="$(e2e_mktemp "e2e_rate_limit_redis")"
    DB_REDIS="${WORK_REDIS}/db.sqlite3"
    STORAGE_REDIS="${WORK_REDIS}/storage_root"
    mkdir -p "${STORAGE_REDIS}"

    if ! rate_limit_server_start "redis" "${DB_REDIS}" "${STORAGE_REDIS}" \
        "HTTP_BEARER_TOKEN=${TOKEN}" \
        "HTTP_JWT_ENABLED=0" \
        "HTTP_RATE_LIMIT_BACKEND=redis" \
        "HTTP_RATE_LIMIT_REDIS_URL=${REDIS_URL}"; then
        e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
        e2e_summary
        exit 1
    fi
    trap 'e2e_stop_server || true' EXIT

    e2e_case_banner "Basic allow/deny on same tool (redis)"
    e2e_mark_case_start "redis_case1_req1"
    rate_limit_rpc_call "redis_case1_req1" "redis_tool" "${AUTHZ}"
    R1="$(e2e_rpc_read_status "redis_case1_req1")"
    trace_add "redis" "basic_1" "redis_tool" "200" "${R1}"
    e2e_assert_eq "redis req1 HTTP 200" "200" "${R1}"
    e2e_mark_case_end "redis_case1_req1"

    e2e_mark_case_start "redis_case1_req2"
    rate_limit_rpc_call "redis_case1_req2" "redis_tool" "${AUTHZ}"
    R2="$(e2e_rpc_read_status "redis_case1_req2")"
    trace_add "redis" "basic_2" "redis_tool" "429" "${R2}"
    e2e_assert_eq "redis req2 HTTP 429" "429" "${R2}"
    e2e_mark_case_end "redis_case1_req2"

    # Best-effort Redis state snapshot (requires redis-cli with -u support).
    if command -v redis-cli >/dev/null 2>&1; then
        REDIS_KEY="rl:tools:redis_tool:127.0.0.1"
        set +e
        redis-cli -u "${REDIS_URL}" TTL "${REDIS_KEY}" >"${E2E_ARTIFACT_DIR}/redis_ttl.txt" 2>"${E2E_ARTIFACT_DIR}/redis_ttl_stderr.txt"
        redis-cli -u "${REDIS_URL}" HMGET "${REDIS_KEY}" tokens ts >"${E2E_ARTIFACT_DIR}/redis_hmget.txt" 2>"${E2E_ARTIFACT_DIR}/redis_hmget_stderr.txt"
        set -e
    else
        e2e_log "redis-cli not found; skipping Redis state snapshot"
        e2e_skip "redis-cli not available for state snapshot"
    fi

    e2e_stop_server
    trap - EXIT
else
    e2e_log "REDIS_URL not set; skipping redis backend smoke"
    e2e_skip "REDIS_URL not set"
fi

# ---------------------------------------------------------------------------
# Emit trace JSON + summary
# ---------------------------------------------------------------------------

python3 - <<'PY' "$TRACE_JSONL" "${E2E_ARTIFACT_DIR}/decision_trace.json"
import json, sys
src, dest = sys.argv[1], sys.argv[2]
items = []
with open(src, "r", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        items.append(json.loads(line))
with open(dest, "w", encoding="utf-8") as f:
    json.dump(items, f, indent=2, sort_keys=True)
PY

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary

