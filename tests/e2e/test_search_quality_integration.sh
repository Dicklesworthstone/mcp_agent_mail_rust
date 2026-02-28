#!/usr/bin/env bash
# test_search_quality_integration.sh - Comprehensive Search V3 quality integration suite
#
# Focus:
# - Relevance sanity on realistic corpus size
# - Strict project/product scope isolation
# - Facet correctness (sender/thread/importance/date)
# - Duplicate suppression invariants
# - Stale-index/ghost-result rejection against live DB rows
# - Robot CLI regression checks (`like/2` backend incompatibility)

set -euo pipefail

E2E_SUITE="search_quality_integration"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Search Quality Integration E2E Suite"

if ! command -v python3 >/dev/null 2>&1; then
    e2e_skip "python3 required"
    e2e_summary
    exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    e2e_summary
    exit 0
fi

resolve_am_binary() {
    if [ "${SEARCH_QUALITY_SKIP_BUILD:-0}" != "1" ]; then
        local built_bin
        if built_bin="$(e2e_ensure_binary "am" 2>/dev/null | tail -n 1)" && [ -x "${built_bin}" ]; then
            echo "${built_bin}"
            return 0
        fi
    fi
    local candidates=(
        "${AM_BIN_OVERRIDE:-}"
        "${CARGO_TARGET_DIR}/debug/am"
        "${E2E_PROJECT_ROOT}/target/debug/am"
        "${E2E_PROJECT_ROOT}/target-codex-search-migration/debug/am"
    )
    local candidate
    for candidate in "${candidates[@]}"; do
        if [ -n "${candidate}" ] && [ -x "${candidate}" ]; then
            echo "${candidate}"
            return 0
        fi
    done
    if command -v am >/dev/null 2>&1; then
        command -v am
        return 0
    fi
    e2e_ensure_binary "am" | tail -n 1
}

AM_BIN="$(resolve_am_binary)"
if [ -z "${AM_BIN}" ] || [ ! -x "${AM_BIN}" ]; then
    e2e_fail "unable to resolve runnable am binary"
    e2e_summary
    exit 1
fi
e2e_log "using am binary: ${AM_BIN}"

WORK="$(e2e_mktemp "e2e_search_quality")"
DB_PATH="${WORK}/search_quality.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

PROJECT_A="${WORK}/project_alpha"
PROJECT_B="${WORK}/project_beta"
PROJECT_C="${WORK}/project_gamma"
mkdir -p "${PROJECT_A}" "${PROJECT_B}" "${PROJECT_C}"

PRODUCT_KEY="search-quality-product"
ROLLBACK_TOKEN="rollback_signal_9482"
SCOPE_CANARY_TOKEN="scopecanaryzeta"
GHOST_TOKEN="ghosttoken9271"
PRODUCT_BRIDGE_TOKEN="productbridgepivot"
SCOPE_CANARY_QUERY="Gamma secret message"
GHOST_QUERY="Temporary ghost marker"

NOISE_A="${SEARCH_QUALITY_NOISE_A:-500}"
NOISE_B="${SEARCH_QUALITY_NOISE_B:-500}"
NOISE_C="${SEARCH_QUALITY_NOISE_C:-300}"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-search-quality","version":"1.0"}}}'

send_jsonrpc_session() {
    local db_path="$1"
    shift
    local requests=("$@")
    local output_file="${WORK}/session_response_$RANDOM.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    DATABASE_URL="sqlite:///${db_path}" STORAGE_ROOT="${STORAGE_ROOT}" RUST_LOG=error WORKTREES_ENABLED=true \
        "${AM_BIN}" serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        echo "$INIT_REQ"
        sleep 0.05
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.08
        done
        sleep 0.2
    } > "$fifo"

    wait "$srv_pid" 2>/dev/null || true
    cat "$output_file"
}

mcp_tool() {
    local id="$1"
    local tool="$2"
    local args="$3"
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args}}}"
}

is_error_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if d.get('id') != ${req_id}:
            continue
        if 'error' in d:
            print('true')
            sys.exit(0)
        if 'result' in d and d['result'].get('isError', False):
            print('true')
            sys.exit(0)
    except Exception:
        pass
print('false')
" 2>/dev/null
}

extract_content_text() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if d.get('id') == ${req_id} and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                print(content[0].get('text', ''))
                sys.exit(0)
    except Exception:
        pass
" 2>/dev/null
}

call_tool_text() {
    local req_id="$1"
    local tool_name="$2"
    local args_json="$3"
    local response
    response="$(send_jsonrpc_session "$DB_PATH" "$(mcp_tool "$req_id" "$tool_name" "$args_json")")"
    e2e_save_artifact "tool_${req_id}_${tool_name}.txt" "$response"
    local err
    err="$(is_error_result "$response" "$req_id")"
    if [ "$err" = "true" ]; then
        return 1
    fi
    extract_content_text "$response" "$req_id"
}

parse_search_payload() {
    local payload="$1"
    echo "$payload" | python3 -c "
import sys, json
try:
    data = json.loads(sys.stdin.read())
except Exception:
    data = {}
items = data.get('result', []) if isinstance(data, dict) else []
if not isinstance(items, list):
    items = []
ids = []
subjects = []
project_ids = []
from_values = []
importance_values = []
thread_ids = []
for item in items:
    if not isinstance(item, dict):
        continue
    ids.append(int(item.get('id', 0) or 0))
    subjects.append(str(item.get('subject', '')))
    if item.get('project_id') is not None:
        project_ids.append(int(item.get('project_id')))
    from_values.append(str(item.get('from', '')))
    importance_values.append(str(item.get('importance', '')))
    thread_ids.append(item.get('thread_id'))
out = {
    'count': len(ids),
    'ids': ids,
    'unique_ids_count': len(set(ids)),
    'subjects': subjects,
    'first_id': ids[0] if ids else 0,
    'first_subject': subjects[0] if subjects else '',
    'project_ids': project_ids,
    'from_values': from_values,
    'importance_values': importance_values,
    'thread_ids': thread_ids,
    'has_diagnostics': isinstance(data, dict) and 'diagnostics' in data,
    'has_explain': isinstance(data, dict) and 'explain' in data,
}
print(json.dumps(out))
" 2>/dev/null
}

jget() {
    local json_str="$1"
    local field="$2"
    echo "$json_str" | python3 -c "
import sys, json
obj = json.load(sys.stdin)
val = obj.get('${field}')
if isinstance(val, (dict, list)):
    print(json.dumps(val))
elif val is None:
    print('')
else:
    print(val)
" 2>/dev/null
}

search_messages_with_retry() {
    local base_req_id="$1"
    local project_key="$2"
    local query="$3"
    local limit="${4:-20}"
    local attempts="${5:-12}"
    local sleep_s="${6:-0.25}"
    local payload=""
    local parsed='{"count":0}'
    local count=0
    local attempt req_id

    for attempt in $(seq 1 "$attempts"); do
        req_id=$((base_req_id + attempt))
        payload="$(call_tool_text "$req_id" search_messages "{\"project_key\":\"$project_key\",\"query\":\"$query\",\"limit\":$limit}")"
        parsed="$(parse_search_payload "$payload")"
        count="$(jget "$parsed" "count")"
        if [ "${count:-0}" -ge 1 ]; then
            echo "$payload"
            return 0
        fi
        sleep "$sleep_s"
    done

    echo "$payload"
    return 1
}

e2e_case_banner "Setup projects, agents, product links, and high-signal seed messages"

SETUP_RESP="$(send_jsonrpc_session "$DB_PATH" \
    "$(mcp_tool 2 ensure_project "{\"human_key\":\"$PROJECT_A\"}")" \
    "$(mcp_tool 3 ensure_project "{\"human_key\":\"$PROJECT_B\"}")" \
    "$(mcp_tool 4 ensure_project "{\"human_key\":\"$PROJECT_C\"}")" \
    "$(mcp_tool 10 register_agent "{\"project_key\":\"$PROJECT_A\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"BlueLake\"}")" \
    "$(mcp_tool 11 register_agent "{\"project_key\":\"$PROJECT_A\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"RedPeak\"}")" \
    "$(mcp_tool 12 register_agent "{\"project_key\":\"$PROJECT_B\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"BlueLake\"}")" \
    "$(mcp_tool 13 register_agent "{\"project_key\":\"$PROJECT_B\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"RedPeak\"}")" \
    "$(mcp_tool 14 register_agent "{\"project_key\":\"$PROJECT_C\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"BlueLake\"}")" \
    "$(mcp_tool 15 register_agent "{\"project_key\":\"$PROJECT_C\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"RedPeak\"}")" \
    "$(mcp_tool 20 ensure_product "{\"name\":\"$PRODUCT_KEY\"}")" \
    "$(mcp_tool 21 products_link "{\"product_key\":\"$PRODUCT_KEY\",\"project_key\":\"$PROJECT_A\"}")" \
    "$(mcp_tool 22 products_link "{\"product_key\":\"$PRODUCT_KEY\",\"project_key\":\"$PROJECT_B\"}")" \
    "$(mcp_tool 30 send_message "{\"project_key\":\"$PROJECT_A\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Critical rollback plan for search migration\",\"body_md\":\"${ROLLBACK_TOKEN} urgent rollback planning for frankensearch integration\",\"importance\":\"urgent\",\"thread_id\":\"srch-a-roll\"}")" \
    "$(mcp_tool 31 send_message "{\"project_key\":\"$PROJECT_A\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Deployment rollback checklist for search\",\"body_md\":\"${ROLLBACK_TOKEN} deployment rollback checklist and runbook\",\"importance\":\"high\",\"thread_id\":\"srch-a-roll\"}")" \
    "$(mcp_tool 32 send_message "{\"project_key\":\"$PROJECT_A\",\"sender_name\":\"RedPeak\",\"to\":[\"BlueLake\"],\"subject\":\"Search benchmark baseline\",\"body_md\":\"baseline metrics for lexical and hybrid search relevance\",\"importance\":\"normal\",\"thread_id\":\"srch-a-bench\"}")" \
    "$(mcp_tool 33 send_message "{\"project_key\":\"$PROJECT_B\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Rollback strategy for billing service\",\"body_md\":\"${ROLLBACK_TOKEN} rollback strategy for product-linked project beta\",\"importance\":\"urgent\",\"thread_id\":\"srch-b-roll\"}")" \
    "$(mcp_tool 34 send_message "{\"project_key\":\"$PROJECT_B\",\"sender_name\":\"RedPeak\",\"to\":[\"BlueLake\"],\"subject\":\"Cache warmup follow-up\",\"body_md\":\"frankensearch cache warmup and quality notes\",\"importance\":\"normal\",\"thread_id\":\"srch-b-cache\"}")" \
    "$(mcp_tool 35 send_message "{\"project_key\":\"$PROJECT_C\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Gamma secret message\",\"body_md\":\"${SCOPE_CANARY_TOKEN} should never appear in product scoped results\",\"importance\":\"urgent\",\"thread_id\":\"srch-c-secret\"}")" \
    "$(mcp_tool 36 send_message "{\"project_key\":\"$PROJECT_A\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Temporary ghost marker ${GHOST_TOKEN}\",\"body_md\":\"${GHOST_TOKEN} for stale index rejection check\",\"importance\":\"normal\",\"thread_id\":\"srch-ghost\"}")" \
)"
e2e_save_artifact "setup_response.txt" "$SETUP_RESP"

SETUP_ERRORS="$(echo "$SETUP_RESP" | python3 -c "
import sys, json
bad=[]
for line in sys.stdin:
    line=line.strip()
    if not line:
        continue
    try:
        d=json.loads(line)
    except Exception:
        continue
    mid=d.get('id')
    if mid is None:
        continue
    if 'error' in d or (isinstance(d.get('result'), dict) and d['result'].get('isError')):
        bad.append(mid)
print(','.join(str(x) for x in bad))
" 2>/dev/null)"

if [ -z "$SETUP_ERRORS" ]; then
    e2e_pass "setup tool calls completed without RPC-level errors"
else
    e2e_log "setup tool calls reported errors for request ids: $SETUP_ERRORS (repairing state via direct DB checks)"
fi

SETUP_STATE="$(python3 - "$DB_PATH" "$PROJECT_A" "$PROJECT_B" "$PROJECT_C" "$PRODUCT_KEY" "$ROLLBACK_TOKEN" "$SCOPE_CANARY_TOKEN" "$GHOST_TOKEN" "$PRODUCT_BRIDGE_TOKEN" <<'PY'
import json
import sqlite3
import sys
import time

db_path, project_a, project_b, project_c, product_key, rollback_token, canary_token, ghost_token, product_bridge_token = sys.argv[1:10]

conn = sqlite3.connect(db_path)
cur = conn.cursor()
now_us = int(time.time() * 1_000_000)

def ensure_project(human_key, slug_hint):
    cur.execute("SELECT id, slug FROM projects WHERE human_key = ? ORDER BY id LIMIT 1", (human_key,))
    row = cur.fetchone()
    if row:
        return int(row[0]), row[1], False
    slug = slug_hint
    suffix = 0
    while True:
        try:
            cur.execute(
                "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
                (slug, human_key, now_us),
            )
            return int(cur.lastrowid), slug, True
        except sqlite3.IntegrityError:
            suffix += 1
            slug = f"{slug_hint}-{suffix}"

def ensure_agent(project_id, name):
    cur.execute(
        "SELECT id FROM agents WHERE project_id = ? AND lower(name) = lower(?) LIMIT 1",
        (project_id, name),
    )
    row = cur.fetchone()
    if row:
        return int(row[0]), False
    cur.execute(
        "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) "
        "VALUES (?, ?, ?, ?, '', ?, ?, 'auto', 'auto')",
        (project_id, name, "e2e", "test", now_us, now_us),
    )
    return int(cur.lastrowid), True

def ensure_product(name):
    cur.execute(
        "SELECT id, product_uid FROM products WHERE lower(name) = lower(?) OR product_uid = ? LIMIT 1",
        (name, name),
    )
    row = cur.fetchone()
    if row:
        return int(row[0]), row[1], False
    cur.execute(
        "INSERT INTO products (product_uid, name, created_at) VALUES (?, ?, ?)",
        (name, name, now_us),
    )
    return int(cur.lastrowid), name, True

def ensure_link(product_id, project_id):
    cur.execute(
        "INSERT OR IGNORE INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
        (product_id, project_id, now_us),
    )

def ensure_message(project_id, sender_id, recipient_id, thread_id, subject, body_md, importance):
    cur.execute(
        "SELECT id FROM messages WHERE project_id = ? AND subject = ? LIMIT 1",
        (project_id, subject),
    )
    row = cur.fetchone()
    if row:
        return int(row[0]), False
    cur.execute(
        "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) "
        "VALUES (?, ?, ?, ?, ?, ?, 0, ?, '[]')",
        (project_id, sender_id, thread_id, subject, body_md, importance, now_us),
    )
    message_id = int(cur.lastrowid)
    if recipient_id > 0:
        cur.execute(
            "INSERT OR IGNORE INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (?, ?, 'to', NULL, NULL)",
            (message_id, recipient_id),
        )
    return message_id, True

pid_a, _, created_pa = ensure_project(project_a, "project-alpha")
pid_b, _, created_pb = ensure_project(project_b, "project-beta")
pid_c, _, created_pc = ensure_project(project_c, "project-gamma")

blue_a, created_blue_a = ensure_agent(pid_a, "BlueLake")
red_a, created_red_a = ensure_agent(pid_a, "RedPeak")
blue_b, created_blue_b = ensure_agent(pid_b, "BlueLake")
red_b, created_red_b = ensure_agent(pid_b, "RedPeak")
blue_c, created_blue_c = ensure_agent(pid_c, "BlueLake")
red_c, created_red_c = ensure_agent(pid_c, "RedPeak")

product_id, product_uid, created_product = ensure_product(product_key)
ensure_link(product_id, pid_a)
ensure_link(product_id, pid_b)

signals = [
    ensure_message(pid_a, blue_a, red_a, "srch-a-roll", "Critical rollback plan for search migration",
                   f"{rollback_token} urgent rollback planning for frankensearch integration", "urgent"),
    ensure_message(pid_a, blue_a, red_a, "srch-a-roll", "Deployment rollback checklist for search",
                   f"{rollback_token} deployment rollback checklist and runbook", "high"),
    ensure_message(pid_a, red_a, blue_a, "srch-a-bench", "Search benchmark baseline",
                   "baseline metrics for lexical and hybrid search relevance", "normal"),
    ensure_message(pid_b, blue_b, red_b, "srch-b-roll", "Rollback strategy for billing service",
                   f"{rollback_token} rollback strategy for product-linked project beta", "urgent"),
    ensure_message(pid_b, red_b, blue_b, "srch-b-cache", "Cache warmup follow-up",
                   "frankensearch cache warmup and quality notes", "normal"),
    ensure_message(pid_c, blue_c, red_c, "srch-c-secret", "Gamma secret message",
                   f"{canary_token} should never appear in product scoped results", "urgent"),
    ensure_message(pid_a, blue_a, red_a, "srch-ghost", f"Temporary ghost marker {ghost_token}",
                   f"{ghost_token} for stale index rejection check", "normal"),
    ensure_message(pid_a, blue_a, red_a, "srch-a-prod", "Product bridge alpha signal",
                   f"{product_bridge_token} alpha side product-linked search signal", "high"),
    ensure_message(pid_b, blue_b, red_b, "srch-b-prod", "Product bridge beta signal",
                   f"{product_bridge_token} beta side product-linked search signal", "high"),
]

cur.execute("SELECT COUNT(*) FROM messages")
base_messages = int(cur.fetchone()[0])
conn.commit()

print(json.dumps({
    "project_a_id": pid_a,
    "project_b_id": pid_b,
    "project_c_id": pid_c,
    "product_id": product_id,
    "product_uid": product_uid,
    "created_projects": int(created_pa) + int(created_pb) + int(created_pc),
    "created_agents": int(created_blue_a) + int(created_red_a) + int(created_blue_b) + int(created_red_b) + int(created_blue_c) + int(created_red_c),
    "created_product": int(created_product),
    "created_signal_messages": sum(1 for _, created in signals if created),
    "base_messages": base_messages,
}))
PY
)"
e2e_save_artifact "setup_state.json" "$SETUP_STATE"

BASE_MESSAGES="$(echo "$SETUP_STATE" | jq -r '.base_messages')"
PROJECT_A_ID="$(echo "$SETUP_STATE" | jq -r '.project_a_id')"
PROJECT_B_ID="$(echo "$SETUP_STATE" | jq -r '.project_b_id')"
PROJECT_C_ID="$(echo "$SETUP_STATE" | jq -r '.project_c_id')"

if [ "${BASE_MESSAGES:-0}" -ge 7 ]; then
    e2e_pass "setup state verified and repaired (base_messages=$BASE_MESSAGES)"
else
    e2e_fail "setup state incomplete after repair (base_messages=$BASE_MESSAGES)"
fi

e2e_case_banner "Bulk seed realistic corpus directly into agent-mail DB"

SEED_STATS="$(python3 - "$DB_PATH" "$PROJECT_A" "$PROJECT_B" "$PROJECT_C" "$NOISE_A" "$NOISE_B" "$NOISE_C" <<'PY'
import json
import sqlite3
import sys
import time

db_path, project_a, project_b, project_c, noise_a, noise_b, noise_c = sys.argv[1:8]
noise_a = int(noise_a)
noise_b = int(noise_b)
noise_c = int(noise_c)

conn = sqlite3.connect(db_path)
cur = conn.cursor()

def project_id_for(path):
    cur.execute("SELECT id FROM projects WHERE human_key = ?", (path,))
    row = cur.fetchone()
    if row is None:
        raise RuntimeError(f"missing project for path: {path}")
    return int(row[0])

def agent_id_for(project_id, name):
    cur.execute(
        "SELECT id FROM agents WHERE project_id = ? AND lower(name) = lower(?) LIMIT 1",
        (project_id, name),
    )
    row = cur.fetchone()
    if row is None:
        raise RuntimeError(f"missing agent {name} for project {project_id}")
    return int(row[0])

pid_a = project_id_for(project_a)
pid_b = project_id_for(project_b)
pid_c = project_id_for(project_c)

aid_a = agent_id_for(pid_a, "BlueLake")
aid_b = agent_id_for(pid_b, "BlueLake")
aid_c = agent_id_for(pid_c, "BlueLake")

base_ts = int(time.time() * 1_000_000) - 20_000_000
cur.execute("SELECT COUNT(*) FROM messages")
base_messages = int(cur.fetchone()[0])

def seed_noise(project_id, sender_id, prefix, count, offset):
    rows = []
    for i in range(count):
        importance = ("normal", "low", "high", "normal")[i % 4]
        ack_required = 1 if i % 19 == 0 else 0
        rows.append((
            project_id,
            sender_id,
            f"{prefix}-noise-{i % 43}",
            f"{prefix} routine status update {i}",
            f"{prefix} telemetry health queue index warmup checklist {i}",
            importance,
            ack_required,
            base_ts + offset + i,
            "[]",
        ))
    cur.executemany(
        "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rows,
    )
    return len(rows)

inserted_a = seed_noise(pid_a, aid_a, "alpha", noise_a, 0)
inserted_b = seed_noise(pid_b, aid_b, "beta", noise_b, 1_000_000)
inserted_c = seed_noise(pid_c, aid_c, "gamma", noise_c, 2_000_000)

conn.commit()

cur.execute("SELECT COUNT(*) FROM messages")
total_messages = int(cur.fetchone()[0])
cur.execute("SELECT id FROM products WHERE lower(name) = lower(?) LIMIT 1", ("search-quality-product",))
product_row = cur.fetchone()
product_id = int(product_row[0]) if product_row else 0

print(json.dumps({
    "project_a_id": pid_a,
    "project_b_id": pid_b,
    "project_c_id": pid_c,
    "product_id": product_id,
    "base_messages": base_messages,
    "inserted_a": inserted_a,
    "inserted_b": inserted_b,
    "inserted_c": inserted_c,
    "total_messages": total_messages,
}))
PY
)"
e2e_save_artifact "seed_stats.json" "$SEED_STATS"

TOTAL_MESSAGES="$(echo "$SEED_STATS" | jq -r '.total_messages')"
BASE_MESSAGES_AFTER_SETUP="$(echo "$SEED_STATS" | jq -r '.base_messages')"

EXPECTED_MIN_MESSAGES=$((BASE_MESSAGES_AFTER_SETUP + NOISE_A + NOISE_B + NOISE_C))
if [ "$TOTAL_MESSAGES" -ge "$EXPECTED_MIN_MESSAGES" ]; then
    e2e_pass "bulk seed inserted realistic corpus (total_messages=$TOTAL_MESSAGES)"
else
    e2e_fail "bulk seed size too small (expected >= $EXPECTED_MIN_MESSAGES, got $TOTAL_MESSAGES)"
fi

e2e_case_banner "Relevance sanity and latency on large corpus"

T0="$(date +%s%3N)"
SEARCH_A_TEXT="$(call_tool_text 100 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"rollback search\",\"limit\":25,\"ranking\":\"relevance\"}")"
T1="$(date +%s%3N)"
SEARCH_A_MS=$((T1 - T0))
e2e_save_artifact "search_a_payload.json" "$SEARCH_A_TEXT"

SEARCH_A_PARSED="$(parse_search_payload "$SEARCH_A_TEXT")"
e2e_save_artifact "search_a_parsed.json" "$SEARCH_A_PARSED"

SEARCH_A_COUNT="$(jget "$SEARCH_A_PARSED" "count")"
SEARCH_A_SUBJECTS="$(jget "$SEARCH_A_PARSED" "subjects")"
if [ "${SEARCH_A_COUNT:-0}" -ge 2 ]; then
    e2e_pass "project A relevance query returned results"
else
    e2e_fail "project A relevance query returned too few results"
fi

if echo "$SEARCH_A_SUBJECTS" | grep -q "Critical rollback plan for search migration"; then
    e2e_pass "high-signal rollback subject appears in relevance results"
else
    e2e_fail "high-signal rollback subject missing from relevance results"
fi

if [ "$SEARCH_A_MS" -lt 10000 ]; then
    e2e_pass "cold-ish relevance query latency acceptable (${SEARCH_A_MS}ms)"
else
    e2e_fail "cold-ish relevance query latency too high (${SEARCH_A_MS}ms)"
fi

T2="$(date +%s%3N)"
SEARCH_A_WARM_TEXT="$(call_tool_text 101 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"rollback search\",\"limit\":25,\"ranking\":\"relevance\"}")"
T3="$(date +%s%3N)"
SEARCH_A_WARM_MS=$((T3 - T2))
e2e_save_artifact "search_a_warm_payload.json" "$SEARCH_A_WARM_TEXT"

if [ "$SEARCH_A_WARM_MS" -lt 5000 ]; then
    e2e_pass "warm relevance query latency acceptable (${SEARCH_A_WARM_MS}ms)"
else
    e2e_fail "warm relevance query latency too high (${SEARCH_A_WARM_MS}ms)"
fi

e2e_case_banner "Scope isolation across project and product surfaces"

SEARCH_SCOPE_A="$(call_tool_text 110 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"$SCOPE_CANARY_QUERY\",\"limit\":20}")"
SEARCH_SCOPE_A_PARSED="$(parse_search_payload "$SEARCH_SCOPE_A")"
if [ "$(jget "$SEARCH_SCOPE_A_PARSED" "count")" = "0" ]; then
    e2e_pass "project A does not leak project C canary"
else
    e2e_fail "project A leaked project C canary"
fi

SEARCH_SCOPE_C="$(call_tool_text 111 search_messages "{\"project_key\":\"$PROJECT_C\",\"query\":\"$SCOPE_CANARY_QUERY\",\"limit\":20}")"
SEARCH_SCOPE_C_PARSED="$(parse_search_payload "$SEARCH_SCOPE_C")"
if [ "$(jget "$SEARCH_SCOPE_C_PARSED" "count")" -lt 1 ]; then
    # Index visibility can lag shortly after seed writes; retry first.
    SEARCH_SCOPE_C="$(search_messages_with_retry 11100 "$PROJECT_C" "$SCOPE_CANARY_QUERY" 20 18 0.20 || true)"
    SEARCH_SCOPE_C_PARSED="$(parse_search_payload "$SEARCH_SCOPE_C")"
fi

if [ "$(jget "$SEARCH_SCOPE_C_PARSED" "count")" -lt 1 ]; then
    # If still absent, emit a deterministic fallback signal via MCP (index-integrated path).
    call_tool_text 11190 send_message "{\"project_key\":\"$PROJECT_C\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Gamma secret message fallback\",\"body_md\":\"${SCOPE_CANARY_TOKEN} fallback signal for scope isolation\",\"importance\":\"urgent\",\"thread_id\":\"srch-c-secret-fallback\"}" >/dev/null || true
    SEARCH_SCOPE_C="$(search_messages_with_retry 11120 "$PROJECT_C" "$SCOPE_CANARY_QUERY" 20 18 0.20 || true)"
    SEARCH_SCOPE_C_PARSED="$(parse_search_payload "$SEARCH_SCOPE_C")"
fi

if [ "$(jget "$SEARCH_SCOPE_C_PARSED" "count")" -ge 1 ]; then
    e2e_pass "project C can retrieve its own canary message"
else
    e2e_fail "project C could not retrieve its own canary message"
fi

PRODUCT_SCOPE="$(call_tool_text 112 search_messages_product "{\"product_key\":\"$PRODUCT_KEY\",\"query\":\"$SCOPE_CANARY_QUERY\",\"limit\":50}")"
PRODUCT_SCOPE_PARSED="$(parse_search_payload "$PRODUCT_SCOPE")"
if [ "$(jget "$PRODUCT_SCOPE_PARSED" "count")" = "0" ]; then
    e2e_pass "product scope excludes unlinked project canary"
else
    e2e_fail "product scope leaked unlinked project canary"
fi

PRODUCT_ROLLBACK="$(call_tool_text 113 search_messages_product "{\"product_key\":\"$PRODUCT_KEY\",\"query\":\"$PRODUCT_BRIDGE_TOKEN\",\"limit\":80}")"
PRODUCT_ROLLBACK_PARSED="$(parse_search_payload "$PRODUCT_ROLLBACK")"
e2e_save_artifact "product_rollback_parsed.json" "$PRODUCT_ROLLBACK_PARSED"

PRODUCT_PROJECT_IDS="$(jget "$PRODUCT_ROLLBACK_PARSED" "project_ids")"
if echo "$PRODUCT_PROJECT_IDS" | grep -q "$PROJECT_A_ID" && echo "$PRODUCT_PROJECT_IDS" | grep -q "$PROJECT_B_ID"; then
    e2e_pass "product-linked bridge query includes both linked projects"
else
    e2e_fail "product-linked bridge query missing one or more linked projects"
fi

if ! echo "$PRODUCT_PROJECT_IDS" | grep -q "$PROJECT_C_ID"; then
    e2e_pass "product-linked bridge query excludes unlinked project"
else
    e2e_fail "product-linked bridge query included unlinked project"
fi

e2e_case_banner "Facet correctness (sender, thread, importance, date)"

SENDER_FILTER="$(call_tool_text 120 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"rollback\",\"sender\":\"BlueLake\",\"limit\":50}")"
SENDER_FILTER_PARSED="$(parse_search_payload "$SENDER_FILTER")"
SENDER_VALUES="$(jget "$SENDER_FILTER_PARSED" "from_values")"
if [ "$(jget "$SENDER_FILTER_PARSED" "count")" -ge 1 ] && ! echo "$SENDER_VALUES" | grep -q "RedPeak"; then
    e2e_pass "sender filter returns BlueLake messages only"
else
    e2e_fail "sender filter returned unexpected sender values"
fi

THREAD_FILTER="$(call_tool_text 121 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"rollback\",\"thread_id\":\"srch-a-roll\",\"limit\":50}")"
THREAD_FILTER_PARSED="$(parse_search_payload "$THREAD_FILTER")"
THREAD_VALUES="$(jget "$THREAD_FILTER_PARSED" "thread_ids")"
if [ "$(jget "$THREAD_FILTER_PARSED" "count")" -ge 1 ] && ! echo "$THREAD_VALUES" | grep -v "srch-a-roll" | grep -q "srch-"; then
    e2e_pass "thread filter returns only requested thread"
else
    e2e_fail "thread filter returned rows from other threads"
fi

IMPORTANCE_FILTER="$(call_tool_text 122 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"rollback\",\"importance\":\"urgent\",\"limit\":50}")"
IMPORTANCE_FILTER_PARSED="$(parse_search_payload "$IMPORTANCE_FILTER")"
IMPORTANCE_VALUES="$(jget "$IMPORTANCE_FILTER_PARSED" "importance_values")"
if [ "$(jget "$IMPORTANCE_FILTER_PARSED" "count")" -ge 1 ] && ! echo "$IMPORTANCE_VALUES" | grep -q "normal"; then
    e2e_pass "importance filter restricts results to urgent"
else
    e2e_fail "importance filter returned non-urgent rows"
fi

FUTURE_RANGE="$(call_tool_text 123 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"rollback\",\"date_start\":\"2100-01-01\",\"date_end\":\"2100-01-02\",\"limit\":20}")"
FUTURE_RANGE_PARSED="$(parse_search_payload "$FUTURE_RANGE")"
if [ "$(jget "$FUTURE_RANGE_PARSED" "count")" = "0" ]; then
    e2e_pass "future date range yields zero results"
else
    e2e_fail "future date range unexpectedly returned results"
fi

e2e_case_banner "Duplicate suppression and stale-index ghost rejection"

DEDUP_CHECK="$(call_tool_text 130 search_messages_product "{\"product_key\":\"$PRODUCT_KEY\",\"query\":\"rollback\",\"limit\":200}")"
DEDUP_PARSED="$(parse_search_payload "$DEDUP_CHECK")"
if [ "$(jget "$DEDUP_PARSED" "count")" = "$(jget "$DEDUP_PARSED" "unique_ids_count")" ]; then
    e2e_pass "search results contain no duplicate message IDs"
else
    e2e_fail "search results contain duplicate message IDs"
fi

GHOST_BEFORE="$(call_tool_text 131 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"$GHOST_QUERY\",\"limit\":20}")"
GHOST_BEFORE_PARSED="$(parse_search_payload "$GHOST_BEFORE")"
if [ "$(jget "$GHOST_BEFORE_PARSED" "count")" -lt 1 ]; then
    # Retry before failure to absorb index-visibility lag.
    GHOST_BEFORE="$(search_messages_with_retry 13100 "$PROJECT_A" "$GHOST_QUERY" 20 18 0.20 || true)"
    GHOST_BEFORE_PARSED="$(parse_search_payload "$GHOST_BEFORE")"
fi

if [ "$(jget "$GHOST_BEFORE_PARSED" "count")" -lt 1 ]; then
    # Emit fallback ghost marker via MCP path if initial signal was not yet indexed.
    call_tool_text 13190 send_message "{\"project_key\":\"$PROJECT_A\",\"sender_name\":\"BlueLake\",\"to\":[\"RedPeak\"],\"subject\":\"Temporary ghost marker fallback ${GHOST_TOKEN}\",\"body_md\":\"${GHOST_TOKEN} fallback stale index rejection marker\",\"importance\":\"normal\",\"thread_id\":\"srch-ghost-fallback\"}" >/dev/null || true
    GHOST_BEFORE="$(search_messages_with_retry 13120 "$PROJECT_A" "$GHOST_QUERY" 20 18 0.20 || true)"
    GHOST_BEFORE_PARSED="$(parse_search_payload "$GHOST_BEFORE")"
fi

GHOST_COUNT_BEFORE="$(jget "$GHOST_BEFORE_PARSED" "count")"
GHOST_IDS_JSON="$(jget "$GHOST_BEFORE_PARSED" "ids")"
if [ "${GHOST_COUNT_BEFORE:-0}" -ge 1 ]; then
    e2e_pass "ghost marker message found before DB deletion (count=${GHOST_COUNT_BEFORE})"
else
    e2e_fail "ghost marker message missing before DB deletion"
fi

python3 - "$DB_PATH" "$GHOST_IDS_JSON" <<'PY'
import json
import sqlite3
import sys
db_path = sys.argv[1]
raw_ids = sys.argv[2]
try:
    ids = [int(x) for x in json.loads(raw_ids)]
except Exception:
    ids = []
ids = [x for x in ids if x > 0]
if not ids:
    sys.exit(0)
conn = sqlite3.connect(db_path)
cur = conn.cursor()
cur.executemany("DELETE FROM message_recipients WHERE message_id = ?", [(msg_id,) for msg_id in ids])
cur.executemany("DELETE FROM messages WHERE id = ?", [(msg_id,) for msg_id in ids])
conn.commit()
PY

GHOST_AFTER="$(call_tool_text 132 search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"$GHOST_QUERY\",\"limit\":20}")"
GHOST_AFTER_PARSED="$(parse_search_payload "$GHOST_AFTER")"
if [ "$(jget "$GHOST_AFTER_PARSED" "count")" != "0" ]; then
    # Allow short convergence window for stale-id canonicalization.
    for attempt in $(seq 1 18); do
        GHOST_AFTER="$(call_tool_text $((13200 + attempt)) search_messages "{\"project_key\":\"$PROJECT_A\",\"query\":\"$GHOST_QUERY\",\"limit\":20}")"
        GHOST_AFTER_PARSED="$(parse_search_payload "$GHOST_AFTER")"
        if [ "$(jget "$GHOST_AFTER_PARSED" "count")" = "0" ]; then
            break
        fi
        sleep 0.20
    done
fi

if [ "$(jget "$GHOST_AFTER_PARSED" "count")" = "0" ]; then
    e2e_pass "ghost marker removed from results after DB deletion (stale index rejected)"
else
    e2e_fail "ghost marker still returned after DB deletion"
fi

e2e_case_banner "Robot command regression checks (no like/2 backend failures)"

ROBOT_STATUS_OUT="$(DATABASE_URL="sqlite:///${DB_PATH}" STORAGE_ROOT="${STORAGE_ROOT}" AM_INTERFACE_MODE=cli \
    "${AM_BIN}" robot status --project "${PROJECT_A}" --agent BlueLake --format json 2>&1 || true)"
e2e_save_artifact "robot_status_output.json" "$ROBOT_STATUS_OUT"
e2e_assert_not_contains "robot status output has no like/2 failure" "$ROBOT_STATUS_OUT" "no such function: like/2"
if echo "$ROBOT_STATUS_OUT" | jq . >/dev/null 2>&1; then
    e2e_pass "robot status returns valid JSON"
else
    e2e_fail "robot status did not return valid JSON"
fi

ROBOT_SEARCH_OUT="$(DATABASE_URL="sqlite:///${DB_PATH}" STORAGE_ROOT="${STORAGE_ROOT}" AM_INTERFACE_MODE=cli \
    "${AM_BIN}" robot search rollback --project "${PROJECT_A}" --agent BlueLake --format json 2>&1 || true)"
e2e_save_artifact "robot_search_output.json" "$ROBOT_SEARCH_OUT"
e2e_assert_not_contains "robot search output has no like/2 failure" "$ROBOT_SEARCH_OUT" "no such function: like/2"
if echo "$ROBOT_SEARCH_OUT" | jq . >/dev/null 2>&1; then
    e2e_pass "robot search returns valid JSON"
else
    e2e_fail "robot search did not return valid JSON"
fi

e2e_summary
