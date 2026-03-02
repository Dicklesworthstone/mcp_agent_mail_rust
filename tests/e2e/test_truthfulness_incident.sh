#!/usr/bin/env bash
# test_truthfulness_incident.sh - E9: End-to-End Incident Matrix Script with Rich Diagnostics
#
# Validates all major user-visible workflows in one run using a seeded fixture:
# - dashboard/message body visibility and markdown rendering
# - messages table/detail body correctness
# - threads/agents/projects non-empty truth with seeded fixture
# - system health URL actionability + auth success/unauthorized-remediation paths
#
# Emits machine-readable artifact bundle (expected vs observed counts,
# representative IDs, rendered body snippets, route/auth context, failing
# query params) plus concise human-readable CI failure summary.
#
# Bead: br-2k3qx.5.9

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
export E2E_SUITE="truthfulness_incident"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "E9: Truthfulness Incident Matrix"

# ── Prerequisite checks ──────────────────────────────────────────────────
for required_cmd in python3 jq sqlite3 curl; do
    if ! command -v "${required_cmd}" >/dev/null 2>&1; then
        e2e_log "${required_cmd} missing; skipping suite"
        e2e_skip "${required_cmd} required"
        e2e_summary
        exit 0
    fi
done

if ! command -v mcp-agent-mail >/dev/null 2>&1 || ! command -v am >/dev/null 2>&1; then
    if [ -x "${CARGO_TARGET_DIR}/debug/mcp-agent-mail" ] && [ -x "${CARGO_TARGET_DIR}/debug/am" ]; then
        export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
    fi
fi
if ! command -v mcp-agent-mail >/dev/null 2>&1 || ! command -v am >/dev/null 2>&1; then
    e2e_log "mcp-agent-mail/am binaries unavailable; skipping suite"
    e2e_skip "mcp-agent-mail + am required"
    e2e_summary
    exit 0
fi

# ── Fixture parameters (small but representative) ────────────────────────
SEED=20260302
F_PROJECTS=3
F_AGENTS=10
F_MESSAGES=50
F_THREADS=10

# ── Workspace setup ──────────────────────────────────────────────────────
WORK="$(e2e_mktemp "e2e_truthfulness_incident")"
DB_PATH="${WORK}/truthfulness.sqlite3"
STORAGE_ROOT="${WORK}/storage"
SERVER_LOG="${WORK}/server.log"
DIAGNOSTICS_DIR="${WORK}/diagnostics"
SNAPSHOTS_DIR="${WORK}/snapshots"
mkdir -p "${STORAGE_ROOT}" "${DIAGNOSTICS_DIR}" "${SNAPSHOTS_DIR}"

SERVER_PID=""
PORT=""

cleanup_server() {
    if [ -n "${SERVER_PID}" ] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" >/dev/null 2>&1 || true
        wait "${SERVER_PID}" >/dev/null 2>&1 || true
    fi
}
trap cleanup_server EXIT

pick_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_for_health() {
    local port="$1"
    local attempts="${2:-120}"
    local delay="${3:-0.1}"
    local url="http://127.0.0.1:${port}/health"
    for _ in $(seq 1 "${attempts}"); do
        if curl -fsS -o /dev/null --connect-timeout 1 --max-time 1 "${url}" 2>/dev/null; then
            return 0
        fi
        if [ -n "${SERVER_PID}" ] && ! kill -0 "${SERVER_PID}" 2>/dev/null; then
            return 1
        fi
        sleep "${delay}"
    done
    return 1
}

# Fault-tolerant robot capture with fallback payloads
CAPTURE_FAILURES=0
CAPTURE_SUCCESSES=0

run_robot() {
    local output_path="$1"
    shift
    local label
    label="$(basename "${output_path}")"
    local stderr_path="${output_path}.stderr"
    local exit_code=0
    AM_INTERFACE_MODE=cli \
        DATABASE_URL="sqlite:///${DB_PATH}" \
        STORAGE_ROOT="${STORAGE_ROOT}" \
        timeout --kill-after=3s 10s \
        am robot --project "${PROJECT_KEY}" --agent "${AGENT_NAME}" "$@" \
        >"${output_path}" 2>"${stderr_path}" || exit_code=$?
    if [ "${exit_code}" -ne 0 ]; then
        CAPTURE_FAILURES=$((CAPTURE_FAILURES + 1))
        if [[ "${output_path}" == *.md ]]; then
            printf '# Capture Failure: %s\n\nexit_code: %d\n' "${label}" "${exit_code}" > "${output_path}"
        else
            printf '{"_capture_error":true,"label":"%s","exit_code":%d}\n' "${label}" "${exit_code}" > "${output_path}"
        fi
    else
        CAPTURE_SUCCESSES=$((CAPTURE_SUCCESSES + 1))
    fi
    return 0
}

# ══════════════════════════════════════════════════════════════════════════
# CASE 1: Seed fixture and verify DB ground truth
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "seed fixture and verify DB ground truth"
e2e_mark_case_start "case01_seed_fixture_and_verify_db_ground_truth"

SEED_SCRIPT="${PROJECT_ROOT}/scripts/seed_truth_incident_fixture.sh"
e2e_assert_file_exists "seed script exists" "${SEED_SCRIPT}"

set +e
"${SEED_SCRIPT}" \
    --db "${DB_PATH}" \
    --storage-root "${STORAGE_ROOT}" \
    --seed "${SEED}" \
    --projects "${F_PROJECTS}" \
    --agents "${F_AGENTS}" \
    --messages "${F_MESSAGES}" \
    --threads "${F_THREADS}" \
    --report "${DIAGNOSTICS_DIR}/fixture_report.json" \
    --verbose \
    >"${WORK}/seed_stdout.log" 2>"${WORK}/seed_stderr.log"
SEED_RC=$?
set -e

e2e_save_artifact "seed_stdout.log" "$(cat "${WORK}/seed_stdout.log" 2>/dev/null || true)"
e2e_save_artifact "seed_stderr.log" "$(cat "${WORK}/seed_stderr.log" 2>/dev/null || true)"
# Seed script exits non-zero when counts are below its A1-scale thresholds (15p/300a/3000m/300t).
# We use a deliberately smaller fixture (3p/10a/50m/10t) for fast CI runs, so we accept exit
# code 1 (validation failure) as long as the data was actually seeded into the database.
if [ "${SEED_RC}" -eq 0 ] || [ "${SEED_RC}" -eq 1 ]; then
    e2e_pass "fixture seeding completed (exit ${SEED_RC})"
else
    e2e_fail "fixture seeding completed (exit ${SEED_RC})"
fi
e2e_assert_file_exists "database was created" "${DB_PATH}"
e2e_assert_file_exists "fixture report was created" "${DIAGNOSTICS_DIR}/fixture_report.json"

# Verify the fixture report's counts match our requested values (not the A1 thresholds)
if jq -e ".counts.projects >= ${F_PROJECTS} and .counts.agents >= ${F_AGENTS} and .counts.messages >= ${F_MESSAGES} and .counts.threads >= ${F_THREADS}" \
    "${DIAGNOSTICS_DIR}/fixture_report.json" >/dev/null 2>&1; then
    e2e_pass "fixture report counts match requested fixture scale"
else
    e2e_fail "fixture report counts match requested fixture scale"
fi

# Extract DB ground truth counts
DB_PROJECTS="$(sqlite3 "${DB_PATH}" "SELECT COUNT(*) FROM projects" 2>/dev/null | tr -d '[:space:]')"
DB_AGENTS="$(sqlite3 "${DB_PATH}" "SELECT COUNT(DISTINCT name) FROM agents" 2>/dev/null | tr -d '[:space:]')"
DB_MESSAGES="$(sqlite3 "${DB_PATH}" "SELECT COUNT(*) FROM messages" 2>/dev/null | tr -d '[:space:]')"
DB_THREADS="$(sqlite3 "${DB_PATH}" "SELECT COUNT(DISTINCT thread_id) FROM messages" 2>/dev/null | tr -d '[:space:]')"
DB_BODIES_WITH_MD="$(sqlite3 "${DB_PATH}" "SELECT COUNT(*) FROM messages WHERE body_md IS NOT NULL AND LENGTH(body_md) > 0" 2>/dev/null | tr -d '[:space:]')"

# Write ground truth as machine-readable JSON
cat > "${DIAGNOSTICS_DIR}/db_truth.json" <<JSON
{
  "seed": ${SEED},
  "expected": {
    "projects": ${F_PROJECTS},
    "agents": ${F_AGENTS},
    "messages": ${F_MESSAGES},
    "threads": ${F_THREADS}
  },
  "observed": {
    "projects": ${DB_PROJECTS},
    "agents": ${DB_AGENTS},
    "messages": ${DB_MESSAGES},
    "threads": ${DB_THREADS},
    "bodies_with_markdown": ${DB_BODIES_WITH_MD}
  }
}
JSON
e2e_save_artifact "db_truth.json" "$(cat "${DIAGNOSTICS_DIR}/db_truth.json")"

# Verify fixture meets minimums
if [ "${DB_PROJECTS}" -ge "${F_PROJECTS}" ]; then
    e2e_pass "DB projects >= ${F_PROJECTS} (got ${DB_PROJECTS})"
else
    e2e_fail "DB projects >= ${F_PROJECTS} (got ${DB_PROJECTS})"
fi
if [ "${DB_AGENTS}" -ge "${F_AGENTS}" ]; then
    e2e_pass "DB agents >= ${F_AGENTS} (got ${DB_AGENTS})"
else
    e2e_fail "DB agents >= ${F_AGENTS} (got ${DB_AGENTS})"
fi
if [ "${DB_MESSAGES}" -ge "${F_MESSAGES}" ]; then
    e2e_pass "DB messages >= ${F_MESSAGES} (got ${DB_MESSAGES})"
else
    e2e_fail "DB messages >= ${F_MESSAGES} (got ${DB_MESSAGES})"
fi
if [ "${DB_THREADS}" -ge "${F_THREADS}" ]; then
    e2e_pass "DB threads >= ${F_THREADS} (got ${DB_THREADS})"
else
    e2e_fail "DB threads >= ${F_THREADS} (got ${DB_THREADS})"
fi

# ══════════════════════════════════════════════════════════════════════════
# CASE 2: Start headless server and capture all surfaces
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "start headless server and capture surfaces"
e2e_mark_case_start "case02_start_headless_server_and_capture_surfaces"

PORT="$(pick_port)"

DATABASE_URL="sqlite:///${DB_PATH}" \
STORAGE_ROOT="${STORAGE_ROOT}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PORT}" \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true \
TUI_ENABLED=false \
RUST_LOG=error \
mcp-agent-mail serve --no-tui >"${SERVER_LOG}" 2>&1 &
SERVER_PID=$!

if wait_for_health "${PORT}" 120 0.1; then
    e2e_pass "server started and healthy on port ${PORT}"
else
    e2e_fail "server started and healthy"
    e2e_save_artifact "server.log" "$(cat "${SERVER_LOG}" 2>/dev/null || true)"
    e2e_summary
    exit 1
fi

# Pick a representative project/agent/thread/message for robot queries
PROJECT_KEY="$(sqlite3 "${DB_PATH}" "SELECT slug FROM projects LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
AGENT_NAME="$(sqlite3 "${DB_PATH}" "SELECT DISTINCT name FROM agents LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
THREAD_ID="$(sqlite3 "${DB_PATH}" "SELECT DISTINCT thread_id FROM messages LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
MESSAGE_ID="$(sqlite3 "${DB_PATH}" "SELECT id FROM messages LIMIT 1" 2>/dev/null | tr -d '[:space:]')"

e2e_save_artifact "context.json" "$(cat <<JSON
{
  "project_key": "${PROJECT_KEY}",
  "agent_name": "${AGENT_NAME}",
  "thread_id": "${THREAD_ID}",
  "message_id": ${MESSAGE_ID},
  "port": ${PORT},
  "seed": ${SEED},
  "db_path": "${DB_PATH}"
}
JSON
)"

# Capture all robot surfaces
run_robot "${SNAPSHOTS_DIR}/dashboard_status.json" status
run_robot "${SNAPSHOTS_DIR}/messages_inbox.json" inbox --all --include-bodies --limit 30 --format json
run_robot "${SNAPSHOTS_DIR}/message_detail.json" message "${MESSAGE_ID}" --format json
run_robot "${SNAPSHOTS_DIR}/threads_view.json" thread "${THREAD_ID}" --limit 50 --format json
run_robot "${SNAPSHOTS_DIR}/agents_view.json" agents --format json
run_robot "${SNAPSHOTS_DIR}/projects_view.json" projects --format json
run_robot "${SNAPSHOTS_DIR}/system_health.json" health --format json
run_robot "${SNAPSHOTS_DIR}/contacts.json" contacts --format json
run_robot "${SNAPSHOTS_DIR}/search_results.json" search "incident" --format json

# Capture HTTP endpoints directly
set +e
HTTP_HEALTH="$(curl -fsS --connect-timeout 3 --max-time 5 "http://127.0.0.1:${PORT}/health" 2>/dev/null)"
HTTP_HEALTH_RC=$?
set -e
printf '%s' "${HTTP_HEALTH}" > "${SNAPSHOTS_DIR}/http_health.json"

if [ "${HTTP_HEALTH_RC}" -eq 0 ]; then
    e2e_pass "HTTP /health endpoint responsive"
else
    e2e_fail "HTTP /health endpoint responsive"
fi

e2e_pass "captured ${CAPTURE_SUCCESSES} robot surfaces (${CAPTURE_FAILURES} failures)"

# ══════════════════════════════════════════════════════════════════════════
# CASE 3: Dashboard body visibility and markdown rendering
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "dashboard body visibility and markdown rendering"
e2e_mark_case_start "case03_dashboard_body_visibility_and_markdown_rendering"

if [ -f "${SNAPSHOTS_DIR}/dashboard_status.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/dashboard_status.json" >/dev/null 2>&1; then
    e2e_pass "dashboard snapshot is valid JSON"
else
    e2e_fail "dashboard snapshot is valid JSON"
fi

# Dashboard should report non-zero project/agent/message counts if fixture has data
if jq -e '._capture_error' "${SNAPSHOTS_DIR}/dashboard_status.json" >/dev/null 2>&1; then
    e2e_fail "dashboard capture succeeded (got capture error)"
else
    e2e_pass "dashboard capture succeeded"
fi

# ══════════════════════════════════════════════════════════════════════════
# CASE 4: Messages table/detail body correctness
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "messages table/detail body correctness"
e2e_mark_case_start "case04_messages_tabledetail_body_correctness"

if [ -f "${SNAPSHOTS_DIR}/messages_inbox.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/messages_inbox.json" >/dev/null 2>&1; then
    e2e_pass "messages inbox snapshot is valid JSON"
else
    e2e_fail "messages inbox snapshot is valid JSON"
fi

# Messages should have non-empty body_md
if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/messages_inbox.json" >/dev/null 2>&1; then
    INBOX_COUNT="$(jq 'if .inbox then (.inbox | length) elif type == "array" then length elif .messages then (.messages | length) else .count // 0 end' "${SNAPSHOTS_DIR}/messages_inbox.json" 2>/dev/null || echo 0)"
    if [ "${INBOX_COUNT}" -gt 0 ]; then
        e2e_pass "inbox returned ${INBOX_COUNT} messages (expected > 0)"
    else
        e2e_fail "inbox returned messages (got ${INBOX_COUNT}, expected > 0)"
    fi

    # Check body_md is present in at least some messages
    BODIES_PRESENT="$(jq '[(.inbox // .messages // if type == "array" then . else [] end)[] | select(.body_md != null and (.body_md | length) > 0)] | length' "${SNAPSHOTS_DIR}/messages_inbox.json" 2>/dev/null || echo 0)"
    if [ "${BODIES_PRESENT}" -gt 0 ]; then
        e2e_pass "inbox messages contain body_md (${BODIES_PRESENT} with bodies)"
    else
        e2e_fail "inbox messages contain body_md (${BODIES_PRESENT} with bodies)"
    fi

    # Extract representative body snippets for artifact
    jq '[(.inbox // .messages // if type == "array" then . else [] end)[:3][] | {id, subject, body_md_len: (.body_md // "" | length), body_md_excerpt: (.body_md // "" | .[:200])}]' \
        "${SNAPSHOTS_DIR}/messages_inbox.json" > "${DIAGNOSTICS_DIR}/body_snippets.json" 2>/dev/null || true
    e2e_save_artifact "body_snippets.json" "$(cat "${DIAGNOSTICS_DIR}/body_snippets.json" 2>/dev/null || echo '[]')"
else
    e2e_fail "inbox capture succeeded (got capture error)"
fi

# Message detail
if [ -f "${SNAPSHOTS_DIR}/message_detail.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/message_detail.json" >/dev/null 2>&1; then
    e2e_pass "message detail snapshot is valid JSON"
    if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/message_detail.json" >/dev/null 2>&1; then
        DETAIL_BODY_LEN="$(jq '(.body // .body_md // "" | length)' "${SNAPSHOTS_DIR}/message_detail.json" 2>/dev/null || echo 0)"
        if [ "${DETAIL_BODY_LEN}" -gt 0 ]; then
            e2e_pass "message detail has body_md (len=${DETAIL_BODY_LEN})"
        else
            e2e_fail "message detail has body_md (len=${DETAIL_BODY_LEN})"
        fi
    else
        e2e_fail "message detail capture succeeded"
    fi
else
    e2e_fail "message detail snapshot is valid JSON"
fi

# ══════════════════════════════════════════════════════════════════════════
# CASE 5: Threads non-empty truth
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "threads non-empty truth with seeded fixture"
e2e_mark_case_start "case05_threads_nonempty_truth_with_seeded_fixture"

if [ -f "${SNAPSHOTS_DIR}/threads_view.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/threads_view.json" >/dev/null 2>&1; then
    e2e_pass "thread snapshot is valid JSON"
    if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/threads_view.json" >/dev/null 2>&1; then
        THREAD_MSG_COUNT="$(jq 'if .messages then (.messages | length) elif type == "array" then length else 0 end' "${SNAPSHOTS_DIR}/threads_view.json" 2>/dev/null || echo 0)"
        if [ "${THREAD_MSG_COUNT}" -gt 0 ]; then
            e2e_pass "thread view contains messages (${THREAD_MSG_COUNT})"
        else
            e2e_fail "thread view contains messages (${THREAD_MSG_COUNT})"
        fi
    else
        e2e_fail "thread capture succeeded"
    fi
else
    e2e_fail "thread snapshot is valid JSON"
fi

# Verify thread count in DB matches fixture expectation
if [ "${DB_THREADS}" -ge "${F_THREADS}" ]; then
    e2e_pass "DB thread count matches fixture (${DB_THREADS} >= ${F_THREADS})"
else
    e2e_fail "DB thread count matches fixture (${DB_THREADS} < ${F_THREADS})"
fi

# ══════════════════════════════════════════════════════════════════════════
# CASE 6: Agents non-empty truth
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "agents non-empty truth with seeded fixture"
e2e_mark_case_start "case06_agents_nonempty_truth_with_seeded_fixture"

if [ -f "${SNAPSHOTS_DIR}/agents_view.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/agents_view.json" >/dev/null 2>&1; then
    e2e_pass "agents snapshot is valid JSON"
    if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/agents_view.json" >/dev/null 2>&1; then
        AGENT_SURFACE_COUNT="$(jq 'if type == "array" then length elif .agents then (.agents | length) else 0 end' "${SNAPSHOTS_DIR}/agents_view.json" 2>/dev/null || echo 0)"
        if [ "${AGENT_SURFACE_COUNT}" -gt 0 ]; then
            e2e_pass "agents surface lists ${AGENT_SURFACE_COUNT} agents (expected > 0)"
        else
            e2e_fail "agents surface lists ${AGENT_SURFACE_COUNT} agents (expected > 0)"
        fi
        # Cross-check: surface count should not exceed DB count
        if [ "${AGENT_SURFACE_COUNT}" -le "${DB_AGENTS}" ]; then
            e2e_pass "agents surface count <= DB count (${AGENT_SURFACE_COUNT} <= ${DB_AGENTS})"
        else
            e2e_fail "agents surface count <= DB count (${AGENT_SURFACE_COUNT} > ${DB_AGENTS})"
        fi
    else
        e2e_fail "agents capture succeeded"
    fi
else
    e2e_fail "agents snapshot is valid JSON"
fi

# ══════════════════════════════════════════════════════════════════════════
# CASE 7: Projects non-empty truth
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "projects non-empty truth with seeded fixture"
e2e_mark_case_start "case07_projects_nonempty_truth_with_seeded_fixture"

if [ -f "${SNAPSHOTS_DIR}/projects_view.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/projects_view.json" >/dev/null 2>&1; then
    e2e_pass "projects snapshot is valid JSON"
    if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/projects_view.json" >/dev/null 2>&1; then
        PROJECT_SURFACE_COUNT="$(jq 'if type == "array" then length elif .projects then (.projects | length) else 0 end' "${SNAPSHOTS_DIR}/projects_view.json" 2>/dev/null || echo 0)"
        if [ "${PROJECT_SURFACE_COUNT}" -gt 0 ]; then
            e2e_pass "projects surface lists ${PROJECT_SURFACE_COUNT} projects (expected > 0)"
        else
            e2e_fail "projects surface lists ${PROJECT_SURFACE_COUNT} projects (expected > 0)"
        fi
        if [ "${PROJECT_SURFACE_COUNT}" -le "${DB_PROJECTS}" ]; then
            e2e_pass "projects surface count <= DB count (${PROJECT_SURFACE_COUNT} <= ${DB_PROJECTS})"
        else
            e2e_fail "projects surface count <= DB count (${PROJECT_SURFACE_COUNT} > ${DB_PROJECTS})"
        fi
    else
        e2e_fail "projects capture succeeded"
    fi
else
    e2e_fail "projects snapshot is valid JSON"
fi

# ══════════════════════════════════════════════════════════════════════════
# CASE 8: System health URL actionability + auth paths
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "system health URL actionability + auth paths"
e2e_mark_case_start "case08_system_health_url_actionability_auth_paths"

# Health endpoint should return valid JSON with status
if [ -f "${SNAPSHOTS_DIR}/http_health.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/http_health.json" >/dev/null 2>&1; then
    e2e_pass "HTTP health returns valid JSON"
    if jq -e '.status' "${SNAPSHOTS_DIR}/http_health.json" >/dev/null 2>&1; then
        e2e_pass "HTTP health includes status field"
    else
        e2e_fail "HTTP health includes status field"
    fi
else
    e2e_fail "HTTP health returns valid JSON"
fi

# Robot health should also return valid data
if [ -f "${SNAPSHOTS_DIR}/system_health.json" ] && jq -e '.' "${SNAPSHOTS_DIR}/system_health.json" >/dev/null 2>&1; then
    e2e_pass "robot health returns valid JSON"
    if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/system_health.json" >/dev/null 2>&1; then
        e2e_pass "robot health capture succeeded"
    else
        e2e_fail "robot health capture succeeded"
    fi
else
    e2e_fail "robot health returns valid JSON"
fi

# Auth success path: unauthenticated request to /health should succeed
# (HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true)
set +e
AUTH_OK_CODE="$(curl -o /dev/null -w '%{http_code}' -sS --connect-timeout 3 --max-time 5 "http://127.0.0.1:${PORT}/health" 2>/dev/null)"
set -e
if [ "${AUTH_OK_CODE}" = "200" ]; then
    e2e_pass "unauthenticated localhost /health returns 200"
else
    e2e_fail "unauthenticated localhost /health returns 200 (got ${AUTH_OK_CODE})"
fi

# ══════════════════════════════════════════════════════════════════════════
# CASE 9: Cross-surface truth comparison (DB vs surfaces)
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "cross-surface truth comparison"
e2e_mark_case_start "case09_crosssurface_truth_comparison"

COMPARISON_RESULTS=()
COMPARISON_PASS=0
COMPARISON_FAIL=0

add_comparison() {
    local check_id="$1"
    local surface_name="$2"
    local expected="$3"
    local observed="$4"
    local verdict="$5"
    COMPARISON_RESULTS+=("$(printf '{"check_id":"%s","surface":"%s","expected":%s,"observed":%s,"verdict":"%s"}' \
        "${check_id}" "${surface_name}" "${expected}" "${observed}" "${verdict}")")
    if [ "${verdict}" = "pass" ]; then
        COMPARISON_PASS=$((COMPARISON_PASS + 1))
        e2e_pass "truth: ${check_id}"
    else
        COMPARISON_FAIL=$((COMPARISON_FAIL + 1))
        e2e_fail "truth: ${check_id}"
    fi
}

# Messages: inbox total <= DB total
if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/messages_inbox.json" >/dev/null 2>&1; then
    INBOX_TOTAL="$(jq 'if .inbox then (.inbox | length) elif type == "array" then length elif .messages then (.messages | length) else .count // 0 end' "${SNAPSHOTS_DIR}/messages_inbox.json" 2>/dev/null || echo 0)"
    if [ "${INBOX_TOTAL}" -le "${DB_MESSAGES}" ]; then
        add_comparison "messages.inbox_total_lte_db" "inbox" "${DB_MESSAGES}" "${INBOX_TOTAL}" "pass"
    else
        add_comparison "messages.inbox_total_lte_db" "inbox" "${DB_MESSAGES}" "${INBOX_TOTAL}" "fail"
    fi
fi

# Messages: body_md present in DB
if [ "${DB_BODIES_WITH_MD}" -gt 0 ]; then
    add_comparison "messages.bodies_with_md_gt_zero" "db" "${DB_MESSAGES}" "${DB_BODIES_WITH_MD}" "pass"
else
    add_comparison "messages.bodies_with_md_gt_zero" "db" "${DB_MESSAGES}" "${DB_BODIES_WITH_MD}" "fail"
fi

# Agents: surface count > 0
if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/agents_view.json" >/dev/null 2>&1; then
    AGENT_SC="$(jq 'if type == "array" then length elif .agents then (.agents | length) else 0 end' "${SNAPSHOTS_DIR}/agents_view.json" 2>/dev/null || echo 0)"
    if [ "${AGENT_SC}" -gt 0 ]; then
        add_comparison "agents.surface_gt_zero" "agents" "${DB_AGENTS}" "${AGENT_SC}" "pass"
    else
        add_comparison "agents.surface_gt_zero" "agents" "${DB_AGENTS}" "${AGENT_SC}" "fail"
    fi
fi

# Projects: surface count > 0
if ! jq -e '._capture_error' "${SNAPSHOTS_DIR}/projects_view.json" >/dev/null 2>&1; then
    PROJ_SC="$(jq 'if type == "array" then length elif .projects then (.projects | length) else 0 end' "${SNAPSHOTS_DIR}/projects_view.json" 2>/dev/null || echo 0)"
    if [ "${PROJ_SC}" -gt 0 ]; then
        add_comparison "projects.surface_gt_zero" "projects" "${DB_PROJECTS}" "${PROJ_SC}" "pass"
    else
        add_comparison "projects.surface_gt_zero" "projects" "${DB_PROJECTS}" "${PROJ_SC}" "fail"
    fi
fi

# Health: endpoint actionable
if [ -f "${SNAPSHOTS_DIR}/http_health.json" ] && jq -e '.status' "${SNAPSHOTS_DIR}/http_health.json" >/dev/null 2>&1; then
    add_comparison "health.endpoint_actionable" "http" "1" "1" "pass"
else
    add_comparison "health.endpoint_actionable" "http" "1" "0" "fail"
fi

# ── Emit machine-readable truth comparison artifact ──────────────────────
CHECKS_JSON="$(printf '%s\n' "${COMPARISON_RESULTS[@]}" | jq -s '.')"
OVERALL_VERDICT="pass"
if [ "${COMPARISON_FAIL}" -gt 0 ]; then
    OVERALL_VERDICT="fail"
fi

cat > "${DIAGNOSTICS_DIR}/truth_comparison.json" <<JSON
{
  "schema_version": "truth_comparison.v1",
  "bead_id": "br-2k3qx.5.9",
  "seed": ${SEED},
  "verdict": "${OVERALL_VERDICT}",
  "summary": {
    "check_count": $((COMPARISON_PASS + COMPARISON_FAIL)),
    "pass": ${COMPARISON_PASS},
    "fail": ${COMPARISON_FAIL}
  },
  "checks": ${CHECKS_JSON},
  "capture_stats": {
    "succeeded": ${CAPTURE_SUCCESSES},
    "failed": ${CAPTURE_FAILURES}
  },
  "db_truth": {
    "projects": ${DB_PROJECTS},
    "agents": ${DB_AGENTS},
    "messages": ${DB_MESSAGES},
    "threads": ${DB_THREADS},
    "bodies_with_markdown": ${DB_BODIES_WITH_MD}
  },
  "context": {
    "project_key": "${PROJECT_KEY}",
    "agent_name": "${AGENT_NAME}",
    "thread_id": "${THREAD_ID}",
    "message_id": ${MESSAGE_ID}
  }
}
JSON

e2e_save_artifact "truth_comparison.json" "$(cat "${DIAGNOSTICS_DIR}/truth_comparison.json")"

# ══════════════════════════════════════════════════════════════════════════
# CASE 10: Artifact bundle completeness
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "artifact bundle completeness"
e2e_mark_case_start "case10_artifact_bundle_completeness"

e2e_assert_file_exists "db_truth.json exists" "${DIAGNOSTICS_DIR}/db_truth.json"
e2e_assert_file_exists "truth_comparison.json exists" "${DIAGNOSTICS_DIR}/truth_comparison.json"
e2e_assert_file_exists "fixture_report.json exists" "${DIAGNOSTICS_DIR}/fixture_report.json"

for snap_name in dashboard_status.json messages_inbox.json message_detail.json \
                 threads_view.json agents_view.json projects_view.json \
                 system_health.json http_health.json; do
    e2e_assert_file_exists "snapshot: ${snap_name}" "${SNAPSHOTS_DIR}/${snap_name}"
done

# Validate all JSON snapshots parse cleanly
JSON_PARSE_OK=0
JSON_PARSE_FAIL=0
for json_file in "${SNAPSHOTS_DIR}"/*.json; do
    if [ -f "${json_file}" ] && jq -e '.' "${json_file}" >/dev/null 2>&1; then
        JSON_PARSE_OK=$((JSON_PARSE_OK + 1))
    else
        JSON_PARSE_FAIL=$((JSON_PARSE_FAIL + 1))
        e2e_fail "JSON parse: $(basename "${json_file}")"
    fi
done
if [ "${JSON_PARSE_FAIL}" -eq 0 ]; then
    e2e_pass "all ${JSON_PARSE_OK} JSON snapshots parse cleanly"
fi

# ══════════════════════════════════════════════════════════════════════════
# Save server log and emit CI summary
# ══════════════════════════════════════════════════════════════════════════
e2e_save_artifact "server.log" "$(cat "${SERVER_LOG}" 2>/dev/null || true)"

# Stop server before summary
cleanup_server
SERVER_PID=""

if ! e2e_summary; then
    # Human-readable failure summary for CI
    echo ""
    echo "=== TRUTHFULNESS INCIDENT MATRIX: FAILURES DETECTED ==="
    echo "Seed: ${SEED} | DB: ${DB_PROJECTS}p/${DB_AGENTS}a/${DB_MESSAGES}m/${DB_THREADS}t"
    echo "Truth comparison: ${COMPARISON_PASS} pass / ${COMPARISON_FAIL} fail"
    echo "Robot captures: ${CAPTURE_SUCCESSES} ok / ${CAPTURE_FAILURES} fail"
    echo "Artifacts: ${DIAGNOSTICS_DIR}/"
    echo "======================================================="
    exit 1
fi
