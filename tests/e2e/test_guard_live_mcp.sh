#!/usr/bin/env bash
# test_guard_live_mcp.sh — E2E: guard conflict detection against MCP-populated database
#
# Verifies (br-3h13.18.5):
# Unlike test_guard.sh which uses direct SQL inserts, this test populates
# the database exclusively through MCP tool calls (file_reservation_paths,
# release_file_reservations, force_release_file_reservation) and then
# verifies the pre-commit guard hook correctly detects/allows commits
# against that MCP-populated database.
#
# The guard pre-commit hook is a Python script that queries the SQLite DB
# directly (via AGENT_MAIL_DB env var). The `am guard check` CLI command
# reads JSON archive files instead, so we test via git commit.
#
# Cases:
# 1. Pre-commit blocks commit when MCP-created exclusive reservation conflicts
# 2. Pre-commit allows the reservation owner to commit
# 3. Pre-commit passes after release_file_reservations via MCP
# 4. Pre-commit blocks commit with MCP-created glob pattern reservation
# 5. Pre-commit passes after force_release_file_reservation via MCP
# 6. Pre-commit blocks rename when old path reserved via MCP

set -euo pipefail

E2E_SUITE="guard_live_mcp"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Guard Live MCP E2E Test Suite"

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

WORK="$(e2e_mktemp "e2e_guard_live")"
GUARD_DB="${WORK}/guard_live.sqlite3"
REPO="${WORK}/repo"
PROJECT_PATH="${REPO}"
STORAGE_ROOT="${WORK}/storage_root"
export STORAGE_ROOT
mkdir -p "${STORAGE_ROOT}"
e2e_log "Storage root: ${STORAGE_ROOT}"

PROJECT_SLUG="$(python3 - "${PROJECT_PATH}" <<'PY'
import sys

value = sys.argv[1].strip().lower()
out = []
prev_dash = False
for ch in value:
    if ("a" <= ch <= "z") or ("0" <= ch <= "9"):
        out.append(ch)
        prev_dash = False
    elif not prev_dash:
        out.append("-")
        prev_dash = True
slug = "".join(out).strip("-")
print(slug or "project")
PY
)"
ARCHIVE_PROJECT_ROOT="${STORAGE_ROOT}/projects/${PROJECT_SLUG}"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-guard-live","version":"1.0"}}}'

# Create a git repo with some files
mkdir -p "$REPO"
e2e_init_git_repo "$REPO"
mkdir -p "$REPO/app/api" "$REPO/lib" "$REPO/shared"
echo "# init" > "$REPO/app/api/views.py"
echo "# init" > "$REPO/app/api/models.py"
echo "# init" > "$REPO/lib/original_module.py"
echo "# init" > "$REPO/shared/utils.py"
echo "init" > "$REPO/README.md"
e2e_git_commit "$REPO" "initial commit"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

MCP_OUTPUT_FILE=""
MCP_STDERR_FILE=""
MCP_FIFO=""
MCP_PID=""
MCP_INITIALIZED=0
MCP_LAST_RESPONSE=""

cleanup_shared_mcp() {
    exec 3>&- 2>/dev/null || true
    if [ -n "${MCP_PID:-}" ]; then
        kill "$MCP_PID" 2>/dev/null || true
        wait "$MCP_PID" 2>/dev/null || true
        MCP_PID=""
    fi
    MCP_INITIALIZED=0
}

trap 'cleanup_shared_mcp; _e2e_cleanup' EXIT

jsonrpc_request_id() {
    python3 -c 'import json, sys; print(json.loads(sys.stdin.read()).get("id", ""))' 2>/dev/null
}

wait_for_jsonrpc_response() {
    local req_id="$1"
    local label="$2"

    for _ in $(seq 1 100); do
        if python3 - "$MCP_OUTPUT_FILE" "$req_id" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
req_id = sys.argv[2]
if not path.is_file():
    sys.exit(1)

for line in path.read_text(encoding="utf-8", errors="ignore").splitlines():
    try:
        data = json.loads(line)
    except json.JSONDecodeError:
        continue
    if str(data.get("id", "")) == req_id:
        sys.exit(0)
sys.exit(1)
PY
        then
            return 0
        fi
        sleep 0.1
    done

    e2e_fail "${label}: MCP response id ${req_id} timed out"
    e2e_save_artifact "${label//[^A-Za-z0-9_]/_}_mcp_stdout.txt" \
        "$(cat "$MCP_OUTPUT_FILE" 2>/dev/null || true)"
    e2e_save_artifact "${label//[^A-Za-z0-9_]/_}_mcp_stderr.txt" \
        "$(cat "$MCP_STDERR_FILE" 2>/dev/null || true)"
    return 1
}

start_shared_mcp_session() {
    local db_path="$1"

    if [ -n "${MCP_PID:-}" ] && kill -0 "$MCP_PID" 2>/dev/null; then
        return 0
    fi

    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.shared.XXXXXX")"
    MCP_OUTPUT_FILE="${srv_work}/stdout.txt"
    MCP_STDERR_FILE="${srv_work}/stderr.txt"
    MCP_FIFO="${srv_work}/stdin_fifo"
    mkfifo "$MCP_FIFO"

    DATABASE_URL="sqlite:////${db_path}" RUST_LOG="${RUST_LOG:-error}" \
        am serve-stdio < "$MCP_FIFO" > "$MCP_OUTPUT_FILE" 2>"$MCP_STDERR_FILE" &
    MCP_PID=$!
    exec 3>"$MCP_FIFO"
    sleep 0.3
}

send_jsonrpc_session() {
    local db_path="$1"; shift
    local requests=("$@")
    start_shared_mcp_session "$db_path"

    for req in "${requests[@]}"; do
        if [ "$req" = "$INIT_REQ" ]; then
            if [ "$MCP_INITIALIZED" = "0" ]; then
                echo "$req" >&3
                wait_for_jsonrpc_response "1" "initialize" || return 1
                MCP_INITIALIZED=1
            fi
            continue
        fi

        local req_id
        req_id="$(printf '%s' "$req" | jsonrpc_request_id)"
        echo "$req" >&3
        sleep 0.15
        wait_for_jsonrpc_response "$req_id" "request_${req_id}" || return 1
    done

    MCP_LAST_RESPONSE="$(cat "$MCP_OUTPUT_FILE" 2>/dev/null || true)"
}

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

wait_for_archive_project() {
    local label="$1"

    for _ in $(seq 1 50); do
        if [ -d "${ARCHIVE_PROJECT_ROOT}" ]; then
            e2e_log "${label}: archive project visible"
            return 0
        fi
        sleep 0.2
    done

    e2e_fail "${label}: archive project did not become visible"
    e2e_save_artifact "${label//[^A-Za-z0-9_]/_}_archive_tree.txt" \
        "$(find "${STORAGE_ROOT}" -maxdepth 4 -type d -print 2>&1 || true)"
}

wait_for_reservation_artifact() {
    local label="$1"
    local pattern="$2"
    local state="$3"

    for _ in $(seq 1 50); do
        if python3 - "${ARCHIVE_PROJECT_ROOT}" "${pattern}" "${state}" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
pattern = sys.argv[2]
state = sys.argv[3]
reservations_dir = root / "file_reservations"
if not reservations_dir.is_dir():
    sys.exit(1)

found_pattern = False
for path in reservations_dir.glob("*.json"):
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        continue
    if data.get("path_pattern") != pattern:
        continue
    found_pattern = True
    released = data.get("released_ts") not in (None, "", 0, "0", "null")
    if state == "active" and not released:
        sys.exit(0)
    if state == "released" and released:
        sys.exit(0)
    if state == "absent_or_released" and released:
        sys.exit(0)

if state == "absent_or_released" and not found_pattern:
    sys.exit(0)
sys.exit(1)
PY
        then
            e2e_log "${label}: reservation ${state} artifact visible for ${pattern}"
            return 0
        fi
        sleep 0.2
    done

    e2e_fail "${label}: reservation ${state} artifact not visible for ${pattern}"
    e2e_save_artifact "${label//[^A-Za-z0-9_]/_}_reservations.txt" \
        "$(find "${ARCHIVE_PROJECT_ROOT}/file_reservations" -maxdepth 1 -type f -print -exec cat {} \; 2>&1 || true)"
}

# ===========================================================================
# Setup: create project + agents via MCP
# ===========================================================================
e2e_case_banner "Setup: project + agents via MCP"

send_jsonrpc_session "$GUARD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"RedFox\",\"project_key\":\"${PROJECT_PATH}\",\"program\":\"test\",\"model\":\"test\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"BlueLake\",\"project_key\":\"${PROJECT_PATH}\",\"program\":\"test\",\"model\":\"test\"}}}"
SETUP_RESP="$MCP_LAST_RESPONSE"
e2e_save_artifact "setup.txt" "$SETUP_RESP"

SETUP_OK=true
for rid in 10 11 12; do
    if [ "$(is_error_result "$SETUP_RESP" $rid)" = "true" ]; then
        SETUP_OK=false
    fi
done

if [ "$SETUP_OK" = "true" ]; then
    e2e_pass "setup: project + 2 agents created"
else
    e2e_fail "setup: creation failed"
    e2e_summary
    exit 1
fi
wait_for_archive_project "setup"

# Install guard hook — set DATABASE_URL so the hook embeds the DB path
set +e
DATABASE_URL="sqlite:////${GUARD_DB}" am guard install "$PROJECT_PATH" "$REPO" 2>"${WORK}/install_stderr.txt"
install_rc=$?
set -e
e2e_assert_exit_code "guard install succeeds" "0" "$install_rc"

# Verify hook installed
HOOKS_DIR="${REPO}/.git/hooks"
e2e_assert_file_exists "hook plugin exists" "${HOOKS_DIR}/hooks.d/pre-commit/50-agent-mail.py"

# ===========================================================================
# Case 1: Pre-commit blocks commit when MCP-created exclusive reservation conflicts
# ===========================================================================
e2e_case_banner "precommit_blocks_mcp_reservation"

# Create exclusive reservation via MCP tool (BlueLake reserves app/api/views.py)
send_jsonrpc_session "$GUARD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"paths\":[\"app/api/views.py\"],\"exclusive\":true,\"ttl_seconds\":3600,\"reason\":\"Working on API views\"}}}"
RES1_RESP="$MCP_LAST_RESPONSE"
e2e_save_artifact "case_01_reserve.txt" "$RES1_RESP"

if [ "$(is_error_result "$RES1_RESP" 20)" = "true" ]; then
    e2e_fail "case1: reservation creation failed"
else
    e2e_pass "case1: exclusive reservation created via MCP"
fi
wait_for_reservation_artifact "case1" "app/api/views.py" "active"

# Stage a conflicting change and attempt commit (RedFox)
echo "# modified by RedFox" >> "$REPO/app/api/views.py"
git -C "$REPO" add app/api/views.py

set +e
AGENT_NAME=RedFox AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "conflicting change" \
    2>"${WORK}/case1_stderr.txt"
case1_rc=$?
set -e

e2e_assert_exit_code "case1: commit blocked by pre-commit hook" "1" "$case1_rc"
case1_stderr="$(cat "${WORK}/case1_stderr.txt" 2>/dev/null || true)"
e2e_assert_contains "case1: mentions reservation conflict" "$case1_stderr" "file reservation conflict"
e2e_assert_contains "case1: mentions BlueLake" "$case1_stderr" "BlueLake"
e2e_save_artifact "case_01_stderr.txt" "$case1_stderr"

# ===========================================================================
# Case 2: Pre-commit allows the reservation owner to commit
# ===========================================================================
e2e_case_banner "precommit_allows_owner"

# Same staged change (still staged from case 1 failure), commit as BlueLake (the owner)
set +e
AGENT_NAME=BlueLake AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "owner change" \
    2>"${WORK}/case2_stderr.txt"
case2_rc=$?
set -e

e2e_assert_exit_code "case2: owner commit succeeds" "0" "$case2_rc"
e2e_save_artifact "case_02_stderr.txt" "$(cat "${WORK}/case2_stderr.txt" 2>/dev/null || true)"

# ===========================================================================
# Case 3: Pre-commit passes after release_file_reservations via MCP
# ===========================================================================
e2e_case_banner "precommit_after_release"

# Release via MCP tool
send_jsonrpc_session "$GUARD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"paths\":[\"app/api/views.py\"]}}}"
REL_RESP="$MCP_LAST_RESPONSE"
e2e_save_artifact "case_03_release.txt" "$REL_RESP"

if [ "$(is_error_result "$REL_RESP" 30)" = "true" ]; then
    e2e_fail "case3: release failed"
else
    e2e_pass "case3: release_file_reservations succeeded"
fi
wait_for_reservation_artifact "case3" "app/api/views.py" "absent_or_released"

# Stage new change and commit as RedFox — should pass now
echo "# second modification by RedFox" >> "$REPO/app/api/views.py"
git -C "$REPO" add app/api/views.py

set +e
AGENT_NAME=RedFox AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "change after release" \
    2>"${WORK}/case3_stderr.txt"
case3_rc=$?
set -e

e2e_assert_exit_code "case3: commit passes after release" "0" "$case3_rc"
e2e_save_artifact "case_03_stderr.txt" "$(cat "${WORK}/case3_stderr.txt" 2>/dev/null || true)"

# ===========================================================================
# Case 4: Pre-commit blocks commit with MCP-created glob pattern reservation
# ===========================================================================
e2e_case_banner "precommit_glob_pattern"

# Create glob-pattern reservation via MCP (BlueLake reserves app/api/*.py)
send_jsonrpc_session "$GUARD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"paths\":[\"app/api/*.py\"],\"exclusive\":true,\"ttl_seconds\":3600,\"reason\":\"Working on all API files\"}}}"
RES4_RESP="$MCP_LAST_RESPONSE"
e2e_save_artifact "case_04_reserve.txt" "$RES4_RESP"

if [ "$(is_error_result "$RES4_RESP" 40)" = "true" ]; then
    e2e_fail "case4: glob reservation creation failed"
else
    e2e_pass "case4: glob pattern reservation created"
fi
wait_for_reservation_artifact "case4" "app/api/*.py" "active"

# Stage change to app/api/models.py (matches glob app/api/*.py) and commit as RedFox
echo "# modified by RedFox" >> "$REPO/app/api/models.py"
git -C "$REPO" add app/api/models.py

set +e
AGENT_NAME=RedFox AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "glob conflict" \
    2>"${WORK}/case4_stderr.txt"
case4_rc=$?
set -e

e2e_assert_exit_code "case4: glob pattern blocks commit" "1" "$case4_rc"
case4_stderr="$(cat "${WORK}/case4_stderr.txt" 2>/dev/null || true)"
e2e_assert_contains "case4: mentions BlueLake" "$case4_stderr" "BlueLake"
e2e_save_artifact "case_04_stderr.txt" "$case4_stderr"

# Non-matching path should pass (commit only shared/utils.py, unstage models.py first)
git -C "$REPO" reset HEAD app/api/models.py >/dev/null 2>&1
git -C "$REPO" checkout -- app/api/models.py >/dev/null 2>&1 || true
echo "# safe change" >> "$REPO/shared/utils.py"
git -C "$REPO" add shared/utils.py

set +e
AGENT_NAME=RedFox AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "safe non-matching change" \
    2>"${WORK}/case4b_stderr.txt"
case4b_rc=$?
set -e

e2e_assert_exit_code "case4: non-matching path passes" "0" "$case4b_rc"
e2e_save_artifact "case_04b_stderr.txt" "$(cat "${WORK}/case4b_stderr.txt" 2>/dev/null || true)"

# ===========================================================================
# Case 5: Pre-commit passes after force_release_file_reservation via MCP
# ===========================================================================
e2e_case_banner "precommit_after_force_release"

# Get the reservation ID from the glob reservation response
RES4_TEXT="$(extract_result "$RES4_RESP" 40)"
RES4_ID="$(echo "$RES4_TEXT" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
granted = d.get('granted', d.get('file_reservations', {}).get('granted', []))
if granted:
    print(granted[0].get('id', ''))
" 2>/dev/null)"

if [ -z "$RES4_ID" ]; then
    e2e_log "WARN: could not extract reservation ID, trying with id=2"
    RES4_ID="2"
fi
e2e_log "force-releasing reservation ID: $RES4_ID"

# Force-release via MCP tool (RedFox force-releases BlueLake's glob reservation)
# Restart the shared stdio server with short inactivity thresholds so the
# recently-created reservation is considered stale by the server-side config.
cleanup_shared_mcp
export FILE_RESERVATION_INACTIVITY_SECONDS=0
export FILE_RESERVATION_ACTIVITY_GRACE_SECONDS=0
send_jsonrpc_session "$GUARD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"force_release_file_reservation\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"RedFox\",\"file_reservation_id\":${RES4_ID},\"note\":\"Needed urgently\"}}}"
FREL_RESP="$MCP_LAST_RESPONSE"
e2e_save_artifact "case_05_force_release.txt" "$FREL_RESP"

if [ "$(is_error_result "$FREL_RESP" 50)" = "true" ]; then
    e2e_fail "case5: force_release failed"
else
    e2e_pass "case5: force_release_file_reservation succeeded"
fi
unset FILE_RESERVATION_INACTIVITY_SECONDS FILE_RESERVATION_ACTIVITY_GRACE_SECONDS
wait_for_reservation_artifact "case5" "app/api/*.py" "absent_or_released"

# Now commit the previously conflicting file — should pass
echo "# modified after force-release" >> "$REPO/app/api/models.py"
git -C "$REPO" add app/api/models.py

set +e
AGENT_NAME=RedFox AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "change after force-release" \
    2>"${WORK}/case5_stderr.txt"
case5_rc=$?
set -e

e2e_assert_exit_code "case5: commit passes after force-release" "0" "$case5_rc"
e2e_save_artifact "case_05_stderr.txt" "$(cat "${WORK}/case5_stderr.txt" 2>/dev/null || true)"

# ===========================================================================
# Case 6: Pre-commit blocks rename when old path reserved via MCP
# ===========================================================================
e2e_case_banner "precommit_rename_scenario"

# Create reservation on the original file path via MCP
send_jsonrpc_session "$GUARD_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"paths\":[\"lib/original_module.py\"],\"exclusive\":true,\"ttl_seconds\":3600,\"reason\":\"Refactoring module\"}}}"
RES6_RESP="$MCP_LAST_RESPONSE"
e2e_save_artifact "case_06_reserve.txt" "$RES6_RESP"

if [ "$(is_error_result "$RES6_RESP" 60)" = "true" ]; then
    e2e_fail "case6: rename reservation creation failed"
else
    e2e_pass "case6: reservation on original path created"
fi
wait_for_reservation_artifact "case6" "lib/original_module.py" "active"

# Stage a rename in git
git -C "$REPO" mv lib/original_module.py lib/renamed_module.py

set +e
AGENT_NAME=RedFox AGENT_MAIL_DB="$GUARD_DB" git -C "$REPO" commit -m "rename conflicting file" \
    2>"${WORK}/case6_stderr.txt"
case6_rc=$?
set -e

e2e_assert_exit_code "case6: rename commit blocked" "1" "$case6_rc"
case6_stderr="$(cat "${WORK}/case6_stderr.txt" 2>/dev/null || true)"
e2e_assert_contains "case6: mentions BlueLake" "$case6_stderr" "BlueLake"
e2e_save_artifact "case_06_stderr.txt" "$case6_stderr"

# Reset the rename for cleanup
git -C "$REPO" reset HEAD -- lib/original_module.py lib/renamed_module.py >/dev/null 2>&1
git -C "$REPO" checkout -- lib/original_module.py >/dev/null 2>&1 || true
[ -f "$REPO/lib/renamed_module.py" ] && mv "$REPO/lib/renamed_module.py" "$REPO/lib/original_module.py" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
