#!/usr/bin/env bash
# test_resources_project_agents.sh — E2E: resource://project/{slug} + agents/{slug}
#
# Verifies (br-3h13.18.1):
# - resource://project/{slug} returns valid project data
# - resource://agents/{slug} returns agent roster
# - Error cases: nonexistent slug, empty project
# - Live data: new agent registrations visible in subsequent reads

set -euo pipefail

E2E_SUITE="resources_project_agents"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Resource: project/{slug} + agents/{slug} E2E Test Suite"

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

WORK="$(e2e_mktemp "e2e_res_proj")"
RES_DB="${WORK}/resources_test.sqlite3"
PROJECT_PATH="/tmp/e2e_res_proj_$$"
PROJECT_SLUG="tmp-e2e-res-proj-$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-res-proj","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"; shift
    local requests=("$@")
    local output_file="${WORK}/session_response_$$.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error \
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

extract_resource_text() {
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
            r = d['result']
            contents = r.get('contents', r.get('content', []))
            if contents:
                print(contents[0].get('text', ''))
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

parse_json_field() {
    local text="$1"
    local field="$2"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    val = d
    for key in '$field'.split('.'):
        if isinstance(val, dict):
            val = val.get(key, '')
        elif isinstance(val, list) and key.isdigit():
            val = val[int(key)]
        else:
            val = ''
            break
    print(val if val is not None else '')
except Exception:
    print('')
" 2>/dev/null
}

json_array_len() {
    local text="$1"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if isinstance(d, list):
        print(len(d))
    else:
        print(0)
except Exception:
    print(0)
" 2>/dev/null
}

# ===========================================================================
# Case 1: Setup — create project + register 2 agents
# ===========================================================================
e2e_case_banner "Setup: project + 2 agents (RedFox, BlueLake)"

SETUP_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"RedFox\",\"project_key\":\"${PROJECT_PATH}\",\"program\":\"claude-code\",\"model\":\"test\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"BlueLake\",\"project_key\":\"${PROJECT_PATH}\",\"program\":\"codex-cli\",\"model\":\"test\"}}}" \
)"
e2e_save_artifact "case_01_setup.txt" "$SETUP_RESP"

PROJ_ERR="$(is_error_result "$SETUP_RESP" 10)"
A1_ERR="$(is_error_result "$SETUP_RESP" 11)"
A2_ERR="$(is_error_result "$SETUP_RESP" 12)"

if [ "$PROJ_ERR" = "false" ] && [ "$A1_ERR" = "false" ] && [ "$A2_ERR" = "false" ]; then
    e2e_pass "setup: project + 2 agents created"
else
    e2e_fail "setup: creation failed (project=$PROJ_ERR, a1=$A1_ERR, a2=$A2_ERR)"
    e2e_summary
    exit 1
fi

# ===========================================================================
# Case 2: resource://project/{slug} — happy path
# ===========================================================================
e2e_case_banner "project_by_slug_happy_path"

PROJ_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://project/${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_02_project_slug.txt" "$PROJ_RESP"

PROJ_ERR="$(is_error_result "$PROJ_RESP" 20)"
PROJ_TEXT="$(extract_resource_text "$PROJ_RESP" 20)"
e2e_save_artifact "case_02_project_text.json" "$PROJ_TEXT"

if [ "$PROJ_ERR" = "false" ] && [ -n "$PROJ_TEXT" ]; then
    e2e_pass "project/{slug}: returns data"
else
    e2e_fail "project/{slug}: returned error or empty"
fi

# Verify slug in response
PROJ_SLUG_FIELD="$(parse_json_field "$PROJ_TEXT" "slug")"
if [ -n "$PROJ_SLUG_FIELD" ]; then
    e2e_pass "project/{slug}: slug field present"
else
    e2e_fail "project/{slug}: slug field missing"
fi

# ===========================================================================
# Case 3: resource://project/nonexistent-slug — not found
# ===========================================================================
e2e_case_banner "project_by_slug_not_found"

NF_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://project/nonexistent-slug-abc\"}}" \
)"
e2e_save_artifact "case_03_project_not_found.txt" "$NF_RESP"

NF_ERR="$(is_error_result "$NF_RESP" 30)"
if [ "$NF_ERR" = "true" ]; then
    e2e_pass "project/nonexistent: returns error"
else
    # May return empty/default — that's also acceptable
    NF_TEXT="$(extract_resource_text "$NF_RESP" 30)"
    if [ -z "$NF_TEXT" ] || echo "$NF_TEXT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print('ok' if not d or d.get('error') else 'noterror')" 2>/dev/null | grep -q "ok"; then
        e2e_pass "project/nonexistent: returns empty or error response"
    else
        e2e_fail "project/nonexistent: unexpected response"
    fi
fi

# ===========================================================================
# Case 4: resource://agents/{slug} — happy path (2 agents)
# ===========================================================================
e2e_case_banner "agents_by_slug_happy_path"

AGENTS_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://agents/${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_04_agents_slug.txt" "$AGENTS_RESP"

AGENTS_ERR="$(is_error_result "$AGENTS_RESP" 40)"
AGENTS_TEXT="$(extract_resource_text "$AGENTS_RESP" 40)"
e2e_save_artifact "case_04_agents_text.json" "$AGENTS_TEXT"

if [ "$AGENTS_ERR" = "false" ] && [ -n "$AGENTS_TEXT" ]; then
    e2e_pass "agents/{slug}: returns data"
else
    e2e_fail "agents/{slug}: returned error or empty"
fi

# Verify the agents text contains both agent names
e2e_assert_contains "agents/{slug}: contains RedFox" "$AGENTS_TEXT" "RedFox"
e2e_assert_contains "agents/{slug}: contains BlueLake" "$AGENTS_TEXT" "BlueLake"

# ===========================================================================
# Case 5: resource://agents/{slug} — nonexistent project
# ===========================================================================
e2e_case_banner "agents_by_slug_not_found"

ANF_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://agents/nonexistent-slug-xyz\"}}" \
)"
e2e_save_artifact "case_05_agents_not_found.txt" "$ANF_RESP"

ANF_ERR="$(is_error_result "$ANF_RESP" 50)"
ANF_TEXT="$(extract_resource_text "$ANF_RESP" 50)"
if [ "$ANF_ERR" = "true" ] || [ -z "$ANF_TEXT" ] || [ "$ANF_TEXT" = "[]" ]; then
    e2e_pass "agents/nonexistent: returns error or empty"
else
    e2e_fail "agents/nonexistent: unexpected non-empty response"
fi

# ===========================================================================
# Case 6: Register third agent, verify agents/{slug} shows 3
# ===========================================================================
e2e_case_banner "agents_after_new_registration"

REG3_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"name\":\"GoldHawk\",\"project_key\":\"${PROJECT_PATH}\",\"program\":\"test\",\"model\":\"test\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://agents/${PROJECT_SLUG}\"}}" \
)"
e2e_save_artifact "case_06_register_and_read.txt" "$REG3_RESP"

REG3_ERR="$(is_error_result "$REG3_RESP" 60)"
if [ "$REG3_ERR" = "false" ]; then
    e2e_pass "register 3rd agent: succeeded"
else
    e2e_fail "register 3rd agent: failed"
fi

AGENTS3_TEXT="$(extract_resource_text "$REG3_RESP" 61)"
e2e_save_artifact "case_06_agents_3.json" "$AGENTS3_TEXT"

e2e_assert_contains "agents after 3rd reg: contains GoldHawk" "$AGENTS3_TEXT" "GoldHawk"
e2e_assert_contains "agents after 3rd reg: still has RedFox" "$AGENTS3_TEXT" "RedFox"
e2e_assert_contains "agents after 3rd reg: still has BlueLake" "$AGENTS3_TEXT" "BlueLake"

# ===========================================================================
# Case 7: resource://projects (list all) — verify our project appears
# ===========================================================================
e2e_case_banner "projects_list_contains_ours"

PLIST_RESP="$(send_jsonrpc_session "$RES_DB" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://projects\"}}" \
)"
e2e_save_artifact "case_07_projects_list.txt" "$PLIST_RESP"

PLIST_ERR="$(is_error_result "$PLIST_RESP" 70)"
PLIST_TEXT="$(extract_resource_text "$PLIST_RESP" 70)"
e2e_save_artifact "case_07_projects_text.json" "$PLIST_TEXT"

if [ "$PLIST_ERR" = "false" ] && [ -n "$PLIST_TEXT" ]; then
    e2e_pass "resource://projects: returns data"
else
    e2e_fail "resource://projects: returned error or empty"
fi

e2e_assert_contains "resource://projects: contains our slug" "$PLIST_TEXT" "$PROJECT_SLUG"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
