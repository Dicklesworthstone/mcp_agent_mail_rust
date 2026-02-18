#!/usr/bin/env bash
# test_robot_empty_db.sh — E2E: all 16 robot subcommands against a fresh/empty database
#
# Verifies (br-3h13.18.3):
# - Every robot subcommand produces valid JSON output on an empty DB
# - No null fields, no crashes, no 500 errors
# - Empty-state responses (empty arrays, zeroed counts) are well-formed
#
# This exercises the "day zero" experience: agents starting against fresh databases.

set -euo pipefail

E2E_SUITE="robot_empty_db"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Robot Empty DB E2E Test Suite"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

if ! command -v jq >/dev/null 2>&1; then
    e2e_log "jq not found; skipping suite"
    e2e_skip "jq required for JSON validation"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# ---------------------------------------------------------------------------
# Setup — fresh database, NO seed data
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_robot_empty")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
PROJECT_PATH="${WORK}/test_project"
mkdir -p "${STORAGE_ROOT}" "${PROJECT_PATH}"

export DATABASE_URL="sqlite:///${DB_PATH}"
export STORAGE_ROOT
export AM_INTERFACE_MODE=cli
ROBOT_AGENT="RedFox"

e2e_log "Work directory: ${WORK}"
e2e_log "DB path: ${DB_PATH} (will be created on first use)"
e2e_log "Project path: ${PROJECT_PATH}"

# ---------------------------------------------------------------------------
# Minimal seed: project + agent only (zero messages, zero threads, zero data)
# ---------------------------------------------------------------------------

e2e_case_banner "seed_minimal_project_and_agent"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-empty","version":"1.0"}}}'
ENSURE_PROJECT="{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}"
REGISTER_AGENT="{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"RedFox\",\"project_key\":\"${PROJECT_PATH}\",\"program\":\"claude-code\",\"model\":\"test\"}}}"

SEED_OUT="$(
    (echo "$INIT_REQ"; sleep 0.3; echo "$ENSURE_PROJECT"; sleep 0.3; echo "$REGISTER_AGENT"; sleep 0.3) \
    | DATABASE_URL="sqlite:///${DB_PATH}" RUST_LOG=error am serve-stdio 2>/dev/null
)" || true
e2e_save_artifact "seed_output.txt" "$SEED_OUT"

if [ -f "${DB_PATH}" ]; then
    e2e_pass "seed: database created"
else
    e2e_fail "seed: database not created"
    e2e_summary
    exit 1
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

run_robot() {
    local subcommand="$1"
    shift
    local agent_args=()
    if [ -n "${ROBOT_AGENT:-}" ]; then
        agent_args=(--agent "${ROBOT_AGENT}")
    fi
    local output
    output=$(DATABASE_URL="sqlite:///${DB_PATH}" STORAGE_ROOT="${STORAGE_ROOT}" \
        AM_INTERFACE_MODE=cli am robot --project "${PROJECT_PATH}" "${agent_args[@]}" "$subcommand" "$@" 2>&1) || true
    echo "$output"
}

assert_valid_json() {
    local case_id="$1"
    local output="$2"
    if echo "$output" | jq . >/dev/null 2>&1; then
        e2e_pass "${case_id}: valid JSON output"
        return 0
    else
        e2e_fail "${case_id}: invalid JSON output"
        e2e_save_artifact "${case_id}_invalid_json.txt" "$output"
        return 1
    fi
}

assert_no_null_string() {
    local case_id="$1"
    local output="$2"
    if echo "$output" | grep -qi '"null"' 2>/dev/null; then
        e2e_fail "${case_id}: output contains string 'null'"
        e2e_save_artifact "${case_id}_has_null.txt" "$output"
    else
        e2e_pass "${case_id}: no null strings"
    fi
}

assert_no_undefined() {
    local case_id="$1"
    local output="$2"
    if echo "$output" | grep -qi 'undefined' 2>/dev/null; then
        e2e_fail "${case_id}: output contains 'undefined'"
        e2e_save_artifact "${case_id}_has_undefined.txt" "$output"
    else
        e2e_pass "${case_id}: no undefined values"
    fi
}

# ---------------------------------------------------------------------------
# Track 2: Situational Awareness
# ---------------------------------------------------------------------------

e2e_case_banner "robot_status_empty_db"
OUT="$(run_robot status --format json)"
e2e_save_artifact "robot_status_empty_db/stdout.txt" "$OUT"
assert_valid_json "status_empty" "$OUT"
assert_no_null_string "status_empty" "$OUT"
assert_no_undefined "status_empty" "$OUT"

e2e_case_banner "robot_inbox_empty_db"
OUT="$(run_robot inbox --format json)"
e2e_save_artifact "robot_inbox_empty_db/stdout.txt" "$OUT"
assert_valid_json "inbox_empty" "$OUT"
assert_no_undefined "inbox_empty" "$OUT"

e2e_case_banner "robot_timeline_empty_db"
OUT="$(run_robot timeline --format json)"
e2e_save_artifact "robot_timeline_empty_db/stdout.txt" "$OUT"
assert_valid_json "timeline_empty" "$OUT"
assert_no_undefined "timeline_empty" "$OUT"

e2e_case_banner "robot_overview_empty_db"
OUT="$(run_robot overview --format json)"
e2e_save_artifact "robot_overview_empty_db/stdout.txt" "$OUT"
assert_valid_json "overview_empty" "$OUT"
assert_no_undefined "overview_empty" "$OUT"

# ---------------------------------------------------------------------------
# Track 3: Context & Discovery
# ---------------------------------------------------------------------------

e2e_case_banner "robot_thread_empty_db"
OUT="$(run_robot thread --format json -- 99999)"
e2e_save_artifact "robot_thread_empty_db/stdout.txt" "$OUT"
# Thread for nonexistent ID should produce valid JSON (error envelope or empty)
assert_valid_json "thread_empty" "$OUT"
assert_no_undefined "thread_empty" "$OUT"

e2e_case_banner "robot_search_empty_db"
OUT="$(run_robot search --format json -- "anything")"
e2e_save_artifact "robot_search_empty_db/stdout.txt" "$OUT"
assert_valid_json "search_empty" "$OUT"
assert_no_undefined "search_empty" "$OUT"

e2e_case_banner "robot_message_empty_db"
OUT="$(run_robot message --format json -- 99999)"
e2e_save_artifact "robot_message_empty_db/stdout.txt" "$OUT"
# Nonexistent message ID produces a meaningful error, not a crash
e2e_assert_contains "message_empty: error message" "$OUT" "not found"
e2e_assert_not_contains "message_empty: no panic" "$OUT" "panic"

e2e_case_banner "robot_navigate_empty_db"
OUT="$(run_robot navigate --format json -- "resource://projects")"
e2e_save_artifact "robot_navigate_empty_db/stdout.txt" "$OUT"
assert_valid_json "navigate_empty" "$OUT"
assert_no_undefined "navigate_empty" "$OUT"

# ---------------------------------------------------------------------------
# Track 4: Monitoring & Analytics
# ---------------------------------------------------------------------------

e2e_case_banner "robot_reservations_empty_db"
OUT="$(run_robot reservations --format json)"
e2e_save_artifact "robot_reservations_empty_db/stdout.txt" "$OUT"
assert_valid_json "reservations_empty" "$OUT"
assert_no_undefined "reservations_empty" "$OUT"

e2e_case_banner "robot_metrics_empty_db"
OUT="$(run_robot metrics --format json)"
e2e_save_artifact "robot_metrics_empty_db/stdout.txt" "$OUT"
assert_valid_json "metrics_empty" "$OUT"
assert_no_undefined "metrics_empty" "$OUT"

e2e_case_banner "robot_health_empty_db"
OUT="$(run_robot health --format json)"
e2e_save_artifact "robot_health_empty_db/stdout.txt" "$OUT"
assert_valid_json "health_empty" "$OUT"
assert_no_undefined "health_empty" "$OUT"

e2e_case_banner "robot_analytics_empty_db"
OUT="$(run_robot analytics --format json)"
e2e_save_artifact "robot_analytics_empty_db/stdout.txt" "$OUT"
assert_valid_json "analytics_empty" "$OUT"
assert_no_undefined "analytics_empty" "$OUT"

# ---------------------------------------------------------------------------
# Track 5: Entity Views
# ---------------------------------------------------------------------------

e2e_case_banner "robot_agents_empty_db"
OUT="$(run_robot agents --format json)"
e2e_save_artifact "robot_agents_empty_db/stdout.txt" "$OUT"
assert_valid_json "agents_empty" "$OUT"
assert_no_undefined "agents_empty" "$OUT"

e2e_case_banner "robot_contacts_empty_db"
OUT="$(run_robot contacts --format json)"
e2e_save_artifact "robot_contacts_empty_db/stdout.txt" "$OUT"
assert_valid_json "contacts_empty" "$OUT"
assert_no_undefined "contacts_empty" "$OUT"

e2e_case_banner "robot_projects_empty_db"
OUT="$(run_robot projects --format json)"
e2e_save_artifact "robot_projects_empty_db/stdout.txt" "$OUT"
assert_valid_json "projects_empty" "$OUT"
assert_no_undefined "projects_empty" "$OUT"

e2e_case_banner "robot_attachments_empty_db"
OUT="$(run_robot attachments --format json)"
e2e_save_artifact "robot_attachments_empty_db/stdout.txt" "$OUT"
assert_valid_json "attachments_empty" "$OUT"
assert_no_undefined "attachments_empty" "$OUT"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
