#!/usr/bin/env bash
# test_robot_format_md.sh — E2E: strict markdown support across 16 robot subcommands
#
# Verifies (br-3h13.18.6):
# - Thread/message produce actual markdown with the seeded bodies
# - Other commands reject --format md and produce structured --format toon
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
# Port zero cannot have a listener; keep robot reads on this private mailbox.
export HTTP_HOST=127.0.0.1 HTTP_PORT=0 HTTP_PATH=/mcp/
export AGENT_MAIL_URL=http://127.0.0.1:0/mcp/
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
    E2E_STDIO_TIMEOUT_SECONDS=10 RUST_LOG=error \
        e2e_stdio_session "$db_path" \
        "$INIT_REQ" \
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${project}\"}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"RedFox\",\"project_key\":\"${project}\",\"program\":\"claude-code\",\"model\":\"test\"}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"BlueLake\",\"project_key\":\"${project}\",\"program\":\"codex-cli\",\"model\":\"test\"}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${project}\",\"sender_name\":\"BlueLake\",\"to\":[\"RedFox\"],\"subject\":\"Build review needed\",\"body_md\":\"Please review PR #42 for the auth module.\",\"thread_id\":\"robot-md-thread\"}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${project}\",\"sender_name\":\"RedFox\",\"to\":[\"BlueLake\"],\"subject\":\"Status update\",\"body_md\":\"Completed the migration task. All tests passing.\",\"thread_id\":\"robot-md-thread\"}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"set_contact_policy\",\"arguments\":{\"project_key\":\"${project}\",\"agent_name\":\"RedFox\",\"policy\":\"open\"}}}"
}

SEED_OUT="$(seed_data "${DB_PATH}" "${PROJECT_PATH}")"
e2e_save_artifact "seed_output.txt" "$SEED_OUT"

if [ -f "${DB_PATH}" ]; then
    SEED_METADATA="$(RESP="$SEED_OUT" python3 - "$DB_PATH" "$PROJECT_PATH" <<'PY'
import json
import os
import sqlite3
import sys

responses = [json.loads(line) for line in os.environ["RESP"].splitlines() if line.strip()]
by_id = {item["id"]: item for item in responses if "id" in item}
for request_id in range(1, 8):
    response = by_id[request_id]
    assert "result" in response and "error" not in response, response
    assert not response["result"].get("isError", False), response

def payload(request_id):
    return json.loads(by_id[request_id]["result"]["content"][0]["text"])

project_id = payload(2)["id"]
messages = [payload(request_id)["deliveries"][0]["payload"] for request_id in (5, 6)]
with sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True) as conn:
    assert conn.execute("SELECT id FROM projects WHERE human_key = ?", (sys.argv[2],)).fetchall() == [(project_id,)]
    assert conn.execute("SELECT name FROM agents WHERE project_id = ? ORDER BY name", (project_id,)).fetchall() == [("BlueLake",), ("RedFox",)]
    for message, subject, body, recipient in zip(messages,
            ("Build review needed", "Status update"),
            ("Please review PR #42 for the auth module.", "Completed the migration task. All tests passing."),
            ("RedFox", "BlueLake")):
        message_id = message["id"]
        assert isinstance(message_id, int) and not isinstance(message_id, bool) and message_id > 0
        assert conn.execute("SELECT subject, body_md, thread_id FROM messages WHERE id = ? AND project_id = ?", (message_id, project_id)).fetchall() == [(subject, body, "robot-md-thread")]
        assert conn.execute("SELECT a.name FROM message_recipients mr JOIN agents a ON a.id = mr.agent_id WHERE mr.message_id = ? AND mr.kind = 'to'", (message_id,)).fetchall() == [(recipient,)]
print(f'{messages[0]["id"]}\t{messages[0]["thread_id"]}')
PY
)"
    IFS=$'\t' read -r MESSAGE_ID THREAD_ID <<< "$SEED_METADATA"
    e2e_pass "seed: project, agents, messages, thread, and recipients persisted"
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
    ROBOT_EXIT_STATUS=0
    ROBOT_OUTPUT=$(DATABASE_URL="sqlite:///${DB_PATH}" STORAGE_ROOT="${STORAGE_ROOT}" \
        AM_INTERFACE_MODE=cli am robot --project "${PROJECT_PATH}" "${agent_args[@]}" "$subcommand" "$@" 2>&1) || ROBOT_EXIT_STATUS=$?
}

assert_structured_output() {
    local case_id="$1"
    local output="$2"
    local format="$3"
    # Non-empty
    if [ -z "$output" ]; then
        e2e_fail "${case_id}: output is empty"
        return 1
    fi
    e2e_pass "${case_id}: output is non-empty"

    local structure='^_meta:'
    if [ "$format" = md ]; then
        structure='^#{1,3} '
    fi
    if echo "$output" | grep -qE "$structure"; then
        e2e_pass "${case_id}: contains ${format} structure"
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

assert_supported_status() {
    local case_id="$1"
    if [ "$ROBOT_EXIT_STATUS" -eq 0 ]; then
        e2e_pass "${case_id}: supported format succeeded (exit ${ROBOT_EXIT_STATUS})"
    else
        e2e_fail "${case_id}: supported format exited ${ROBOT_EXIT_STATUS}"
    fi
}

check_unsupported_markdown() {
    local case_id="$1"
    local subcommand="$2"
    shift 2
    run_robot "$subcommand" --format md "$@"
    e2e_save_artifact "robot_${case_id}/output.md" "$ROBOT_OUTPUT"
    if [ "$ROBOT_EXIT_STATUS" -ne 0 ]; then
        e2e_pass "${case_id}: unsupported markdown rejected (exit ${ROBOT_EXIT_STATUS})"
    else
        e2e_fail "${case_id}: unsupported markdown returned success"
    fi
    e2e_assert_contains "${case_id}: documented format refusal" "$ROBOT_OUTPUT" \
        '--format md is only supported for `am robot thread` and `am robot message`'

    run_robot "$subcommand" --format toon "$@"
    e2e_save_artifact "robot_${case_id}/output.toon" "$ROBOT_OUTPUT"
    assert_supported_status "${case_id} toon"
    assert_structured_output "${case_id} toon" "$ROBOT_OUTPUT" toon
}

# ---------------------------------------------------------------------------
# Track 2: Situational Awareness
# ---------------------------------------------------------------------------

e2e_case_banner "robot_status_md"
check_unsupported_markdown status_md status

e2e_case_banner "robot_inbox_md"
check_unsupported_markdown inbox_md inbox

e2e_case_banner "robot_timeline_md"
check_unsupported_markdown timeline_md timeline

e2e_case_banner "robot_overview_md"
check_unsupported_markdown overview_md overview

# ---------------------------------------------------------------------------
# Track 3: Context & Discovery
# ---------------------------------------------------------------------------

e2e_case_banner "robot_thread_md"
run_robot thread --format md -- "$THREAD_ID"
OUT="$ROBOT_OUTPUT"
e2e_save_artifact "robot_thread_md/output.md" "$OUT"
assert_supported_status thread_md
assert_structured_output thread_md "$OUT" md
e2e_assert_contains "thread_md: first seeded subject" "$OUT" "Build review needed"
e2e_assert_contains "thread_md: second seeded subject" "$OUT" "Status update"
e2e_assert_contains "thread_md: first seeded body" "$OUT" "Please review PR #42 for the auth module."
e2e_assert_contains "thread_md: second seeded body" "$OUT" "Completed the migration task. All tests passing."

e2e_case_banner "robot_search_md"
check_unsupported_markdown search_md search -- "review"

e2e_case_banner "robot_message_md"
run_robot message --format md -- "$MESSAGE_ID"
OUT="$ROBOT_OUTPUT"
e2e_save_artifact "robot_message_md/output.md" "$OUT"
assert_supported_status message_md
assert_structured_output message_md "$OUT" md
e2e_assert_contains "message_md: seeded subject" "$OUT" "Build review needed"
e2e_assert_contains "message_md: seeded body" "$OUT" "Please review PR #42 for the auth module."

e2e_case_banner "robot_navigate_md"
check_unsupported_markdown navigate_md navigate -- "resource://projects"

# ---------------------------------------------------------------------------
# Track 4: Monitoring & Analytics
# ---------------------------------------------------------------------------

e2e_case_banner "robot_reservations_md"
check_unsupported_markdown reservations_md reservations

e2e_case_banner "robot_metrics_md"
check_unsupported_markdown metrics_md metrics

e2e_case_banner "robot_health_md"
check_unsupported_markdown health_md health

e2e_case_banner "robot_analytics_md"
check_unsupported_markdown analytics_md analytics

# ---------------------------------------------------------------------------
# Track 5: Entity Views
# ---------------------------------------------------------------------------

e2e_case_banner "robot_agents_md"
check_unsupported_markdown agents_md agents

e2e_case_banner "robot_contacts_md"
check_unsupported_markdown contacts_md contacts

e2e_case_banner "robot_projects_md"
check_unsupported_markdown projects_md projects

e2e_case_banner "robot_attachments_md"
check_unsupported_markdown attachments_md attachments

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
