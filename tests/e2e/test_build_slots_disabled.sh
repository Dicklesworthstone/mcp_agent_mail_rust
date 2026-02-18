#!/usr/bin/env bash
# test_build_slots_disabled.sh — E2E: build slot tools when WORKTREES_ENABLED=false
#
# Verifies (br-3h13.18.4):
# - acquire/renew/release_build_slot return errors when worktrees disabled
# - Both WORKTREES_ENABLED=false and unset-entirely are treated as disabled
# - No silent success or phantom slot creation

set -euo pipefail

E2E_SUITE="build_slots_disabled"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Build Slots Disabled E2E Test Suite"

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

WORK="$(e2e_mktemp "e2e_slots_disabled")"
SLOT_DB="${WORK}/slots_disabled_test.sqlite3"
PROJECT_PATH="/tmp/e2e_slots_disabled_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-slots-disabled","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"; shift
    local env_vars="$1"; shift
    local requests=("$@")
    local output_file="${WORK}/session_response_$$.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    env $env_vars DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.3
        done
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

    [ -f "$output_file" ] && cat "$output_file"
}

# Extract JSON-RPC result content text by request id
extract_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $req_id and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                print(content[0].get('text', ''))
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
" 2>/dev/null
}

# Check if result is an MCP tool error or JSON-RPC error
is_error_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $req_id:
            if 'error' in d:
                print('true')
                sys.exit(0)
            if 'result' in d and d['result'].get('isError', False):
                print('true')
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('false')
" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Case 1: Setup with WORKTREES_ENABLED=true — seed project + agent
# ---------------------------------------------------------------------------
e2e_case_banner "Setup: project + agent with worktrees enabled"

SETUP_RESP="$(send_jsonrpc_session "$SLOT_DB" "WORKTREES_ENABLED=true" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"GoldFox\",\"task_description\":\"disabled slot testing\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

PROJ_ERR="$(is_error_result "$SETUP_RESP" 10)"
AGENT_ERR="$(is_error_result "$SETUP_RESP" 11)"

if [ "$PROJ_ERR" = "false" ] && [ "$AGENT_ERR" = "false" ]; then
    e2e_pass "project + agent seeded"
else
    e2e_fail "seed failed (project=$PROJ_ERR, agent=$AGENT_ERR)"
    e2e_summary
    exit 1
fi

# ---------------------------------------------------------------------------
# Case 2: acquire_build_slot with WORKTREES_ENABLED=false
# ---------------------------------------------------------------------------
e2e_case_banner "acquire_slot_when_disabled_false"

RESP="$(send_jsonrpc_session "$SLOT_DB" "WORKTREES_ENABLED=false" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"acquire_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-1\",\"ttl_seconds\":3600}}}" \
)"
e2e_save_artifact "case_02_acquire_disabled.txt" "$RESP"

ACQ_ERR="$(is_error_result "$RESP" 20)"
ACQ_TEXT="$(extract_result "$RESP" 20)"
e2e_save_artifact "case_02_acquire_result.txt" "$ACQ_TEXT"

if [ "$ACQ_ERR" = "true" ]; then
    e2e_pass "acquire_build_slot: returns error when disabled"
else
    e2e_fail "acquire_build_slot: should error when disabled, got success"
fi

# ---------------------------------------------------------------------------
# Case 3: renew_build_slot with WORKTREES_ENABLED=false
# ---------------------------------------------------------------------------
e2e_case_banner "renew_slot_when_disabled_false"

RESP="$(send_jsonrpc_session "$SLOT_DB" "WORKTREES_ENABLED=false" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"renew_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-1\",\"ttl_seconds\":3600}}}" \
)"
e2e_save_artifact "case_03_renew_disabled.txt" "$RESP"

RENEW_ERR="$(is_error_result "$RESP" 30)"
if [ "$RENEW_ERR" = "true" ]; then
    e2e_pass "renew_build_slot: returns error when disabled"
else
    e2e_fail "renew_build_slot: should error when disabled, got success"
fi

# ---------------------------------------------------------------------------
# Case 4: release_build_slot with WORKTREES_ENABLED=false
# ---------------------------------------------------------------------------
e2e_case_banner "release_slot_when_disabled_false"

RESP="$(send_jsonrpc_session "$SLOT_DB" "WORKTREES_ENABLED=false" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"release_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-1\"}}}" \
)"
e2e_save_artifact "case_04_release_disabled.txt" "$RESP"

REL_ERR="$(is_error_result "$RESP" 40)"
if [ "$REL_ERR" = "true" ]; then
    e2e_pass "release_build_slot: returns error when disabled"
else
    e2e_fail "release_build_slot: should error when disabled, got success"
fi

# ---------------------------------------------------------------------------
# Case 5: WORKTREES_ENABLED unset (absent entirely)
# ---------------------------------------------------------------------------
e2e_case_banner "acquire_slot_when_env_unset"

# Use env -u to unset the variable entirely
RESP="$(
    unset WORKTREES_ENABLED
    send_jsonrpc_session "$SLOT_DB" "WORKTREES_ENABLED_UNRELATED=1" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"acquire_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-2\",\"ttl_seconds\":3600}}}" \
)"
e2e_save_artifact "case_05_env_unset.txt" "$RESP"

UNSET_ERR="$(is_error_result "$RESP" 50)"
if [ "$UNSET_ERR" = "true" ]; then
    e2e_pass "acquire_build_slot: returns error when env unset"
else
    e2e_fail "acquire_build_slot: should error when env unset, got success"
fi

# ---------------------------------------------------------------------------
# Case 6: Verify acquire succeeds when WORKTREES_ENABLED=true (control case)
# ---------------------------------------------------------------------------
e2e_case_banner "acquire_slot_enabled_control"

RESP="$(send_jsonrpc_session "$SLOT_DB" "WORKTREES_ENABLED=true" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"acquire_build_slot\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GoldFox\",\"slot\":\"build-ctrl\",\"ttl_seconds\":3600}}}" \
)"
e2e_save_artifact "case_06_control.txt" "$RESP"

CTRL_ERR="$(is_error_result "$RESP" 60)"
CTRL_TEXT="$(extract_result "$RESP" 60)"
e2e_save_artifact "case_06_control_result.txt" "$CTRL_TEXT"

if [ "$CTRL_ERR" = "false" ]; then
    e2e_pass "acquire_build_slot: succeeds when enabled (control)"
else
    e2e_fail "acquire_build_slot: control case should succeed"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
