#!/usr/bin/env bash
# test_two_tier_hardening.sh - Two-tier search hardening E2E suite (br-2tnl.5.10.7)
#
# Coverage goals:
# 1. Thread-safe lazy initialization under concurrent search load
# 2. Fallback/degraded behavior when quality refinement is unavailable
# 3. Timeout handling with post-timeout recovery
# 4. Observability surface verification (health + server logs + per-case diagnostics)
#
# Deterministic CI replay (authoritative):
#   E2E_CLOCK_MODE=deterministic E2E_SEED=123 am e2e run --project . two_tier_hardening
# Compatibility fallback:
#   E2E_CLOCK_MODE=deterministic E2E_SEED=123 AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh two_tier_hardening

set -euo pipefail

E2E_SUITE="two_tier_hardening"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Two-Tier Search Hardening E2E Suite (br-2tnl.5.10.7)"

e2e_ensure_binary "am" >/dev/null
e2e_ensure_binary "mcp-agent-mail" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_two_tier_hardening")"
DB_PATH="${WORK}/two_tier.sqlite3"
STORAGE_ROOT="${WORK}/storage"
PROJECT_KEY="${WORK}/project-two-tier-hardening"
AGENT_A="AmberFalcon"
AGENT_B="BlueHarbor"
mkdir -p "${STORAGE_ROOT}"

SCENARIO_REPORT_JSON="${E2E_ARTIFACT_DIR}/two_tier_hardening_report.json"
SCENARIO_REPORT_JSONL="${E2E_ARTIFACT_DIR}/two_tier_hardening_cases.jsonl"
touch "${SCENARIO_REPORT_JSONL}"

cleanup_server() {
    e2e_stop_server 2>/dev/null || true
}
trap cleanup_server EXIT

json_path() {
    local json_input="$1"
    local path="$2"
    printf '%s' "${json_input}" | python3 -c '
import json, sys
path = sys.argv[1].split(".")
try:
    value = json.load(sys.stdin)
    for key in path:
        if isinstance(value, list):
            value = value[int(key)]
        else:
            value = value[key]
    if isinstance(value, (dict, list)):
        print(json.dumps(value))
    else:
        print(value)
except Exception:
    print("")
' "${path}" 2>/dev/null
}

extract_tool_text_from_response_file() {
    local response_file="$1"
    python3 - "${response_file}" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
if not path.exists():
    print("")
    raise SystemExit(0)

try:
    payload = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    print("")
    raise SystemExit(0)

content = payload.get("result", {}).get("content", [])
if content and isinstance(content[0], dict):
    print(content[0].get("text", ""))
else:
    print("")
PY
}

extract_rpc_error_code_from_response_file() {
    local response_file="$1"
    python3 - "${response_file}" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
if not path.exists():
    print("")
    raise SystemExit(0)

try:
    payload = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    print("")
    raise SystemExit(0)

error = payload.get("error")
if isinstance(error, dict) and "code" in error:
    print(error["code"])
else:
    print("")
PY
}

record_scenario() {
    local case_id="$1"
    local scenario_id="$2"
    local status="$3"
    local reason_code="$4"
    local detail="${5:-}"
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local timing_file="${case_dir}/timing.txt"
    local elapsed_ms="0"
    if [ -f "${timing_file}" ]; then
        elapsed_ms="$(cat "${timing_file}" 2>/dev/null || echo "0")"
    fi

    mkdir -p "${case_dir}"
    cat > "${case_dir}/scenario.json" <<EOF
{
  "scenario_id": "${scenario_id}",
  "case_id": "${case_id}",
  "status": "${status}",
  "reason_code": "${reason_code}",
  "elapsed_ms": ${elapsed_ms:-0},
  "artifact_path": "${case_dir}",
  "replay_command": "$(e2e_repro_command | tr -d '\n' | sed 's/"/\\"/g')",
  "detail": "$(printf '%s' "${detail}" | sed 's/"/\\"/g')"
}
EOF

    python3 - "${case_dir}/scenario.json" <<'PY' >> "${SCENARIO_REPORT_JSONL}"
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
print(json.dumps(payload, separators=(",", ":")))
PY
    echo >> "${SCENARIO_REPORT_JSONL}"
}

build_scenario_report() {
    python3 - "${SCENARIO_REPORT_JSONL}" "${SCENARIO_REPORT_JSON}" <<'PY'
import json
import sys
from pathlib import Path

jsonl_path = Path(sys.argv[1])
out_path = Path(sys.argv[2])

items = []
if jsonl_path.exists():
    for line in jsonl_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            items.append(json.loads(line))
        except json.JSONDecodeError:
            pass

status_counts = {}
for item in items:
    status = item.get("status", "unknown")
    status_counts[status] = status_counts.get(status, 0) + 1

report = {
    "schema_version": 1,
    "suite": "two_tier_hardening",
    "case_count": len(items),
    "status_counts": status_counts,
    "cases": items,
}
out_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
PY
}

make_args_json() {
    local project_key="$1"
    local query="$2"
    local mode="${3:-auto}"
    local limit="${4:-10}"
    python3 - "$project_key" "$query" "$mode" "$limit" <<'PY'
import json
import sys

project_key, query, mode, limit = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
print(json.dumps({
    "project_key": project_key,
    "query": query,
    "mode": mode,
    "limit": limit,
    "explain": True
}))
PY
}

run_rpc_success_case() {
    local case_id="$1"
    local tool_name="$2"
    local args_json="$3"
    local scenario_id="$4"
    local reason_code="$5"
    local detail="${6:-}"

    e2e_mark_case_start "${case_id}"
    if e2e_rpc_call "${case_id}" "${E2E_SERVER_URL}" "${tool_name}" "${args_json}"; then
        e2e_rpc_assert_success "${case_id}" "${scenario_id}: ${tool_name} succeeded"
        record_scenario "${case_id}" "${scenario_id}" "pass" "${reason_code}" "${detail}"
    else
        e2e_fail "${scenario_id}: ${tool_name} failed"
        e2e_extract_case_logs "${case_id}" >/dev/null 2>&1 || true
        record_scenario "${case_id}" "${scenario_id}" "fail" "${reason_code}" "RPC call failed"
        e2e_mark_case_end "${case_id}"
        return 1
    fi
    e2e_mark_case_end "${case_id}"
    return 0
}

# ---------------------------------------------------------------------------
# Case 1: Fixture/parser/assertion helper self-tests
# ---------------------------------------------------------------------------
e2e_case_banner "Fixture + parser helper unit checks"

HELPER_FIXTURE_DIR="${WORK}/helper_fixtures"
mkdir -p "${HELPER_FIXTURE_DIR}"
SUCCESS_FIXTURE="${HELPER_FIXTURE_DIR}/success_response.json"
ERROR_FIXTURE="${HELPER_FIXTURE_DIR}/error_response.json"

cat > "${SUCCESS_FIXTURE}" <<'EOF'
{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"status\":\"ok\",\"result\":[{\"id\":1,\"subject\":\"msg\"}]}"}]}}
EOF

cat > "${ERROR_FIXTURE}" <<'EOF'
{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid params"}}
EOF

SUCCESS_TEXT="$(extract_tool_text_from_response_file "${SUCCESS_FIXTURE}")"
ERROR_CODE="$(extract_rpc_error_code_from_response_file "${ERROR_FIXTURE}")"
STATUS_VALUE="$(json_path "${SUCCESS_TEXT}" "status")"
FIRST_SUBJECT="$(json_path "${SUCCESS_TEXT}" "result.0.subject")"

e2e_assert_eq "helper parser extracts status from fixture" "ok" "${STATUS_VALUE}"
e2e_assert_eq "helper parser extracts first subject from fixture" "msg" "${FIRST_SUBJECT}"
e2e_assert_eq "helper parser extracts JSON-RPC error code" "-32602" "${ERROR_CODE}"
record_scenario \
    "helpers_parser_unit_checks" \
    "two_tier.helpers.parsers" \
    "pass" \
    "fixture_parser_unit_checks" \
    "Validated fixture readers and JSON path parser"

# ---------------------------------------------------------------------------
# Boot server
# ---------------------------------------------------------------------------
if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "two_tier_hardening"; then
    e2e_fail "failed to start server for two-tier hardening suite"
    record_scenario \
        "server_startup" \
        "two_tier.server.startup" \
        "fail" \
        "server_start_failure" \
        "Server startup failed; see diagnostics/server_startup_failure.txt"
    build_scenario_report
    e2e_summary
    exit 1
fi

# ---------------------------------------------------------------------------
# Case 2: Setup corpus
# ---------------------------------------------------------------------------
e2e_case_banner "Corpus setup"

run_rpc_success_case \
    "setup_ensure_project" \
    "ensure_project" \
    "$(python3 -c 'import json,sys; print(json.dumps({"human_key": sys.argv[1]}))' "${PROJECT_KEY}")" \
    "two_tier.setup.ensure_project" \
    "setup_ok"

run_rpc_success_case \
    "setup_register_agent_a" \
    "register_agent" \
    "$(python3 -c 'import json,sys; print(json.dumps({"project_key": sys.argv[1], "program": "e2e-test", "model": "test-model", "name": sys.argv[2]}))' "${PROJECT_KEY}" "${AGENT_A}")" \
    "two_tier.setup.register_agent_a" \
    "setup_ok"

run_rpc_success_case \
    "setup_register_agent_b" \
    "register_agent" \
    "$(python3 -c 'import json,sys; print(json.dumps({"project_key": sys.argv[1], "program": "e2e-test", "model": "test-model", "name": sys.argv[2]}))' "${PROJECT_KEY}" "${AGENT_B}")" \
    "two_tier.setup.register_agent_b" \
    "setup_ok"

for i in 1 2 3 4; do
    run_rpc_success_case \
        "setup_send_message_${i}" \
        "send_message" \
        "$(python3 -c '
import json,sys
project, sender, recipient, idx = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
print(json.dumps({
    "project_key": project,
    "sender_name": sender,
    "to": [recipient],
    "subject": f"Two-tier hardening message {idx}",
    "body_md": f"Message {idx}: migration plan, semantic search hardening, quality refinement fallback verification."
}))
' "${PROJECT_KEY}" "${AGENT_A}" "${AGENT_B}" "${i}")" \
        "two_tier.setup.send_message_${i}" \
        "seed_data_inserted"
done

e2e_assert_db_row_count "${DB_PATH}" "messages" "4" "setup seeded four messages"

# ---------------------------------------------------------------------------
# Case 3: Thread-safe init under concurrent search calls
# ---------------------------------------------------------------------------
e2e_case_banner "Thread-safe init under concurrent search calls"

CONCURRENT_IDS=()
CONCURRENT_PIDS=()
for i in 1 2 3 4 5 6; do
    case_id="concurrent_search_${i}"
    CONCURRENT_IDS+=("${case_id}")
    args_json="$(make_args_json "${PROJECT_KEY}" "migration plan concurrent ${i}" "auto" "10")"
    (
        e2e_rpc_call "${case_id}" "${E2E_SERVER_URL}" "search_messages" "${args_json}"
    ) &
    CONCURRENT_PIDS+=("$!")
done

concurrent_failures=0
for pid in "${CONCURRENT_PIDS[@]}"; do
    if ! wait "${pid}"; then
        concurrent_failures=$((concurrent_failures + 1))
    fi
done

if [ "${concurrent_failures}" -eq 0 ]; then
    e2e_pass "all concurrent search requests completed successfully"
else
    e2e_fail "concurrent search requests failed (${concurrent_failures})"
fi

for case_id in "${CONCURRENT_IDS[@]}"; do
    e2e_rpc_assert_success "${case_id}" "concurrent case ${case_id} succeeded"
done

INIT_COUNT="$(grep -c "Two-tier search context initialized" "${_E2E_SERVER_LOG:-}" 2>/dev/null || echo "0")"
if [ "${INIT_COUNT}" -le 1 ]; then
    e2e_pass "lazy init log appears once under concurrency (count=${INIT_COUNT})"
    record_scenario \
        "concurrent_init" \
        "two_tier.init.concurrent" \
        "pass" \
        "single_init_observed" \
        "Two-tier initialization log count=${INIT_COUNT}"
else
    e2e_fail "lazy init log repeated unexpectedly (count=${INIT_COUNT})"
    record_scenario \
        "concurrent_init" \
        "two_tier.init.concurrent" \
        "fail" \
        "duplicate_init_log" \
        "Two-tier initialization log count=${INIT_COUNT}"
fi

# ---------------------------------------------------------------------------
# Case 4: Fallback/degraded mode behavior + observability
# ---------------------------------------------------------------------------
e2e_case_banner "Fallback/degraded behavior and observability checks"

run_rpc_success_case \
    "fallback_hybrid_search" \
    "search_messages" \
    "$(make_args_json "${PROJECT_KEY}" "quality refinement fallback" "hybrid" "10")" \
    "two_tier.fallback.hybrid_search" \
    "fallback_path_executed"

run_rpc_success_case \
    "fallback_health_check" \
    "health_check" \
    "{}" \
    "two_tier.fallback.health_check" \
    "health_surface_present"

HEALTH_TEXT="$(extract_tool_text_from_response_file "${E2E_ARTIFACT_DIR}/fallback_health_check/response.json")"
AVAILABILITY="$(json_path "${HEALTH_TEXT}" "two_tier_indexing.availability")"
if [ -n "${AVAILABILITY}" ]; then
    e2e_pass "health_check reports two_tier_indexing.availability (${AVAILABILITY})"
else
    e2e_fail "health_check missing two_tier_indexing.availability"
fi

if grep -Eq "FAST-ONLY mode|Search completed in fast-only mode|Two-tier search initialized: full mode|Two-tier search context initialized" "${_E2E_SERVER_LOG:-}" 2>/dev/null; then
    e2e_pass "server logs include two-tier observability startup/runtime messaging"
    record_scenario \
        "fallback_observability" \
        "two_tier.fallback.observability" \
        "pass" \
        "observability_logs_present" \
        "availability=${AVAILABILITY}"
else
    e2e_fail "server logs missing expected two-tier observability messaging"
    record_scenario \
        "fallback_observability" \
        "two_tier.fallback.observability" \
        "fail" \
        "missing_observability_logs" \
        "availability=${AVAILABILITY}"
fi

# ---------------------------------------------------------------------------
# Case 5: Timeout probe and recovery
# ---------------------------------------------------------------------------
e2e_case_banner "Timeout probe and post-timeout recovery"

TIMEOUT_CASE_ID="timeout_probe"
TIMEOUT_CASE_DIR="${E2E_ARTIFACT_DIR}/${TIMEOUT_CASE_ID}"
mkdir -p "${TIMEOUT_CASE_DIR}"
e2e_mark_case_start "${TIMEOUT_CASE_ID}"
TIMEOUT_ARGS="$(make_args_json "${PROJECT_KEY}" "timeout probe query" "auto" "10")"
cat > "${TIMEOUT_CASE_DIR}/request.json" <<EOF
{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"search_messages","arguments":${TIMEOUT_ARGS}}}
EOF
cat > "${TIMEOUT_CASE_DIR}/curl_args.txt" <<EOF
curl --max-time 0.001 -X POST "${E2E_SERVER_URL}" -H "content-type: application/json" --data "@${TIMEOUT_CASE_DIR}/request.json"
EOF

set +e
TIMEOUT_CURL_OUT="$(
    curl -sS \
        --max-time 0.001 \
        -D "${TIMEOUT_CASE_DIR}/headers.txt" \
        -o "${TIMEOUT_CASE_DIR}/response.json" \
        -w "%{http_code}|%{time_total}" \
        -X POST "${E2E_SERVER_URL}" \
        -H "content-type: application/json" \
        --data "@${TIMEOUT_CASE_DIR}/request.json" \
        2>"${TIMEOUT_CASE_DIR}/curl_stderr.txt"
)"
TIMEOUT_CURL_RC=$?
set -e

TIMEOUT_STATUS="${TIMEOUT_CURL_OUT%%|*}"
TIMEOUT_SECS="${TIMEOUT_CURL_OUT##*|}"
TIMEOUT_MS="$(python3 -c "print(int(float('${TIMEOUT_SECS:-0}') * 1000))" 2>/dev/null || echo "0")"
echo "${TIMEOUT_STATUS}" > "${TIMEOUT_CASE_DIR}/status.txt"
echo "${TIMEOUT_MS}" > "${TIMEOUT_CASE_DIR}/timing.txt"

if [ "${TIMEOUT_CURL_RC}" -eq 28 ]; then
    e2e_pass "timeout probe triggered curl timeout as expected (rc=28)"
    record_scenario \
        "${TIMEOUT_CASE_ID}" \
        "two_tier.timeout.probe" \
        "pass" \
        "client_timeout" \
        "curl_rc=28 max_time=0.001s"
else
    e2e_fail "timeout probe did not timeout (curl_rc=${TIMEOUT_CURL_RC}, status=${TIMEOUT_STATUS})"
    record_scenario \
        "${TIMEOUT_CASE_ID}" \
        "two_tier.timeout.probe" \
        "fail" \
        "timeout_not_triggered" \
        "curl_rc=${TIMEOUT_CURL_RC} status=${TIMEOUT_STATUS}"
fi

run_rpc_success_case \
    "recovery_after_timeout" \
    "search_messages" \
    "$(make_args_json "${PROJECT_KEY}" "recovery query after timeout" "auto" "10")" \
    "two_tier.timeout.recovery" \
    "recovery_ok" \
    "Search path still healthy after timeout probe"

# ---------------------------------------------------------------------------
# Finalize artifacts and summary
# ---------------------------------------------------------------------------
build_scenario_report
e2e_assert_file_exists "scenario report JSON created" "${SCENARIO_REPORT_JSON}"
e2e_assert_file_exists "scenario report JSONL created" "${SCENARIO_REPORT_JSONL}"
e2e_summary
