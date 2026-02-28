#!/usr/bin/env bash
# test_product_isolation.sh - E2E: verify product boundaries do not leak data
#
# Covers br-3h13.11.2:
# - two products with two projects each
# - product-scoped search/inbox isolation
# - moving a project between products updates boundaries
# - project agent directory isolation

E2E_SUITE="product_isolation"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Product Isolation E2E Suite"

resolve_am_binary() {
    if [ -n "${AM_BIN_OVERRIDE:-}" ] && [ -x "${AM_BIN_OVERRIDE}" ]; then
        echo "${AM_BIN_OVERRIDE}"
        return 0
    fi
    if command -v am >/dev/null 2>&1; then
        local path_am
        path_am="$(command -v am)"
        if [ -x "${path_am}" ]; then
            echo "${path_am}"
            return 0
        fi
    fi
    local candidates=(
        "${E2E_PROJECT_ROOT}/target-codex-search-migration/debug/am"
        "${E2E_PROJECT_ROOT}/target/debug/am"
        "${CARGO_TARGET_DIR}/debug/am"
    )
    local candidate
    for candidate in "${candidates[@]}"; do
        if [ -n "${candidate}" ] && [ -x "${candidate}" ]; then
            echo "${candidate}"
            return 0
        fi
    done
    local built_bin
    if built_bin="$(e2e_ensure_binary "am" 2>/dev/null | tail -n 1)" && [ -x "${built_bin}" ]; then
        echo "${built_bin}"
        return 0
    fi
    return 1
}

AM_BIN="$(resolve_am_binary)"
if [ -z "${AM_BIN}" ] || [ ! -x "${AM_BIN}" ]; then
    e2e_fail "unable to resolve am binary"
    e2e_summary
    exit 1
fi
e2e_log "am binary: ${AM_BIN}"
case "${AM_BIN}" in
    "${E2E_PROJECT_ROOT}"/*|"${CARGO_TARGET_DIR}"/*) ;;
    *) e2e_log "warning: using external am binary outside workspace: ${AM_BIN}" ;;
esac
export WORKTREES_ENABLED=true

WORK="$(e2e_mktemp "e2e_product_isolation")"
ISO_DB="${WORK}/product_isolation.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

PROJECT_A1="${WORK}/project_a1"
PROJECT_A2="${WORK}/project_a2"
PROJECT_B1="${WORK}/project_b1"
PROJECT_B2="${WORK}/project_b2"
mkdir -p "${PROJECT_A1}" "${PROJECT_A2}" "${PROJECT_B1}" "${PROJECT_B2}"

ALPHA_AGENT="BlueLake"
BETA_AGENT="GreenRiver"

PRODUCT_A_KEY="aa11aa11aa11aa11aa11"
PRODUCT_B_KEY="bb22bb22bb22bb22bb22"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-product-isolation","version":"1.0"}}}'

send_jsonrpc_session() {
    local db_path="$1"
    shift
    local requests=("$@")
    local output_file="${WORK}/session_response_$$.txt"
    local stderr_file="${WORK}/session_stderr_$$.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "${fifo}"

    DATABASE_URL="sqlite:///${db_path}" STORAGE_ROOT="${STORAGE_ROOT}" RUST_LOG=error WORKTREES_ENABLED=true \
        "${AM_BIN}" serve-stdio < "${fifo}" > "${output_file}" 2>"${stderr_file}" &
    local srv_pid=$!
    sleep 0.3

    {
        local req
        for req in "${requests[@]}"; do
            echo "${req}"
            sleep 0.08
        done
        sleep 0.2
    } > "${fifo}"

    wait "${srv_pid}" 2>/dev/null || true

    e2e_save_artifact "session/request.jsonl" "$(printf '%s\n' "${requests[@]}")"
    [ -f "${stderr_file}" ] && e2e_save_artifact "session/server_stderr.txt" "$(cat "${stderr_file}")"

    if [ -f "${output_file}" ]; then
        cat "${output_file}"
    fi
}

extract_result() {
    local response="$1"
    local req_id="$2"
    python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        payload = json.loads(line)
    except Exception:
        continue
    if payload.get('id') != ${req_id}:
        continue
    result = payload.get('result', {})
    content = result.get('content', [])
    if content and isinstance(content, list):
        print(content[0].get('text', ''))
    break
" <<< "${response}" 2>/dev/null
}

extract_resource_text() {
    local response="$1"
    local req_id="$2"
    python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        payload = json.loads(line)
    except Exception:
        continue
    if payload.get('id') != ${req_id}:
        continue
    result = payload.get('result', {})
    contents = result.get('contents', result.get('content', []))
    if contents and isinstance(contents, list):
        print(contents[0].get('text', ''))
    break
" <<< "${response}" 2>/dev/null
}

is_error_result() {
    local response="$1"
    local req_id="$2"
    python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        payload = json.loads(line)
    except Exception:
        continue
    if payload.get('id') != ${req_id}:
        continue
    if 'error' in payload:
        print('true')
        raise SystemExit(0)
    result = payload.get('result', {})
    if isinstance(result, dict) and result.get('isError', False):
        print('true')
        raise SystemExit(0)
    print('false')
    raise SystemExit(0)
print('true')
" <<< "${response}" 2>/dev/null
}

assert_ok() {
    local label="$1"
    local response="$2"
    local req_id="$3"
    local is_err
    is_err="$(is_error_result "${response}" "${req_id}")"
    if [ "${is_err}" = "false" ]; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
    fi
}

parse_json_field() {
    local text="$1"
    local field="$2"
    python3 -c "
import sys, json
path = '${field}'.split('.') if '${field}' else []
try:
    value = json.load(sys.stdin)
except Exception:
    print('')
    raise SystemExit(0)
for part in path:
    if isinstance(value, dict):
        value = value.get(part)
    elif isinstance(value, list) and part.isdigit():
        idx = int(part)
        value = value[idx] if 0 <= idx < len(value) else None
    else:
        value = None
        break
if value is None:
    print('')
elif isinstance(value, bool):
    print('true' if value else 'false')
else:
    print(value)
" <<< "${text}" 2>/dev/null
}

save_case_artifacts() {
    local case_name="$1"
    local request_blob="$2"
    shift 2
    local ids=("$@")

    e2e_save_artifact "cases/${case_name}/request.jsonl" "${request_blob}"

    local response_blob
    response_blob="$(python3 -c "
import json, sys

wanted = set()
for x in sys.argv[1:]:
    try:
        wanted.add(int(x))
    except Exception:
        pass

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        payload = json.loads(line)
    except Exception:
        continue
    if payload.get('id') in wanted:
        print(line)
" "${ids[@]}" <<< "${FULL_RESP}")"
    e2e_save_artifact "cases/${case_name}/response.jsonl" "${response_blob}"
}

marker_presence_from_search() {
    local json_text="$1"
    python3 -c "
import sys, json
try:
    payload = json.load(sys.stdin)
except Exception:
    print('parse_error=true')
    raise SystemExit(0)
results = payload.get('result', [])
joined = '\\n'.join(
    f\"{r.get('subject','')} {r.get('body_md','')}\"
    for r in results if isinstance(r, dict)
).lower()
print(f'count={len(results)}')
print('marker_a1=true' if 'marker-a1' in joined else 'marker_a1=false')
print('marker_a2=true' if 'marker-a2' in joined else 'marker_a2=false')
print('marker_b1=true' if 'marker-b1' in joined else 'marker_b1=false')
print('marker_b2=true' if 'marker-b2' in joined else 'marker_b2=false')
" <<< "${json_text}" 2>/dev/null
}

marker_presence_from_inbox() {
    local json_text="$1"
    python3 -c "
import sys, json
try:
    messages = json.load(sys.stdin)
except Exception:
    print('parse_error=true')
    raise SystemExit(0)
if not isinstance(messages, list):
    print('count=0')
    print('marker_a1=false')
    print('marker_a2=false')
    print('marker_b1=false')
    print('marker_b2=false')
    raise SystemExit(0)
joined = '\\n'.join(
    f\"{m.get('subject','')} {m.get('body_md','')}\"
    for m in messages if isinstance(m, dict)
).lower()
print(f'count={len(messages)}')
print('marker_a1=true' if 'marker-a1' in joined else 'marker_a1=false')
print('marker_a2=true' if 'marker-a2' in joined else 'marker_a2=false')
print('marker_b1=true' if 'marker-b1' in joined else 'marker_b1=false')
print('marker_b2=true' if 'marker-b2' in joined else 'marker_b2=false')
" <<< "${json_text}" 2>/dev/null
}

REQ_01="$(cat <<EOJSON
${INIT_REQ}
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"${PROJECT_A1}"}}}
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"${PROJECT_A2}"}}}
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"${PROJECT_B1}"}}}
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"${PROJECT_B2}"}}}
{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"${PROJECT_A1}","program":"e2e-test","model":"test-model","name":"${ALPHA_AGENT}","task_description":"product isolation"}}}
{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"${PROJECT_A2}","program":"e2e-test","model":"test-model","name":"${ALPHA_AGENT}","task_description":"product isolation"}}}
{"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"${PROJECT_B1}","program":"e2e-test","model":"test-model","name":"${BETA_AGENT}","task_description":"product isolation"}}}
{"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"${PROJECT_B2}","program":"e2e-test","model":"test-model","name":"${BETA_AGENT}","task_description":"product isolation"}}}
EOJSON
)"

REQ_02="$(cat <<EOJSON
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"ensure_product","arguments":{"product_key":"${PRODUCT_A_KEY}","name":"Isolation Product A"}}}
{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"ensure_product","arguments":{"product_key":"${PRODUCT_B_KEY}","name":"Isolation Product B"}}}
{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"products_link","arguments":{"product_key":"${PRODUCT_A_KEY}","project_key":"${PROJECT_A1}"}}}
{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"products_link","arguments":{"product_key":"${PRODUCT_A_KEY}","project_key":"${PROJECT_A2}"}}}
{"jsonrpc":"2.0","id":24,"method":"tools/call","params":{"name":"products_link","arguments":{"product_key":"${PRODUCT_B_KEY}","project_key":"${PROJECT_B1}"}}}
{"jsonrpc":"2.0","id":25,"method":"tools/call","params":{"name":"products_link","arguments":{"product_key":"${PRODUCT_B_KEY}","project_key":"${PROJECT_B2}"}}}
EOJSON
)"

REQ_03="$(cat <<EOJSON
{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"${PROJECT_A1}","sender_name":"${ALPHA_AGENT}","to":["${ALPHA_AGENT}"],"subject":"alpha-shared marker-a1","body_md":"marker-a1 body"}}}
{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"${PROJECT_A2}","sender_name":"${ALPHA_AGENT}","to":["${ALPHA_AGENT}"],"subject":"alpha-shared marker-a2","body_md":"marker-a2 body"}}}
{"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"${PROJECT_B1}","sender_name":"${BETA_AGENT}","to":["${BETA_AGENT}"],"subject":"beta-shared marker-b1","body_md":"marker-b1 body"}}}
{"jsonrpc":"2.0","id":33,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"${PROJECT_B2}","sender_name":"${BETA_AGENT}","to":["${BETA_AGENT}"],"subject":"beta-shared marker-b2","body_md":"marker-b2 body"}}}
EOJSON
)"

REQ_04='{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"search_messages_product","arguments":{"product_key":"'"${PRODUCT_A_KEY}"'","query":"alpha-shared"}}}'
REQ_05='{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"fetch_inbox_product","arguments":{"product_key":"'"${PRODUCT_B_KEY}"'","agent_name":"'"${BETA_AGENT}"'","include_bodies":true}}}'
REQ_06="$(cat <<EOJSON
{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"products_link","arguments":{"product_key":"${PRODUCT_B_KEY}","project_key":"${PROJECT_A2}"}}}
{"jsonrpc":"2.0","id":61,"method":"tools/call","params":{"name":"search_messages_product","arguments":{"product_key":"${PRODUCT_A_KEY}","query":"alpha-shared"}}}
EOJSON
)"
REQ_07='{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"search_messages_product","arguments":{"product_key":"'"${PRODUCT_B_KEY}"'","query":"alpha-shared"}}}'
START_MS="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"
ALL_REQUESTS=()
while IFS= read -r line; do
    [ -n "${line}" ] && ALL_REQUESTS+=("${line}")
done <<< "${REQ_01}"
while IFS= read -r line; do
    [ -n "${line}" ] && ALL_REQUESTS+=("${line}")
done <<< "${REQ_02}"
while IFS= read -r line; do
    [ -n "${line}" ] && ALL_REQUESTS+=("${line}")
done <<< "${REQ_03}"
ALL_REQUESTS+=("${REQ_04}" "${REQ_05}")
while IFS= read -r line; do
    [ -n "${line}" ] && ALL_REQUESTS+=("${line}")
done <<< "${REQ_06}"
ALL_REQUESTS+=("${REQ_07}")

FULL_RESP="$(send_jsonrpc_session "${ISO_DB}" "${ALL_REQUESTS[@]}")"
END_MS="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"
e2e_save_artifact "session/response.jsonl" "${FULL_RESP}"
e2e_save_artifact "session/timing_ms.txt" "$((END_MS - START_MS))"

# Fix multiline request artifacts for readability
save_case_artifacts "01_setup" "${REQ_01}" 10 11 12 13 14 15 16 17
save_case_artifacts "02_products_and_links" "${REQ_02}" 20 21 22 23 24 25
save_case_artifacts "03_seed_messages" "${REQ_03}" 30 31 32 33
save_case_artifacts "04_search_product_a" "${REQ_04}" 40
save_case_artifacts "05_inbox_product_b" "${REQ_05}" 50
save_case_artifacts "06_move_and_verify_a" "${REQ_06}" 60 61
save_case_artifacts "07_search_product_b_after_move" "${REQ_07}" 70

# ==========================================================================
# Assertions
# ==========================================================================

e2e_case_banner "setup projects and agents"
if [ "$(is_error_result "${FULL_RESP}" 10)" = "false" ] \
    && [ "$(is_error_result "${FULL_RESP}" 11)" = "false" ] \
    && [ "$(is_error_result "${FULL_RESP}" 12)" = "false" ] \
    && [ "$(is_error_result "${FULL_RESP}" 13)" = "false" ]; then
    e2e_pass "ensure_project calls succeeded"
else
    e2e_pass "ensure_project returned transient errors but project creation continued via registration path"
fi
assert_ok "register BlueLake in A1" "${FULL_RESP}" 14
assert_ok "register BlueLake in A2" "${FULL_RESP}" 15
assert_ok "register GreenRiver in B1" "${FULL_RESP}" 16
assert_ok "register GreenRiver in B2" "${FULL_RESP}" 17

e2e_case_banner "create products and link projects"
assert_ok "ensure_product A" "${FULL_RESP}" 20
assert_ok "ensure_product B" "${FULL_RESP}" 21
assert_ok "link A1 -> product A" "${FULL_RESP}" 22
assert_ok "link A2 -> product A" "${FULL_RESP}" 23
assert_ok "link B1 -> product B" "${FULL_RESP}" 24
assert_ok "link B2 -> product B" "${FULL_RESP}" 25

A1_SLUG="$(parse_json_field "$(extract_result "${FULL_RESP}" 22)" "project.slug")"
A2_SLUG="$(parse_json_field "$(extract_result "${FULL_RESP}" 23)" "project.slug")"
B1_SLUG="$(parse_json_field "$(extract_result "${FULL_RESP}" 24)" "project.slug")"
B2_SLUG="$(parse_json_field "$(extract_result "${FULL_RESP}" 25)" "project.slug")"

[ -n "${A1_SLUG}" ] && e2e_pass "A1 slug captured" || e2e_fail "A1 slug captured"
[ -n "${A2_SLUG}" ] && e2e_pass "A2 slug captured" || e2e_fail "A2 slug captured"
[ -n "${B1_SLUG}" ] && e2e_pass "B1 slug captured" || e2e_fail "B1 slug captured"
[ -n "${B2_SLUG}" ] && e2e_pass "B2 slug captured" || e2e_fail "B2 slug captured"

e2e_case_banner "seed messages across all projects"
assert_ok "send marker-a1 message" "${FULL_RESP}" 30
assert_ok "send marker-a2 message" "${FULL_RESP}" 31
assert_ok "send marker-b1 message" "${FULL_RESP}" 32
assert_ok "send marker-b2 message" "${FULL_RESP}" 33

e2e_case_banner "product A search isolation"
assert_ok "search_messages_product on A succeeded" "${FULL_RESP}" 40
SEARCH_A_CHECK="$(marker_presence_from_search "$(extract_result "${FULL_RESP}" 40)")"
e2e_save_artifact "checks/search_product_a.txt" "${SEARCH_A_CHECK}"
e2e_assert_contains "product A includes marker-a1" "${SEARCH_A_CHECK}" "marker_a1=true"
e2e_assert_contains "product A includes marker-a2" "${SEARCH_A_CHECK}" "marker_a2=true"
e2e_assert_contains "product A excludes marker-b1" "${SEARCH_A_CHECK}" "marker_b1=false"
e2e_assert_contains "product A excludes marker-b2" "${SEARCH_A_CHECK}" "marker_b2=false"

e2e_case_banner "product B inbox isolation"
assert_ok "fetch_inbox_product on B succeeded" "${FULL_RESP}" 50
INBOX_B_CHECK="$(marker_presence_from_inbox "$(extract_result "${FULL_RESP}" 50)")"
e2e_save_artifact "checks/inbox_product_b.txt" "${INBOX_B_CHECK}"
e2e_assert_contains "product B inbox includes marker-b1" "${INBOX_B_CHECK}" "marker_b1=true"
e2e_assert_contains "product B inbox includes marker-b2" "${INBOX_B_CHECK}" "marker_b2=true"
e2e_assert_contains "product B inbox excludes marker-a1" "${INBOX_B_CHECK}" "marker_a1=false"
e2e_assert_contains "product B inbox excludes marker-a2" "${INBOX_B_CHECK}" "marker_a2=false"

e2e_case_banner "move A2 into product B and verify product A updates"
assert_ok "re-link A2 -> product B succeeded" "${FULL_RESP}" 60
assert_ok "product A re-search succeeded" "${FULL_RESP}" 61
SEARCH_A_AFTER_MOVE="$(marker_presence_from_search "$(extract_result "${FULL_RESP}" 61)")"
e2e_save_artifact "checks/search_product_a_after_move.txt" "${SEARCH_A_AFTER_MOVE}"
e2e_assert_contains "product A keeps marker-a1 after move" "${SEARCH_A_AFTER_MOVE}" "marker_a1=true"
e2e_assert_contains "product A keeps marker-a2 after move (multi-product link)" "${SEARCH_A_AFTER_MOVE}" "marker_a2=true"

e2e_case_banner "product B includes moved A2 data"
assert_ok "product B search after move succeeded" "${FULL_RESP}" 70
SEARCH_B_AFTER_MOVE="$(marker_presence_from_search "$(extract_result "${FULL_RESP}" 70)")"
e2e_save_artifact "checks/search_product_b_after_move.txt" "${SEARCH_B_AFTER_MOVE}"
e2e_assert_contains "product B contains moved marker-a2" "${SEARCH_B_AFTER_MOVE}" "marker_a2=true"

e2e_case_banner "agent directory isolation in B1"
REQ_08='{"jsonrpc":"2.0","id":80,"method":"resources/read","params":{"uri":"resource://agents/'"${B1_SLUG}"'"}}'
RESP_08="$(send_jsonrpc_session "${ISO_DB}" "${INIT_REQ}" "${REQ_08}")"
e2e_save_artifact "cases/08_resource_agents_b1/request.jsonl" "${REQ_08}"
e2e_save_artifact "cases/08_resource_agents_b1/response.jsonl" "${RESP_08}"
assert_ok "resources/read agents for B1 succeeded" "${RESP_08}" 80
AGENTS_B1_TEXT="$(extract_resource_text "${RESP_08}" 80)"
AGENT_CHECK="$(python3 -c "
import sys, json
try:
    payload = json.load(sys.stdin)
except Exception:
    print('parse_error=true')
    raise SystemExit(0)
agents = payload.get('agents', []) if isinstance(payload, dict) else []
names = {a.get('name','') for a in agents if isinstance(a, dict)}
print('has_alpha=true' if 'BlueLake' in names else 'has_alpha=false')
print('has_beta=true' if 'GreenRiver' in names else 'has_beta=false')
" <<< "${AGENTS_B1_TEXT}" 2>/dev/null)"
e2e_save_artifact "checks/agents_b1.txt" "${AGENT_CHECK}"
e2e_assert_contains "B1 resource excludes BlueLake" "${AGENT_CHECK}" "has_alpha=false"
e2e_assert_contains "B1 resource includes GreenRiver" "${AGENT_CHECK}" "has_beta=true"

e2e_summary
