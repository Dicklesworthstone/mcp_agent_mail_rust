#!/usr/bin/env bash
# test_notifications.sh - E2E test suite for notification signal files (br-2ei.9.8)
#
# Tests:
# 1. send_message emits signals for TO and CC recipients
# 2. BCC recipients do NOT get signal files
# 3. fetch_inbox clears signal for that agent
# 4. Debounce: repeated sends within window do not create duplicate signals
# 5. list_pending_signals returns correct entries
# 6. Signal includes metadata when enabled
#
# All tests use a temp MCP server with file-based notification signals.

set -euo pipefail

AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP:-1}"

E2E_SUITE="notifications"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Notifications Signal Files E2E Test Suite"

if ! command -v curl >/dev/null 2>&1; then
    e2e_log "curl not found; skipping suite"
    e2e_skip "curl required"
    e2e_summary
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required"
    e2e_summary
    exit 0
fi

# ---------------------------------------------------------------------------
# Setup: temp workspace, DB, archive, signals directory
# ---------------------------------------------------------------------------
WORK="$(e2e_mktemp "e2e_notify")"
DB_PATH="${WORK}/storage.sqlite3"
STORAGE_ROOT="${WORK}/archive"
SIGNALS_DIR="${WORK}/signals"
TOKEN="test-token-notifications"

mkdir -p "$STORAGE_ROOT" "$SIGNALS_DIR"

# ---------------------------------------------------------------------------
# Start server with notifications enabled
# ---------------------------------------------------------------------------
e2e_log "Starting MCP server: notifications=on, debounce=500ms"

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "notifications" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "HTTP_JWT_ENABLED=0" \
    "NOTIFICATIONS_ENABLED=1" \
    "NOTIFICATIONS_SIGNALS_DIR=${SIGNALS_DIR}" \
    "NOTIFICATIONS_INCLUDE_METADATA=1" \
    "NOTIFICATIONS_DEBOUNCE_MS=500"; then
    e2e_fail "server failed to start"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

URL="${E2E_SERVER_URL}"

# Helper: call MCP tool via HTTP
mcp_call() {
    local tool="$1"
    local args="$2"
    MCP_CALL_SEQ="${MCP_CALL_SEQ:-0}"
    MCP_CALL_SEQ=$((MCP_CALL_SEQ + 1))
    local case_id
    case_id="$(printf "mcp_call_%03d_%s" "${MCP_CALL_SEQ}" "${tool}")"

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call "${case_id}" "${URL}" "${tool}" "${args}" "Authorization: Bearer ${TOKEN}"; then
        :
    fi

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
    if [ -z "${status}" ] || [ "${status}" = "000" ]; then
        return 1
    fi

    e2e_rpc_read_response "${case_id}"
}

# Helper: check if signal file exists for agent
signal_exists() {
    local project="$1"
    local agent="$2"
    [ -f "${SIGNALS_DIR}/projects/${project}/agents/${agent}.signal" ]
}

# Helper: count signal files for a project
signal_count() {
    local project="$1"
    local dir="${SIGNALS_DIR}/projects/${project}/agents"
    if [ -d "$dir" ]; then
        find "$dir" -name "*.signal" | wc -l | tr -d ' '
    else
        echo "0"
    fi
}

# ---------------------------------------------------------------------------
# Setup: create project and register agents
# ---------------------------------------------------------------------------
e2e_case_banner "Setup: create project and register agents"

ensure_result="$(mcp_call ensure_project '{"human_key": "/tmp/e2e_notify_project"}')"
e2e_save_artifact "setup_ensure_project.json" "$ensure_result"

# Extract project slug
project_slug="$(python3 -c "
import json,sys
data = json.loads(sys.argv[1])
for c in data.get('result',{}).get('content',[]):
    if c.get('type')=='text':
        r = json.loads(c['text'])
        # Try nested project.slug first, then flat slug
        slug = r.get('project',{}).get('slug','') if isinstance(r.get('project'), dict) else r.get('slug','')
        if slug:
            print(slug)
            break
" "$ensure_result" 2>/dev/null)"

if [ -z "$project_slug" ]; then
    e2e_fail "could not extract project slug"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 1
fi
e2e_log "Project slug: ${project_slug}"

# Register agents with valid adjective+noun names
SENDER="RedFox"
RECIPIENT_TO="BlueBear"
RECIPIENT_CC="GreenOwl"
RECIPIENT_BCC="PurpleCat"

for agent_name in "$SENDER" "$RECIPIENT_TO" "$RECIPIENT_CC" "$RECIPIENT_BCC"; do
    reg_result="$(mcp_call register_agent "{\"project_key\": \"/tmp/e2e_notify_project\", \"name\": \"${agent_name}\", \"program\": \"e2e\", \"model\": \"test\", \"task_description\": \"e2e test agent\"}")"
    e2e_save_artifact "setup_register_${agent_name}.json" "$reg_result"
done

for agent_name in "$SENDER" "$RECIPIENT_TO" "$RECIPIENT_CC" "$RECIPIENT_BCC"; do
    policy_result="$(mcp_call set_contact_policy "{\"project_key\": \"/tmp/e2e_notify_project\", \"agent_name\": \"${agent_name}\", \"policy\": \"open\"}")"
    e2e_save_artifact "setup_policy_${agent_name}.json" "$policy_result"
    e2e_assert_not_contains "set_contact_policy ${agent_name} has no error" "$policy_result" "\"error\""
done

e2e_pass "project and agents created"

# ---------------------------------------------------------------------------
# Case 1: send_message creates signals for TO and CC, not BCC
# ---------------------------------------------------------------------------
e2e_case_banner "send_message: signals for TO and CC, not BCC"

send_result="$(mcp_call send_message "{
    \"project_key\": \"/tmp/e2e_notify_project\",
    \"sender_name\": \"RedFox\",
    \"to\": [\"BlueBear\"],
    \"cc\": [\"GreenOwl\"],
    \"bcc\": [\"PurpleCat\"],
    \"subject\": \"Test notification\",
    \"body_md\": \"Hello from notification test\"
}")"

e2e_save_artifact "case1_send_result.json" "$send_result"

# Allow brief propagation
sleep 0.2

# TO recipient should have signal
if signal_exists "$project_slug" "$RECIPIENT_TO"; then
    e2e_pass "TO recipient (BlueBear) has signal"
else
    e2e_fail "TO recipient (BlueBear) missing signal"
fi

# CC recipient should have signal
if signal_exists "$project_slug" "$RECIPIENT_CC"; then
    e2e_pass "CC recipient (GreenOwl) has signal"
else
    e2e_fail "CC recipient (GreenOwl) missing signal"
fi

# BCC recipient should NOT have signal
if signal_exists "$project_slug" "$RECIPIENT_BCC"; then
    e2e_fail "BCC recipient (PurpleCat) should NOT have signal"
else
    e2e_pass "BCC recipient (PurpleCat) has no signal"
fi

# Sender should NOT have signal
if signal_exists "$project_slug" "$SENDER"; then
    e2e_fail "Sender should NOT have signal"
else
    e2e_pass "Sender has no signal"
fi

# Verify signal count: exactly 2 (RecipientA + RecipientB)
count="$(signal_count "$project_slug")"
e2e_assert_eq "signal count is 2" "2" "$count"

e2e_save_artifact "case1_signals_tree.txt" "$(e2e_tree "${SIGNALS_DIR}" 2>/dev/null || echo "(empty)")"

# ---------------------------------------------------------------------------
# Case 2: Signal contains metadata
# ---------------------------------------------------------------------------
e2e_case_banner "Signal file contains message metadata"

signal_file="${SIGNALS_DIR}/projects/${project_slug}/agents/${RECIPIENT_TO}.signal"
if [ -f "$signal_file" ]; then
    signal_content="$(cat "$signal_file")"
    e2e_assert_contains "signal has timestamp" "$signal_content" "timestamp"
    e2e_assert_contains "signal has project" "$signal_content" "$project_slug"
    e2e_assert_contains "signal has agent" "$signal_content" "$RECIPIENT_TO"
    e2e_assert_contains "signal has message block" "$signal_content" "message"
    e2e_assert_contains "signal has subject" "$signal_content" "Test notification"
    e2e_assert_contains "signal has sender" "$signal_content" "RedFox"
    e2e_save_artifact "case2_signal_content.json" "$signal_content"
else
    e2e_fail "signal file not found for TO recipient"
fi

# ---------------------------------------------------------------------------
# Case 3: fetch_inbox clears signal for that agent
# ---------------------------------------------------------------------------
e2e_case_banner "fetch_inbox clears signal for that agent"

# Verify signal exists before fetch
if signal_exists "$project_slug" "$RECIPIENT_TO"; then
    e2e_pass "TO signal exists before fetch"
else
    e2e_fail "TO signal missing before fetch"
fi

# Fetch inbox for RecipientA
fetch_result="$(mcp_call fetch_inbox "{\"project_key\": \"/tmp/e2e_notify_project\", \"agent_name\": \"BlueBear\"}")"
e2e_save_artifact "case3_fetch_result.json" "$fetch_result"

# Allow brief propagation
sleep 0.2

# Signal should be cleared for RecipientA
if signal_exists "$project_slug" "$RECIPIENT_TO"; then
    e2e_fail "TO signal should be cleared after fetch_inbox"
else
    e2e_pass "TO signal cleared after fetch_inbox"
fi

# CC signal should still be there (not fetched)
if signal_exists "$project_slug" "$RECIPIENT_CC"; then
    e2e_pass "CC signal still present (not fetched)"
else
    e2e_fail "CC signal should still be present"
fi

# ---------------------------------------------------------------------------
# Case 4: Debounce prevents duplicate signals within window
# ---------------------------------------------------------------------------
e2e_case_banner "Debounce: repeated sends within window"

# Clear remaining signals by fetching
mcp_call fetch_inbox "{\"project_key\": \"/tmp/e2e_notify_project\", \"agent_name\": \"GreenOwl\"}" >/dev/null 2>&1
sleep 0.1

# Send two messages rapidly (within 500ms debounce window)
mcp_call send_message "{
    \"project_key\": \"/tmp/e2e_notify_project\",
    \"sender_name\": \"RedFox\",
    \"to\": [\"BlueBear\"],
    \"subject\": \"Debounce test 1\",
    \"body_md\": \"First message\"
}" >/dev/null 2>&1

# Immediately send second message (should be debounced)
mcp_call send_message "{
    \"project_key\": \"/tmp/e2e_notify_project\",
    \"sender_name\": \"RedFox\",
    \"to\": [\"BlueBear\"],
    \"subject\": \"Debounce test 2\",
    \"body_md\": \"Second message (should be debounced)\"
}" >/dev/null 2>&1

sleep 0.1

# Should still have exactly 1 signal (debounced)
count="$(signal_count "$project_slug")"
e2e_assert_eq "debounce: signal count is 1" "1" "$count"

# Signal should exist for RecipientA (from first send)
if signal_exists "$project_slug" "$RECIPIENT_TO"; then
    # Verify signal content has the first message subject
    signal_content="$(cat "${SIGNALS_DIR}/projects/${project_slug}/agents/${RECIPIENT_TO}.signal")"
    e2e_assert_contains "signal from first (non-debounced) send" "$signal_content" "Debounce test 1"
    e2e_save_artifact "case4_debounce_signal.json" "$signal_content"
else
    e2e_fail "TO signal missing after debounce test"
fi

# Wait for debounce window to expire, then send again
sleep 0.6

mcp_call send_message "{
    \"project_key\": \"/tmp/e2e_notify_project\",
    \"sender_name\": \"RedFox\",
    \"to\": [\"BlueBear\"],
    \"subject\": \"After debounce window\",
    \"body_md\": \"Third message (after debounce expires)\"
}" >/dev/null 2>&1

sleep 0.1

# Signal should be overwritten with new content
if signal_exists "$project_slug" "$RECIPIENT_TO"; then
    signal_content="$(cat "${SIGNALS_DIR}/projects/${project_slug}/agents/${RECIPIENT_TO}.signal")"
    e2e_assert_contains "signal updated after debounce window" "$signal_content" "After debounce window"
else
    e2e_fail "TO signal missing after debounce window expired"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "final_signals_tree.txt" "$(e2e_tree "${SIGNALS_DIR}" 2>/dev/null || echo "(empty)")"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
