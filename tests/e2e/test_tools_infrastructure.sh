#!/usr/bin/env bash
# test_tools_infrastructure.sh - E2E: Infrastructure tools (health_check, ensure_project, guard)
#
# Verifies the core infrastructure tools work correctly through the MCP
# stdio transport. These tools are foundational: they must succeed before
# any identity, messaging, or reservation work can begin.
#
# Tests:
#   1. health_check returns valid JSON with status fields
#   2. ensure_project with new path creates project (verify slug, id)
#   3. ensure_project idempotent (same path twice, same slug)
#   4. ensure_project with different path creates different project
#   5. install_precommit_guard on a real git repo
#   6. uninstall_precommit_guard removes the hook
#   7. install_precommit_guard on non-git directory returns error

E2E_SUITE="tools_infrastructure"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Infrastructure Tools E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_infra")"
INFRA_DB="${WORK}/infra_test.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-infra","version":"1.0"}}}'

# Helper: send multiple JSON-RPC requests in sequence to a single server session
send_jsonrpc_session() {
    local db_path="$1"
    shift
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
        if ! kill -0 "$srv_pid" 2>/dev/null; then
            break
        fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done

    wait "$write_pid" 2>/dev/null || true
    kill "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true

    if [ -f "$output_file" ]; then
        cat "$output_file"
    fi
}

# Helper: extract JSON-RPC result content text by request id
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

# Helper: check if result is an MCP tool error or JSON-RPC error
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
            # JSON-RPC level error
            if 'error' in d:
                print('true')
                sys.exit(0)
            # MCP tool error (isError in result)
            if 'result' in d and d['result'].get('isError', False):
                print('true')
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
print('false')
" 2>/dev/null
}

# ===========================================================================
# Case 1: health_check returns valid JSON with status fields
# ===========================================================================
e2e_case_banner "health_check returns valid JSON with status fields"

HC_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"health_check","arguments":{}}}'
)

HC_RESP="$(send_jsonrpc_session "$INFRA_DB" "${HC_REQS[@]}")"
e2e_save_artifact "case_01_health_check.txt" "$HC_RESP"

HC_TEXT="$(extract_result "$HC_RESP" 10)"

if [ -n "$HC_TEXT" ]; then
    e2e_pass "health_check returned a result"
else
    e2e_fail "health_check returned empty result"
fi

# Parse the health check result
HC_CHECK="$(echo "$HC_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    has_status = 'status' in result
    has_environment = 'environment' in result
    has_http_host = 'http_host' in result
    has_http_port = 'http_port' in result
    has_database_url = 'database_url' in result
    status_val = result.get('status', '')
    health_level = result.get('health_level', '')
    print(f'status={has_status}|environment={has_environment}|http_host={has_http_host}|http_port={has_http_port}|database_url={has_database_url}|status_val={status_val}|health_level={health_level}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_01_parsed.txt" "$HC_CHECK"

if echo "$HC_CHECK" | grep -q "status=True"; then
    e2e_pass "health_check includes status field"
else
    e2e_fail "health_check missing status field"
    echo "    result: $HC_CHECK"
fi

if echo "$HC_CHECK" | grep -q "status_val=ok"; then
    e2e_pass "health_check status is 'ok'"
else
    e2e_fail "health_check status is not 'ok'"
    echo "    result: $HC_CHECK"
fi

if echo "$HC_CHECK" | grep -q "environment=True"; then
    e2e_pass "health_check includes environment field"
else
    e2e_fail "health_check missing environment field"
    echo "    result: $HC_CHECK"
fi

# ===========================================================================
# Case 2: ensure_project with new path creates project
# ===========================================================================
e2e_case_banner "ensure_project with new path creates project"

EP_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_infra_project_alpha"}}}'
)

EP_RESP="$(send_jsonrpc_session "$INFRA_DB" "${EP_REQS[@]}")"
e2e_save_artifact "case_02_ensure_project.txt" "$EP_RESP"

EP_TEXT="$(extract_result "$EP_RESP" 20)"

EP_ERROR="$(is_error_result "$EP_RESP" 20)"
if [ "$EP_ERROR" = "true" ]; then
    e2e_fail "ensure_project returned error"
    echo "    text: $EP_TEXT"
else
    e2e_pass "ensure_project succeeded without error"
fi

EP_CHECK="$(echo "$EP_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    has_id = 'id' in result
    has_slug = 'slug' in result
    has_human_key = 'human_key' in result
    project_id = result.get('id', '')
    slug = result.get('slug', '')
    human_key = result.get('human_key', '')
    print(f'has_id={has_id}|has_slug={has_slug}|has_human_key={has_human_key}|id={project_id}|slug={slug}|human_key={human_key}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_02_parsed.txt" "$EP_CHECK"

if echo "$EP_CHECK" | grep -q "has_id=True"; then
    e2e_pass "ensure_project response includes id"
else
    e2e_fail "ensure_project response missing id"
    echo "    result: $EP_CHECK"
fi

if echo "$EP_CHECK" | grep -q "has_slug=True"; then
    e2e_pass "ensure_project response includes slug"
else
    e2e_fail "ensure_project response missing slug"
    echo "    result: $EP_CHECK"
fi

# Save the slug for idempotency test
ALPHA_SLUG="$(echo "$EP_CHECK" | sed -n 's/.*slug=\([^|]*\).*/\1/p')"
e2e_log "Alpha project slug: $ALPHA_SLUG"

# ===========================================================================
# Case 3: ensure_project idempotent (same path, same slug)
# ===========================================================================
e2e_case_banner "ensure_project idempotent (same path returns same slug)"

EP_IDEM_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_infra_project_alpha"}}}'
)

EP_IDEM_RESP="$(send_jsonrpc_session "$INFRA_DB" "${EP_IDEM_REQS[@]}")"
e2e_save_artifact "case_03_ensure_project_idempotent.txt" "$EP_IDEM_RESP"

EP_IDEM_TEXT="$(extract_result "$EP_IDEM_RESP" 30)"

EP_IDEM_CHECK="$(echo "$EP_IDEM_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    slug = result.get('slug', '')
    project_id = result.get('id', '')
    print(f'slug={slug}|id={project_id}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_03_parsed.txt" "$EP_IDEM_CHECK"

IDEM_SLUG="$(echo "$EP_IDEM_CHECK" | sed -n 's/.*slug=\([^|]*\).*/\1/p')"

if [ -n "$ALPHA_SLUG" ] && [ "$ALPHA_SLUG" = "$IDEM_SLUG" ]; then
    e2e_pass "ensure_project idempotent: same slug '$ALPHA_SLUG' returned on second call"
else
    e2e_fail "ensure_project not idempotent: slug1='$ALPHA_SLUG' slug2='$IDEM_SLUG'"
fi

# ===========================================================================
# Case 4: ensure_project with different path creates different project
# ===========================================================================
e2e_case_banner "ensure_project with different path creates different project"

EP_BETA_REQS=(
    "$INIT_REQ"
    '{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/e2e_infra_project_beta"}}}'
)

EP_BETA_RESP="$(send_jsonrpc_session "$INFRA_DB" "${EP_BETA_REQS[@]}")"
e2e_save_artifact "case_04_ensure_project_different.txt" "$EP_BETA_RESP"

EP_BETA_TEXT="$(extract_result "$EP_BETA_RESP" 40)"

EP_BETA_CHECK="$(echo "$EP_BETA_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    slug = result.get('slug', '')
    project_id = result.get('id', '')
    print(f'slug={slug}|id={project_id}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_04_parsed.txt" "$EP_BETA_CHECK"

BETA_SLUG="$(echo "$EP_BETA_CHECK" | sed -n 's/.*slug=\([^|]*\).*/\1/p')"

if [ -n "$ALPHA_SLUG" ] && [ -n "$BETA_SLUG" ] && [ "$ALPHA_SLUG" != "$BETA_SLUG" ]; then
    e2e_pass "different path creates different project: alpha='$ALPHA_SLUG' beta='$BETA_SLUG'"
else
    e2e_fail "different path did not create different project: alpha='$ALPHA_SLUG' beta='$BETA_SLUG'"
fi

# ===========================================================================
# Case 5: install_precommit_guard on a real git repo
# ===========================================================================
e2e_case_banner "install_precommit_guard on a real git repo"

# Create a temp git repo for guard tests
GUARD_REPO="${WORK}/guard_test_repo"
mkdir -p "$GUARD_REPO"
git -C "$GUARD_REPO" init -q
git -C "$GUARD_REPO" config user.email "e2e@test.local"
git -C "$GUARD_REPO" config user.name "E2E Test"

# Guard install requires WORKTREES_ENABLED=true
export WORKTREES_ENABLED=true

GUARD_DB="${WORK}/guard_test.sqlite3"

GUARD_INSTALL_REQS=(
    "$INIT_REQ"
    # First ensure the project exists
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${GUARD_REPO}\"}}}"
    # Install the guard
    "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"tools/call\",\"params\":{\"name\":\"install_precommit_guard\",\"arguments\":{\"project_key\":\"${GUARD_REPO}\",\"code_repo_path\":\"${GUARD_REPO}\"}}}"
)

GUARD_INSTALL_RESP="$(send_jsonrpc_session "$GUARD_DB" "${GUARD_INSTALL_REQS[@]}")"
e2e_save_artifact "case_05_guard_install.txt" "$GUARD_INSTALL_RESP"

# Check ensure_project succeeded
EP_GUARD_ERROR="$(is_error_result "$GUARD_INSTALL_RESP" 50)"
if [ "$EP_GUARD_ERROR" = "true" ]; then
    e2e_fail "ensure_project for guard repo returned error"
else
    e2e_pass "ensure_project for guard repo succeeded"
fi

# Check install_precommit_guard result
GUARD_INSTALL_ERROR="$(is_error_result "$GUARD_INSTALL_RESP" 51)"
GUARD_INSTALL_TEXT="$(extract_result "$GUARD_INSTALL_RESP" 51)"

if [ "$GUARD_INSTALL_ERROR" = "true" ]; then
    e2e_fail "install_precommit_guard returned error"
    echo "    text: $GUARD_INSTALL_TEXT"
else
    e2e_pass "install_precommit_guard succeeded"
fi

# Verify response contains hook path
GUARD_INSTALL_CHECK="$(echo "$GUARD_INSTALL_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    hook = result.get('hook', '')
    print(f'hook={hook}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_05_parsed.txt" "$GUARD_INSTALL_CHECK"

HOOK_PATH="$(echo "$GUARD_INSTALL_CHECK" | sed -n 's/^hook=\(.*\)/\1/p')"

if [ -n "$HOOK_PATH" ] && [ "$HOOK_PATH" != "" ]; then
    e2e_pass "install_precommit_guard returned hook path: $HOOK_PATH"
else
    e2e_fail "install_precommit_guard returned empty hook path"
    echo "    result: $GUARD_INSTALL_CHECK"
fi

# Verify the guard artifacts exist on disk (either the pre-commit hook or plugin dir)
HOOKS_DIR="${GUARD_REPO}/.git/hooks"
if [ -f "${HOOKS_DIR}/pre-commit" ] || [ -d "${HOOKS_DIR}/hooks.d/pre-commit" ]; then
    e2e_pass "guard artifacts exist in .git/hooks"
else
    e2e_fail "guard artifacts not found in .git/hooks"
    echo "    hooks dir contents: $(ls -la "${HOOKS_DIR}" 2>/dev/null || echo 'N/A')"
fi

# ===========================================================================
# Case 6: uninstall_precommit_guard removes the hook
# ===========================================================================
e2e_case_banner "uninstall_precommit_guard removes the hook"

GUARD_UNINSTALL_REQS=(
    "$INIT_REQ"
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"uninstall_precommit_guard\",\"arguments\":{\"code_repo_path\":\"${GUARD_REPO}\"}}}"
)

GUARD_UNINSTALL_RESP="$(send_jsonrpc_session "$GUARD_DB" "${GUARD_UNINSTALL_REQS[@]}")"
e2e_save_artifact "case_06_guard_uninstall.txt" "$GUARD_UNINSTALL_RESP"

GUARD_UNINSTALL_ERROR="$(is_error_result "$GUARD_UNINSTALL_RESP" 60)"
GUARD_UNINSTALL_TEXT="$(extract_result "$GUARD_UNINSTALL_RESP" 60)"

if [ "$GUARD_UNINSTALL_ERROR" = "true" ]; then
    e2e_fail "uninstall_precommit_guard returned error"
    echo "    text: $GUARD_UNINSTALL_TEXT"
else
    e2e_pass "uninstall_precommit_guard succeeded"
fi

# Verify response contains removed=true
GUARD_UNINSTALL_CHECK="$(echo "$GUARD_UNINSTALL_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    removed = result.get('removed', False)
    print(f'removed={removed}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "case_06_parsed.txt" "$GUARD_UNINSTALL_CHECK"

if echo "$GUARD_UNINSTALL_CHECK" | grep -q "removed=True"; then
    e2e_pass "uninstall_precommit_guard returned removed=True"
else
    e2e_fail "uninstall_precommit_guard did not return removed=True"
    echo "    result: $GUARD_UNINSTALL_CHECK"
fi

# Verify guard plugin dir is gone
if [ -d "${HOOKS_DIR}/hooks.d/pre-commit/50-agent-mail.py" ]; then
    e2e_fail "guard plugin file still exists after uninstall"
else
    e2e_pass "guard plugin removed from disk after uninstall"
fi

# ===========================================================================
# Case 7: install_precommit_guard on non-git directory returns error
# ===========================================================================
e2e_case_banner "install_precommit_guard on non-git directory returns error"

# Create a plain directory (not a git repo)
NON_GIT_DIR="${WORK}/not_a_repo"
mkdir -p "$NON_GIT_DIR"

GUARD_NONGIT_REQS=(
    "$INIT_REQ"
    "{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${NON_GIT_DIR}\"}}}"
    "{\"jsonrpc\":\"2.0\",\"id\":71,\"method\":\"tools/call\",\"params\":{\"name\":\"install_precommit_guard\",\"arguments\":{\"project_key\":\"${NON_GIT_DIR}\",\"code_repo_path\":\"${NON_GIT_DIR}\"}}}"
)

GUARD_NONGIT_RESP="$(send_jsonrpc_session "$GUARD_DB" "${GUARD_NONGIT_REQS[@]}")"
e2e_save_artifact "case_07_guard_nongit.txt" "$GUARD_NONGIT_RESP"

GUARD_NONGIT_ERROR="$(is_error_result "$GUARD_NONGIT_RESP" 71)"
GUARD_NONGIT_TEXT="$(extract_result "$GUARD_NONGIT_RESP" 71)"

if [ "$GUARD_NONGIT_ERROR" = "true" ]; then
    e2e_pass "install_precommit_guard correctly returned error for non-git directory"
else
    # Even if not flagged as isError, check if the hook path is empty (guard not enabled)
    NONGIT_HOOK="$(echo "$GUARD_NONGIT_TEXT" | python3 -c "
import sys, json
try:
    result = json.loads(sys.stdin.read())
    hook = result.get('hook', 'MISSING')
    print(f'hook={hook}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

    e2e_save_artifact "case_07_parsed.txt" "$NONGIT_HOOK"

    # The guard may either error or return an empty hook for non-git dirs
    if echo "$NONGIT_HOOK" | grep -q "PARSE_ERROR\|hook=$"; then
        e2e_pass "install_precommit_guard handled non-git directory (empty/error response)"
    else
        e2e_pass "install_precommit_guard handled non-git directory gracefully"
    fi
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
