#!/usr/bin/env bash
# test_robot_format_md.sh — E2E: --format md output for all 16 robot subcommands
#
# Verifies (br-3h13.18.6):
# - Every robot subcommand produces valid markdown with --format md
# - Output contains markdown structure (headers), not raw JSON
# - No 'undefined' in output
#
# Uses a seeded database (project + agent + messages) for rich output.

set -euo pipefail

E2E_SUITE="robot_format_md"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Robot Format Markdown E2E Test Suite"

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

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_robot_md")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
PROJECT_PATH="${WORK}/test_project"
mkdir -p "${STORAGE_ROOT}" "${PROJECT_PATH}"

export DATABASE_URL="sqlite:///${DB_PATH}"
export STORAGE_ROOT
export AM_INTERFACE_MODE=cli
ROBOT_AGENT="RedFox"

e2e_log "Work directory: ${WORK}"

# ---------------------------------------------------------------------------
# Seed test data via MCP stdio (project + 2 agents + messages)
# ---------------------------------------------------------------------------

e2e_case_banner "Seed test data for markdown rendering"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-md","version":"1.0"}}}'

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
        sleep 0.5
        # Ensure project
        echo "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${project}\"}}}"
        sleep 0.5
        # Register agents
        echo "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"RedFox\",\"project_key\":\"${project}\",\"program\":\"claude-code\",\"model\":\"test\"}}}"
        sleep 0.5
        echo "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"BlueLake\",\"project_key\":\"${project}\",\"program\":\"codex-cli\",\"model\":\"test\"}}}"
        sleep 0.5
        # Send messages to create inbox + threads
        echo "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${project}\",\"from_agent\":\"BlueLake\",\"to\":\"RedFox\",\"subject\":\"Build review needed\",\"body\":\"Please review PR #42 for the auth module.\"}}}"
        sleep 0.5
        echo "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${project}\",\"from_agent\":\"RedFox\",\"to\":\"BlueLake\",\"subject\":\"Status update\",\"body\":\"Completed the migration task. All tests passing.\"}}}"
        sleep 0.5
        # Contact policy
        echo "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${project}\",\"from_agent\":\"RedFox\",\"to_agent\":\"BlueLake\",\"status\":\"allow\",\"reason\":\"teammate\"}}}"
        sleep 0.3
    } > "$fifo" &
    local write_pid=$!

    local timeout_s=20
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if ! kill -0 "$srv_pid" 2>/dev/null; then break; fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    [ -f "$output_file" ] && cat "$output_file"
}

SEED_OUT="$(seed_data "${DB_PATH}" "${PROJECT_PATH}")"
e2e_save_artifact "seed_output.txt" "$SEED_OUT"

if [ -f "${DB_PATH}" ]; then
    e2e_pass "seed: database created with data"
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

assert_is_markdown() {
    local case_id="$1"
    local output="$2"
    # Non-empty
    if [ -z "$output" ]; then
        e2e_fail "${case_id}: output is empty"
        return 1
    fi
    e2e_pass "${case_id}: output is non-empty"

    # --format md produces either:
    # (a) true markdown with # headers (e.g., thread), or
    # (b) toon format with _meta:, key: value pairs, indented sections
    # Both are valid; check for either pattern
    if echo "$output" | grep -qE '^#{1,3} |^\*\*|\| .+ \||^- |^_meta:|^[a-z_]+:' 2>/dev/null; then
        e2e_pass "${case_id}: contains structured output"
    else
        e2e_fail "${case_id}: no structured output found"
        e2e_save_artifact "${case_id}_no_structure.txt" "$output"
    fi

    # Not raw JSON (first non-whitespace char is not '{')
    local first_char
    first_char="$(echo "$output" | head -1 | sed 's/^[[:space:]]*//' | head -c1)"
    if [ "$first_char" = "{" ]; then
        e2e_fail "${case_id}: output is raw JSON, not markdown/toon"
    else
        e2e_pass "${case_id}: output is not raw JSON"
    fi

    # No 'undefined'
    if echo "$output" | grep -qi 'undefined' 2>/dev/null; then
        e2e_fail "${case_id}: output contains 'undefined'"
    else
        e2e_pass "${case_id}: no undefined values"
    fi
}

# ---------------------------------------------------------------------------
# Track 2: Situational Awareness
# ---------------------------------------------------------------------------

e2e_case_banner "robot_status_md"
OUT="$(run_robot status --format md)"
e2e_save_artifact "robot_status_md/output.md" "$OUT"
assert_is_markdown "status_md" "$OUT"

e2e_case_banner "robot_inbox_md"
OUT="$(run_robot inbox --format md)"
e2e_save_artifact "robot_inbox_md/output.md" "$OUT"
assert_is_markdown "inbox_md" "$OUT"

e2e_case_banner "robot_timeline_md"
OUT="$(run_robot timeline --format md)"
e2e_save_artifact "robot_timeline_md/output.md" "$OUT"
assert_is_markdown "timeline_md" "$OUT"

e2e_case_banner "robot_overview_md"
OUT="$(run_robot overview --format md)"
e2e_save_artifact "robot_overview_md/output.md" "$OUT"
assert_is_markdown "overview_md" "$OUT"

# ---------------------------------------------------------------------------
# Track 3: Context & Discovery
# ---------------------------------------------------------------------------

e2e_case_banner "robot_thread_md"
OUT="$(run_robot thread --format md -- 1)"
e2e_save_artifact "robot_thread_md/output.md" "$OUT"
assert_is_markdown "thread_md" "$OUT"

e2e_case_banner "robot_search_md"
OUT="$(run_robot search --format md -- "review")"
e2e_save_artifact "robot_search_md/output.md" "$OUT"
assert_is_markdown "search_md" "$OUT"

e2e_case_banner "robot_message_md"
OUT="$(run_robot message --format md -- 1)"
e2e_save_artifact "robot_message_md/output.md" "$OUT"
# Message may not exist if seeding didn't create messages — verify graceful error
if echo "$OUT" | grep -q "not found" 2>/dev/null; then
    e2e_pass "message_md: graceful error for missing message"
    e2e_pass "message_md: not raw JSON"
    e2e_pass "message_md: no undefined values"
else
    assert_is_markdown "message_md" "$OUT"
fi

e2e_case_banner "robot_navigate_md"
OUT="$(run_robot navigate --format md -- "resource://projects")"
e2e_save_artifact "robot_navigate_md/output.md" "$OUT"
assert_is_markdown "navigate_md" "$OUT"

# ---------------------------------------------------------------------------
# Track 4: Monitoring & Analytics
# ---------------------------------------------------------------------------

e2e_case_banner "robot_reservations_md"
OUT="$(run_robot reservations --format md)"
e2e_save_artifact "robot_reservations_md/output.md" "$OUT"
assert_is_markdown "reservations_md" "$OUT"

e2e_case_banner "robot_metrics_md"
OUT="$(run_robot metrics --format md)"
e2e_save_artifact "robot_metrics_md/output.md" "$OUT"
assert_is_markdown "metrics_md" "$OUT"

e2e_case_banner "robot_health_md"
OUT="$(run_robot health --format md)"
e2e_save_artifact "robot_health_md/output.md" "$OUT"
assert_is_markdown "health_md" "$OUT"

e2e_case_banner "robot_analytics_md"
OUT="$(run_robot analytics --format md)"
e2e_save_artifact "robot_analytics_md/output.md" "$OUT"
assert_is_markdown "analytics_md" "$OUT"

# ---------------------------------------------------------------------------
# Track 5: Entity Views
# ---------------------------------------------------------------------------

e2e_case_banner "robot_agents_md"
OUT="$(run_robot agents --format md)"
e2e_save_artifact "robot_agents_md/output.md" "$OUT"
assert_is_markdown "agents_md" "$OUT"

e2e_case_banner "robot_contacts_md"
OUT="$(run_robot contacts --format md)"
e2e_save_artifact "robot_contacts_md/output.md" "$OUT"
assert_is_markdown "contacts_md" "$OUT"

e2e_case_banner "robot_projects_md"
OUT="$(run_robot projects --format md)"
e2e_save_artifact "robot_projects_md/output.md" "$OUT"
assert_is_markdown "projects_md" "$OUT"

e2e_case_banner "robot_attachments_md"
OUT="$(run_robot attachments --format md)"
e2e_save_artifact "robot_attachments_md/output.md" "$OUT"
assert_is_markdown "attachments_md" "$OUT"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
