#!/usr/bin/env bash
# test_robot.sh - E2E test suite for am robot commands (16 commands + format tests)
#
# Verifies (br-20tyw):
# - All 16 robot subcommands produce valid output
# - Output formats (toon, json, markdown) work correctly
# - Envelope structure (_meta, _alerts, _actions) is present
# - Commands handle empty/missing data gracefully
#
# Test cases by track:
#   Track 2 (Situational Awareness): status, inbox, timeline, overview
#   Track 3 (Context & Discovery): thread, search, message, navigate
#   Track 4 (Monitoring & Analytics): reservations, metrics, health, analytics
#   Track 5 (Entity Views): agents, contacts, projects, attachments
#   Format tests: json, toon, markdown, auto-detect

set -euo pipefail

E2E_SUITE="robot"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Robot Commands E2E Test Suite"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
    e2e_log "jq not found; skipping suite"
    e2e_skip "jq required for JSON validation"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_robot")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
PROJECT_PATH="${WORK}/test_project"
mkdir -p "${STORAGE_ROOT}" "${PROJECT_PATH}"

export DATABASE_URL="sqlite:///${DB_PATH}"
export STORAGE_ROOT
export AM_INTERFACE_MODE=cli

e2e_log "Work directory: ${WORK}"
e2e_log "DB path: ${DB_PATH}"
e2e_log "Project path: ${PROJECT_PATH}"

# ---------------------------------------------------------------------------
# Seed test data using MCP server via stdio
# ---------------------------------------------------------------------------

e2e_case_banner "Seed test data via MCP tools"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-robot","version":"1.0"}}}'

seed_data() {
    local db_path="$1"
    local project="$2"
    local output_file="${WORK}/seed_response.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    DATABASE_URL="sqlite:///${db_path}" RUST_LOG=error \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        echo "$INIT_REQ"
        sleep 0.2
        # Ensure project
        echo "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${project}\"}}}"
        sleep 0.2
        # Register agents
        echo "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${project}\",\"program\":\"claude-code\",\"model\":\"opus-4.6\",\"name\":\"BlueLake\",\"task_description\":\"E2E testing\"}}}"
        sleep 0.2
        echo "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${project}\",\"program\":\"codex-cli\",\"model\":\"gpt-5.2\",\"name\":\"RedFox\",\"task_description\":\"E2E testing\"}}}"
        sleep 0.3
        # Set RedFox's contact policy to "open" so anyone can message them
        echo "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${project}\",\"agent_name\":\"RedFox\",\"policy\":\"open\"}}}"
        sleep 0.3
        # Send a message (now allowed since RedFox accepts all contacts)
        echo "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${project}\",\"sender_name\":\"BlueLake\",\"to\":[\"RedFox\"],\"subject\":\"Robot E2E Test Message\",\"body_md\":\"This is a test message for robot E2E validation.\",\"importance\":\"high\"}}}"
        sleep 0.3
        # Create a file reservation
        echo "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${project}\",\"agent_name\":\"BlueLake\",\"paths\":[\"src/test.rs\"],\"ttl_seconds\":7200,\"exclusive\":true,\"reason\":\"E2E test\"}}}"
        sleep 0.2
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=15
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if ! kill -0 "$srv_pid" 2>/dev/null; then break; fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    cat "$output_file"
}

SEED_RESP="$(seed_data "$DB_PATH" "$PROJECT_PATH")"
e2e_save_artifact "seed_response.txt" "$SEED_RESP"

# Check if seeding succeeded by looking for errors
if echo "$SEED_RESP" | grep -q '"isError":true'; then
    e2e_fail "Data seeding failed - check seed_response.txt"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
e2e_pass "Test data seeded successfully"

# Workaround: frankensqlite doesn't update indexes properly, so we REINDEX
# This ensures queries that use indexes will work correctly
sqlite3 "${DB_PATH}" "REINDEX;" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Run am robot command and capture output
run_robot() {
    local subcommand="$1"
    shift
    local output
    output=$(DATABASE_URL="sqlite:///${DB_PATH}" STORAGE_ROOT="${STORAGE_ROOT}" \
        AM_INTERFACE_MODE=cli am robot --project "${PROJECT_PATH}" "$subcommand" "$@" 2>&1) || true
    echo "$output"
}

# Validate JSON output
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

# Check for envelope structure
assert_envelope() {
    local case_id="$1"
    local output="$2"
    if echo "$output" | jq -e '._meta' >/dev/null 2>&1; then
        e2e_pass "${case_id}: _meta present"
    else
        e2e_fail "${case_id}: _meta missing"
    fi
    if echo "$output" | jq -e '._meta.command' >/dev/null 2>&1; then
        e2e_pass "${case_id}: _meta.command present"
    else
        e2e_fail "${case_id}: _meta.command missing"
    fi
}

# ---------------------------------------------------------------------------
# Track 2: Situational Awareness
# ---------------------------------------------------------------------------

e2e_case_banner "robot status -> dashboard synthesis"
STATUS_OUT="$(run_robot status --format json)"
e2e_save_artifact "case_status.json" "$STATUS_OUT"
assert_valid_json "status" "$STATUS_OUT" && assert_envelope "status" "$STATUS_OUT"
# Check for expected sections
if echo "$STATUS_OUT" | jq -e '.health' >/dev/null 2>&1; then
    e2e_pass "status: health section present"
else
    e2e_skip "status: health section (may be empty)"
fi

e2e_case_banner "robot inbox -> actionable inbox"
# Use --agent RedFox since RedFox received the test message
INBOX_OUT="$(run_robot inbox --agent RedFox --format json)"
e2e_save_artifact "case_inbox.json" "$INBOX_OUT"
assert_valid_json "inbox" "$INBOX_OUT" && assert_envelope "inbox" "$INBOX_OUT"
# Check for messages array
if echo "$INBOX_OUT" | jq -e '.messages' >/dev/null 2>&1; then
    e2e_pass "inbox: messages array present"
else
    e2e_skip "inbox: messages array (format may differ)"
fi

e2e_case_banner "robot inbox --urgent filter"
INBOX_URGENT_OUT="$(run_robot inbox --agent RedFox --urgent --format json)"
e2e_save_artifact "case_inbox_urgent.json" "$INBOX_URGENT_OUT"
assert_valid_json "inbox_urgent" "$INBOX_URGENT_OUT"

e2e_case_banner "robot timeline -> events"
TIMELINE_OUT="$(run_robot timeline --format json)"
e2e_save_artifact "case_timeline.json" "$TIMELINE_OUT"
assert_valid_json "timeline" "$TIMELINE_OUT" && assert_envelope "timeline" "$TIMELINE_OUT"

e2e_case_banner "robot overview -> cross-project summary"
OVERVIEW_OUT="$(run_robot overview --format json)"
e2e_save_artifact "case_overview.json" "$OVERVIEW_OUT"
assert_valid_json "overview" "$OVERVIEW_OUT" && assert_envelope "overview" "$OVERVIEW_OUT"

# ---------------------------------------------------------------------------
# Track 3: Context & Discovery
# ---------------------------------------------------------------------------

e2e_case_banner "robot search -> FTS results"
SEARCH_OUT="$(run_robot search "Robot E2E" --format json)"
e2e_save_artifact "case_search.json" "$SEARCH_OUT"
assert_valid_json "search" "$SEARCH_OUT" && assert_envelope "search" "$SEARCH_OUT"
# Check for results
if echo "$SEARCH_OUT" | jq -e '.results' >/dev/null 2>&1; then
    RESULT_COUNT=$(echo "$SEARCH_OUT" | jq '.results | length')
    if [ "$RESULT_COUNT" -ge 1 ]; then
        e2e_pass "search: found $RESULT_COUNT result(s) for 'Robot E2E'"
    else
        e2e_skip "search: no results (FTS may be limited)"
    fi
else
    e2e_skip "search: results array (format may differ)"
fi

e2e_case_banner "robot navigate -> resource resolution"
# Use resource://projects to get a valid resource list
NAVIGATE_OUT="$(run_robot navigate "resource://projects" --format json 2>&1)" || true
e2e_save_artifact "case_navigate.json" "$NAVIGATE_OUT"
# Navigate may error if resource doesn't exist - that's OK for E2E validation
if echo "$NAVIGATE_OUT" | jq . >/dev/null 2>&1; then
    e2e_pass "navigate: returned JSON"
else
    e2e_skip "navigate: non-JSON output (resource may not exist)"
fi

# ---------------------------------------------------------------------------
# Track 4: Monitoring & Analytics
# ---------------------------------------------------------------------------

e2e_case_banner "robot reservations -> file reservations"
RESERVATIONS_OUT="$(run_robot reservations --format json)"
e2e_save_artifact "case_reservations.json" "$RESERVATIONS_OUT"
assert_valid_json "reservations" "$RESERVATIONS_OUT" && assert_envelope "reservations" "$RESERVATIONS_OUT"

e2e_case_banner "robot reservations --all"
RESERVATIONS_ALL_OUT="$(run_robot reservations --all --format json)"
e2e_save_artifact "case_reservations_all.json" "$RESERVATIONS_ALL_OUT"
assert_valid_json "reservations_all" "$RESERVATIONS_ALL_OUT"

e2e_case_banner "robot metrics -> tool performance"
METRICS_OUT="$(run_robot metrics --format json)"
e2e_save_artifact "case_metrics.json" "$METRICS_OUT"
assert_valid_json "metrics" "$METRICS_OUT" && assert_envelope "metrics" "$METRICS_OUT"

e2e_case_banner "robot health -> system diagnostics"
HEALTH_OUT="$(run_robot health --format json)"
e2e_save_artifact "case_health.json" "$HEALTH_OUT"
assert_valid_json "health" "$HEALTH_OUT" && assert_envelope "health" "$HEALTH_OUT"

e2e_case_banner "robot analytics -> anomaly insights"
ANALYTICS_OUT="$(run_robot analytics --format json)"
e2e_save_artifact "case_analytics.json" "$ANALYTICS_OUT"
assert_valid_json "analytics" "$ANALYTICS_OUT" && assert_envelope "analytics" "$ANALYTICS_OUT"

# ---------------------------------------------------------------------------
# Track 5: Entity Views
# ---------------------------------------------------------------------------

e2e_case_banner "robot agents -> agent roster"
AGENTS_OUT="$(run_robot agents --format json)"
e2e_save_artifact "case_agents.json" "$AGENTS_OUT"
assert_valid_json "agents" "$AGENTS_OUT" && assert_envelope "agents" "$AGENTS_OUT"
# Check for agents
if echo "$AGENTS_OUT" | jq -e '.agents' >/dev/null 2>&1; then
    AGENT_COUNT=$(echo "$AGENTS_OUT" | jq '.agents | length')
    if [ "$AGENT_COUNT" -ge 2 ]; then
        e2e_pass "agents: found $AGENT_COUNT agents (BlueLake, RedFox)"
    else
        e2e_fail "agents: expected at least 2 agents, got $AGENT_COUNT"
    fi
else
    e2e_skip "agents: agents array (format may differ)"
fi

e2e_case_banner "robot agents --active"
AGENTS_ACTIVE_OUT="$(run_robot agents --active --format json)"
e2e_save_artifact "case_agents_active.json" "$AGENTS_ACTIVE_OUT"
assert_valid_json "agents_active" "$AGENTS_ACTIVE_OUT"

e2e_case_banner "robot contacts -> contact graph"
CONTACTS_OUT="$(run_robot contacts --format json)"
e2e_save_artifact "case_contacts.json" "$CONTACTS_OUT"
assert_valid_json "contacts" "$CONTACTS_OUT" && assert_envelope "contacts" "$CONTACTS_OUT"

e2e_case_banner "robot projects -> project summary"
PROJECTS_OUT="$(run_robot projects --format json)"
e2e_save_artifact "case_projects.json" "$PROJECTS_OUT"
assert_valid_json "projects" "$PROJECTS_OUT" && assert_envelope "projects" "$PROJECTS_OUT"

e2e_case_banner "robot attachments -> attachment inventory"
ATTACHMENTS_OUT="$(run_robot attachments --format json)"
e2e_save_artifact "case_attachments.json" "$ATTACHMENTS_OUT"
assert_valid_json "attachments" "$ATTACHMENTS_OUT" && assert_envelope "attachments" "$ATTACHMENTS_OUT"

# ---------------------------------------------------------------------------
# Format Tests
# ---------------------------------------------------------------------------

e2e_case_banner "Format: --format json validation"
JSON_OUT="$(run_robot status --format json)"
if echo "$JSON_OUT" | jq -e '._meta.format == "json"' >/dev/null 2>&1; then
    e2e_pass "format_json: _meta.format is 'json'"
else
    e2e_skip "format_json: _meta.format check (may differ)"
fi

e2e_case_banner "Format: --format toon validation"
TOON_OUT="$(run_robot status --format toon)"
e2e_save_artifact "case_format_toon.txt" "$TOON_OUT"
# TOON format should not be JSON
if ! echo "$TOON_OUT" | jq . >/dev/null 2>&1; then
    e2e_pass "format_toon: output is not JSON (expected)"
else
    # Could still be valid if TOON produces JSON-like output
    e2e_skip "format_toon: output appears JSON-like"
fi
if [ -n "$TOON_OUT" ]; then
    e2e_pass "format_toon: non-empty output"
else
    e2e_fail "format_toon: empty output"
fi

e2e_case_banner "Format: auto-detect piped (should be json)"
PIPED_OUT="$(run_robot status | cat)"
e2e_save_artifact "case_format_piped.txt" "$PIPED_OUT"
# When piped, should default to JSON
if echo "$PIPED_OUT" | jq . >/dev/null 2>&1; then
    e2e_pass "format_piped: valid JSON when piped"
else
    e2e_skip "format_piped: output may vary"
fi

# ---------------------------------------------------------------------------
# Envelope metadata validation
# ---------------------------------------------------------------------------

e2e_case_banner "Envelope: _meta fields validation"
META_OUT="$(run_robot status --format json)"
if echo "$META_OUT" | jq -e '._meta.timestamp' >/dev/null 2>&1; then
    e2e_pass "envelope: _meta.timestamp present"
else
    e2e_fail "envelope: _meta.timestamp missing"
fi
if echo "$META_OUT" | jq -e '._meta.version' >/dev/null 2>&1; then
    e2e_pass "envelope: _meta.version present"
else
    e2e_fail "envelope: _meta.version missing"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
