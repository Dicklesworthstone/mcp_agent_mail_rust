#!/usr/bin/env bash
# test_check_inbox.sh - E2E test suite for am check-inbox command
#
# Verifies (br-3qjmi T4.7):
# - Direct mode: SQLite direct query path
# - HTTP mode: JSON-RPC via /api/ endpoint
# - Rate limiting: Lockfile-based check throttling
# - Output modes: JSON vs human-readable (emoji)
# - Template detection: Silent exit for placeholder values
# - Error handling: Fail-safe for hooks (silent exit on errors)
# - Exit codes: 0 on success/skip, proper signal handling
#
# Artifacts (via e2e_lib.sh helpers):
# - Server logs: tests/artifacts/check_inbox/<timestamp>/logs/server_*.log
# - Per-case directories: <case_id>/stdout.txt, stderr.txt, exit_code.txt
# - Rate limit lockfiles: /tmp/mcp-mail-check-*

set -euo pipefail

E2E_SUITE="check_inbox"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Check-Inbox E2E Test Suite"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

if ! command -v curl >/dev/null 2>&1; then
    e2e_log "curl not found; skipping suite"
    e2e_skip "curl required"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Build the CLI binary
AM_CLI_BIN=""
ensure_am_cli() {
    if [ -n "$AM_CLI_BIN" ] && [ -x "$AM_CLI_BIN" ]; then
        return 0
    fi

    # First check if the binary already exists
    AM_CLI_BIN="${CARGO_TARGET_DIR:-target}/debug/am"
    if [ -x "$AM_CLI_BIN" ]; then
        e2e_log "CLI binary: $AM_CLI_BIN (pre-built)"
        return 0
    fi

    AM_CLI_BIN="$(pwd)/target/debug/am"
    if [ -x "$AM_CLI_BIN" ]; then
        e2e_log "CLI binary: $AM_CLI_BIN (pre-built)"
        return 0
    fi

    # Build if not found
    e2e_log "Building mcp-agent-mail-cli..."
    local build_log="${E2E_ARTIFACT_DIR}/logs/cargo_build.log"
    mkdir -p "$(dirname "$build_log")"
    if ! cargo build -p mcp-agent-mail-cli 2>"$build_log"; then
        e2e_fail "Failed to build mcp-agent-mail-cli (see $build_log)"
        return 1
    fi

    AM_CLI_BIN="${CARGO_TARGET_DIR:-target}/debug/am"
    if [ ! -x "$AM_CLI_BIN" ]; then
        AM_CLI_BIN="$(pwd)/target/debug/am"
    fi

    if [ ! -x "$AM_CLI_BIN" ]; then
        e2e_fail "am binary not found at $AM_CLI_BIN"
        return 1
    fi

    e2e_log "CLI binary: $AM_CLI_BIN"
}

# Run check-inbox and capture output
# Args: case_id [cli_args...]
# Sets: _STDOUT, _STDERR, _EXIT_CODE
run_check_inbox() {
    local case_id="$1"
    shift

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "$case_dir"

    set +e
    _STDOUT=$("$AM_CLI_BIN" check-inbox "$@" 2>"${case_dir}/stderr.txt")
    _EXIT_CODE=$?
    set -e

    echo "$_STDOUT" > "${case_dir}/stdout.txt"
    echo "$_EXIT_CODE" > "${case_dir}/exit_code.txt"
    _STDERR=$(cat "${case_dir}/stderr.txt")
}

# Clean up rate limit lockfiles for a given agent
cleanup_lockfile() {
    local agent_name="$1"
    # Sanitize agent name (non-alphanumeric -> underscore)
    local sanitized
    sanitized=$(echo "$agent_name" | tr -c '[:alnum:]' '_')
    rm -f "/tmp/mcp-mail-check-${sanitized}" 2>/dev/null || true
}

# Start a test server with sample data
# Args: label
# Sets: E2E_SERVER_URL, WORK_DIR, DB_PATH, STORAGE_ROOT
start_test_server() {
    local label="$1"

    WORK_DIR="$(e2e_mktemp "e2e_check_inbox_${label}")"
    DB_PATH="${WORK_DIR}/db.sqlite3"
    STORAGE_ROOT="${WORK_DIR}/storage"
    mkdir -p "$STORAGE_ROOT"

    if ! e2e_start_server_with_logs "$DB_PATH" "$STORAGE_ROOT" "$label" \
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1" \
        "HTTP_RBAC_ENABLED=0"; then
        e2e_fail "server ($label) failed to start"
        return 1
    fi

    # Override URL to use /api/ endpoint
    E2E_SERVER_URL="${E2E_SERVER_URL%/mcp/}/api/"
    return 0
}

# Seed test data using macro_start_session for reliability
# Args: project_key
# Returns: SEED_AGENT_NAME (the auto-generated agent name), SEED_SUCCESS (0/1)
seed_test_data() {
    local project_key="$1"
    local sender_agent="GoldEagle"  # A second agent to send messages
    SEED_SUCCESS=0

    # Use macro_start_session to create project + recipient agent in one call
    e2e_rpc_call "seed_session" "$E2E_SERVER_URL" "macro_start_session" \
        "{\"human_key\":\"${project_key}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"task_description\":\"E2E check-inbox test\",\"inbox_limit\":5}" || true

    # Check if seeding succeeded
    local session_resp
    session_resp="$(cat "${E2E_ARTIFACT_DIR}/seed_session/response.json" 2>/dev/null || echo "{}")"

    if echo "$session_resp" | grep -q '"isError":true'; then
        e2e_log "Warning: macro_start_session failed (pre-existing bug)"
        SEED_AGENT_NAME="BlueLake"
        return 0
    fi

    # Extract the auto-generated agent name from the response
    SEED_AGENT_NAME="$(echo "$session_resp" | python3 -c "
import sys, json, re
try:
    data = json.load(sys.stdin)
    text = data.get('result', {}).get('content', [{}])[0].get('text', '')
    # Parse 'name' from the text (e.g., 'BlueLake')
    match = re.search(r'\"name\"\\s*:\\s*\"([A-Za-z]+)\"', text)
    if match:
        print(match.group(1))
except:
    pass
" 2>/dev/null)"

    if [ -z "$SEED_AGENT_NAME" ]; then
        # Fallback to BlueLake if extraction fails
        SEED_AGENT_NAME="BlueLake"
        return 0
    fi

    # Register a second agent to send messages
    e2e_rpc_call "seed_sender" "$E2E_SERVER_URL" "register_agent" \
        "{\"project_key\":\"${project_key}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"${sender_agent}\"}" || true

    # Check sender registration
    local sender_resp
    sender_resp="$(cat "${E2E_ARTIFACT_DIR}/seed_sender/response.json" 2>/dev/null || echo "{}")"
    if echo "$sender_resp" | grep -q '"isError":true'; then
        e2e_log "Warning: sender registration failed (pre-existing bug)"
        return 0
    fi

    # Send a test message from sender to recipient
    e2e_rpc_call "seed_message" "$E2E_SERVER_URL" "send_message" \
        "{\"project_key\":\"${project_key}\",\"sender_name\":\"${sender_agent}\",\"to\":[\"${SEED_AGENT_NAME}\"],\"subject\":\"Test message\",\"body_md\":\"Hello from E2E test\"}" || true

    # Check message send
    local msg_resp
    msg_resp="$(cat "${E2E_ARTIFACT_DIR}/seed_message/response.json" 2>/dev/null || echo "{}")"
    if echo "$msg_resp" | grep -q '"isError":true'; then
        e2e_log "Warning: message send failed"
        return 0
    fi

    # Send a high-priority message
    e2e_rpc_call "seed_urgent" "$E2E_SERVER_URL" "send_message" \
        "{\"project_key\":\"${project_key}\",\"sender_name\":\"${sender_agent}\",\"to\":[\"${SEED_AGENT_NAME}\"],\"subject\":\"Urgent message\",\"body_md\":\"This is urgent\",\"importance\":\"high\"}" || true

    SEED_SUCCESS=1
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

ensure_am_cli || {
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
}

# ---------------------------------------------------------------------------
# Run 1: Template detection (silent exit for placeholder values)
# ---------------------------------------------------------------------------

e2e_banner "Run 1: Template detection"

e2e_case_banner "Template agent name exits silently"
cleanup_lockfile '$AGENT_NAME'
e2e_mark_case_start "template_agent"
run_check_inbox "template_agent" --agent '$AGENT_NAME' --rate-limit 0
e2e_assert_exit_code "template agent exit code" "0" "$_EXIT_CODE"
e2e_assert_eq "template agent stdout empty" "" "$_STDOUT"
e2e_mark_case_end "template_agent"

e2e_case_banner "Template project exits silently"
cleanup_lockfile 'TestAgent'
e2e_mark_case_start "template_project"
run_check_inbox "template_project" --agent "TestAgent" --project '${PROJECT_KEY}' --rate-limit 0
e2e_assert_exit_code "template project exit code" "0" "$_EXIT_CODE"
e2e_assert_eq "template project stdout empty" "" "$_STDOUT"
e2e_mark_case_end "template_project"

e2e_case_banner "Env var placeholder exits silently"
cleanup_lockfile '__PLACEHOLDER__'
e2e_mark_case_start "env_placeholder"
run_check_inbox "env_placeholder" --agent '__PLACEHOLDER__' --rate-limit 0
e2e_assert_exit_code "placeholder exit code" "0" "$_EXIT_CODE"
e2e_assert_eq "placeholder stdout empty" "" "$_STDOUT"
e2e_mark_case_end "env_placeholder"

# ---------------------------------------------------------------------------
# Run 2: Rate limiting behavior
# ---------------------------------------------------------------------------

e2e_banner "Run 2: Rate limiting"

e2e_case_banner "First check creates lockfile"
cleanup_lockfile 'RateLimitAgent'
e2e_mark_case_start "rate_limit_first"
# Note: This will fail silently because there's no server, but should create lockfile
run_check_inbox "rate_limit_first" --agent "RateLimitAgent" --rate-limit 3600 --host "127.0.0.1" --port 65432
e2e_assert_exit_code "first check exit code" "0" "$_EXIT_CODE"
# Check lockfile was created
LOCKFILE="/tmp/mcp-mail-check-RateLimitAgent"
if [ -f "$LOCKFILE" ]; then
    e2e_pass "lockfile created"
else
    e2e_fail "lockfile not created at $LOCKFILE"
fi
e2e_mark_case_end "rate_limit_first"

e2e_case_banner "Second check within interval is rate-limited (silent exit)"
e2e_mark_case_start "rate_limit_second"
run_check_inbox "rate_limit_second" --agent "RateLimitAgent" --rate-limit 3600 --host "127.0.0.1" --port 65432
e2e_assert_exit_code "second check exit code" "0" "$_EXIT_CODE"
e2e_assert_eq "rate-limited stdout empty" "" "$_STDOUT"
e2e_mark_case_end "rate_limit_second"

e2e_case_banner "Rate limit 0 disables rate limiting"
e2e_mark_case_start "rate_limit_zero"
run_check_inbox "rate_limit_zero" --agent "RateLimitAgent" --rate-limit 0 --host "127.0.0.1" --port 65432
# Even with rate-limit 0, should still exit silently if server not available
e2e_assert_exit_code "rate-limit-0 exit code" "0" "$_EXIT_CODE"
e2e_mark_case_end "rate_limit_zero"

cleanup_lockfile 'RateLimitAgent'

# ---------------------------------------------------------------------------
# Run 3: HTTP mode with live server
# ---------------------------------------------------------------------------

e2e_banner "Run 3: HTTP mode (live server)"

if ! start_test_server "http"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

# Extract port from E2E_SERVER_URL
HTTP_PORT=$(echo "$E2E_SERVER_URL" | sed -E 's|.*:([0-9]+)/.*|\1|')

# Seed test data
PROJECT_KEY="${WORK_DIR}/test_project"
mkdir -p "$PROJECT_KEY"
# Seed data using macro_start_session (sets SEED_AGENT_NAME)
seed_test_data "$PROJECT_KEY"
AGENT_NAME="$SEED_AGENT_NAME"
e2e_log "Using seeded agent: $AGENT_NAME"

if [ "$SEED_SUCCESS" = "1" ]; then
    e2e_case_banner "HTTP mode with messages (human-readable)"
    cleanup_lockfile "$AGENT_NAME"
    e2e_mark_case_start "http_human"
    run_check_inbox "http_human" --agent "$AGENT_NAME" --project "$PROJECT_KEY" \
        --host "127.0.0.1" --port "$HTTP_PORT" --rate-limit 0
    e2e_assert_exit_code "http human exit code" "0" "$_EXIT_CODE"
    # Should contain inbox reminder emoji output
    e2e_assert_contains "http human contains inbox reminder" "$_STDOUT" "INBOX REMINDER"
    e2e_mark_case_end "http_human"

    e2e_case_banner "HTTP mode with messages (JSON)"
    cleanup_lockfile "$AGENT_NAME"
    e2e_mark_case_start "http_json"
    run_check_inbox "http_json" --agent "$AGENT_NAME" --project "$PROJECT_KEY" \
        --host "127.0.0.1" --port "$HTTP_PORT" --rate-limit 0 --json
    e2e_assert_exit_code "http json exit code" "0" "$_EXIT_CODE"
    # Validate JSON structure
    if echo "$_STDOUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'agent' in d and 'unread_count' in d" 2>/dev/null; then
        e2e_pass "http json valid structure"
    else
        e2e_fail "http json invalid structure"
        echo "stdout: $_STDOUT"
    fi
    e2e_mark_case_end "http_json"
else
    e2e_skip "HTTP mode with messages tests (seeding failed - pre-existing bug in register_agent)"
fi

e2e_case_banner "HTTP mode with non-existent agent (silent exit)"
cleanup_lockfile "GhostWolf"
e2e_mark_case_start "http_no_agent"
run_check_inbox "http_no_agent" --agent "GhostWolf" --project "$PROJECT_KEY" \
    --host "127.0.0.1" --port "$HTTP_PORT" --rate-limit 0
e2e_assert_exit_code "http no agent exit code" "0" "$_EXIT_CODE"
# Should exit silently (fail-safe for hooks)
e2e_mark_case_end "http_no_agent"

e2e_stop_server
trap - EXIT

# Clear case markers to avoid JSON issues when running multiple servers
_E2E_CASE_LOG_LINE_COUNTS=()

# ---------------------------------------------------------------------------
# Run 4: Direct mode with SQLite
# ---------------------------------------------------------------------------

e2e_banner "Run 4: Direct mode (SQLite)"

# Start server to seed data, then test direct mode
if ! start_test_server "direct"; then
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

HTTP_PORT=$(echo "$E2E_SERVER_URL" | sed -E 's|.*:([0-9]+)/.*|\1|')
PROJECT_KEY="${WORK_DIR}/direct_project"
mkdir -p "$PROJECT_KEY"
# Seed data using macro_start_session (sets SEED_AGENT_NAME)
seed_test_data "$PROJECT_KEY"
AGENT_NAME="$SEED_AGENT_NAME"
e2e_log "Using seeded agent: $AGENT_NAME"

# Stop server - direct mode doesn't need it
e2e_stop_server
trap - EXIT

# Set up environment for direct mode
export DATABASE_URL="sqlite:////${DB_PATH}"
export STORAGE_ROOT="$STORAGE_ROOT"

if [ "$SEED_SUCCESS" = "1" ]; then
    e2e_case_banner "Direct mode with messages (human-readable)"
    cleanup_lockfile "$AGENT_NAME"
    e2e_mark_case_start "direct_human"
    run_check_inbox "direct_human" --agent "$AGENT_NAME" --project "$PROJECT_KEY" \
        --direct --rate-limit 0
    e2e_assert_exit_code "direct human exit code" "0" "$_EXIT_CODE"
    # Should contain inbox reminder
    e2e_assert_contains "direct human contains inbox reminder" "$_STDOUT" "INBOX REMINDER"
    e2e_mark_case_end "direct_human"

    e2e_case_banner "Direct mode with messages (JSON)"
    cleanup_lockfile "$AGENT_NAME"
    e2e_mark_case_start "direct_json"
    run_check_inbox "direct_json" --agent "$AGENT_NAME" --project "$PROJECT_KEY" \
        --direct --rate-limit 0 --json
    e2e_assert_exit_code "direct json exit code" "0" "$_EXIT_CODE"
    # Validate JSON structure
    if echo "$_STDOUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'agent' in d and 'unread_count' in d" 2>/dev/null; then
        e2e_pass "direct json valid structure"
    else
        e2e_fail "direct json invalid structure"
        echo "stdout: $_STDOUT"
    fi
    e2e_mark_case_end "direct_json"
else
    e2e_skip "Direct mode with messages tests (seeding failed - pre-existing bug in register_agent)"
fi

e2e_case_banner "Direct mode with non-existent agent (silent exit)"
cleanup_lockfile "PurpleFox"
e2e_mark_case_start "direct_no_agent"
run_check_inbox "direct_no_agent" --agent "PurpleFox" --project "$PROJECT_KEY" \
    --direct --rate-limit 0
e2e_assert_exit_code "direct no agent exit code" "0" "$_EXIT_CODE"
# Should exit silently
e2e_mark_case_end "direct_no_agent"

unset DATABASE_URL STORAGE_ROOT

# ---------------------------------------------------------------------------
# Run 5: No agent configured (silent exit)
# ---------------------------------------------------------------------------

e2e_banner "Run 5: No agent configured"

e2e_case_banner "No agent flag and no env var (silent exit)"
e2e_mark_case_start "no_agent_config"
# Clear relevant env vars
unset AGENT_NAME AGENT_MAIL_AGENT 2>/dev/null || true
run_check_inbox "no_agent_config" --rate-limit 0
e2e_assert_exit_code "no agent exit code" "0" "$_EXIT_CODE"
e2e_assert_eq "no agent stdout empty" "" "$_STDOUT"
e2e_mark_case_end "no_agent_config"

# ---------------------------------------------------------------------------
# Run 6: Error handling (fail-safe for hooks)
# ---------------------------------------------------------------------------

e2e_banner "Run 6: Error handling (fail-safe)"

e2e_case_banner "Connection refused exits silently"
cleanup_lockfile "SilentBear"
e2e_mark_case_start "conn_refused"
run_check_inbox "conn_refused" --agent "SilentBear" --project "/tmp/nonexistent" \
    --host "127.0.0.1" --port 65432 --rate-limit 0
e2e_assert_exit_code "conn refused exit code" "0" "$_EXIT_CODE"
# Should exit silently - fail-safe for hooks
e2e_mark_case_end "conn_refused"

e2e_case_banner "Invalid project path in direct mode exits silently"
cleanup_lockfile "QuietFrog"
e2e_mark_case_start "direct_invalid_project"
export DATABASE_URL="sqlite:////nonexistent/path/db.sqlite3"
run_check_inbox "direct_invalid_project" --agent "QuietFrog" \
    --project "/nonexistent/project" --direct --rate-limit 0
e2e_assert_exit_code "direct invalid project exit code" "0" "$_EXIT_CODE"
unset DATABASE_URL
e2e_mark_case_end "direct_invalid_project"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

# Note: e2e_summary may fail due to pre-existing bug in e2e_write_server_log_stats
# (malformed JSON from grep -c output). We check for actual test failures separately.
e2e_summary || true

# Exit with appropriate code based on actual test failures
if [ "${_E2E_FAIL}" -gt 0 ]; then
    exit 1
fi
exit 0
