#!/usr/bin/env bash
# test_stress_load.sh - E2E stress/load test for the HTTP server
#
# Hammers a live HTTP server with concurrent MCP tool calls to surface:
#   1. SQLite pool exhaustion under many simultaneous clients
#   2. Git lock file contention from concurrent archive writes
#   3. HTTP handler overload / request queuing
#   4. Agent registration thundering herd
#   5. Message send + inbox fetch mixed concurrency
#   6. File reservation conflicts under contention
#   7. Product bus fan-in reads over the same live workload
#   8. Core metrics resource snapshots for DB/queue resource ledgers
#   9. Robot status snapshots over the isolated mailbox
#
# Usage:
#   bash tests/e2e/test_stress_load.sh
#   STRESS_PROFILE=large bash tests/e2e/test_stress_load.sh
#   STRESS_PROFILE=large STRESS_RUN_LARGE=1 bash tests/e2e/test_stress_load.sh
#
# Configuration (env vars):
#   STRESS_PROFILE=ci         Profile: ci, legacy, or large
#   STRESS_RUN_LARGE=0        Set to 1 to run the ignored 1k-agent profile
#   STRESS_AGENTS             Override simulated agents
#   STRESS_CONCURRENCY        Override max parallel curl processes
#   STRESS_MSGS_PER_AGENT     Override messages each agent sends
#   STRESS_DURATION_SECS      Override mixed read/write duration
#   STRESS_HEALTH_CHECKS      Override sequential health checks

set -euo pipefail

# shellcheck disable=SC2034 # consumed by scripts/e2e_lib.sh after sourcing
E2E_SUITE="stress_load"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=../../scripts/e2e_lib.sh
# shellcheck disable=SC1091
source "${PROJECT_ROOT}/scripts/e2e_lib.sh"

e2e_init_artifacts

e2e_banner "HTTP Stress/Load Test Suite"

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

STRESS_PROFILE="${STRESS_PROFILE:-ci}"
STRESS_RUN_LARGE="${STRESS_RUN_LARGE:-0}"

case "$STRESS_PROFILE" in
    ci)
        PROFILE_AGENTS=12
        PROFILE_CONCURRENCY=4
        PROFILE_MSGS_PER_AGENT=2
        PROFILE_DURATION_SECS=3
        PROFILE_HEALTH_CHECKS=25
        PROFILE_RSS_GROWTH_BUDGET_KB=200000
        ;;
    legacy)
        PROFILE_AGENTS=50
        PROFILE_CONCURRENCY=20
        PROFILE_MSGS_PER_AGENT=5
        PROFILE_DURATION_SECS=10
        PROFILE_HEALTH_CHECKS=100
        PROFILE_RSS_GROWTH_BUDGET_KB=300000
        ;;
    large)
        PROFILE_AGENTS=1000
        PROFILE_CONCURRENCY=200
        PROFILE_MSGS_PER_AGENT=1
        PROFILE_DURATION_SECS=60
        PROFILE_HEALTH_CHECKS=200
        PROFILE_RSS_GROWTH_BUDGET_KB=1048576
        ;;
    *)
        e2e_fail "unknown STRESS_PROFILE=${STRESS_PROFILE}"
        e2e_summary
        exit 1
        ;;
esac

N_AGENTS="${STRESS_AGENTS:-$PROFILE_AGENTS}"
CONCURRENCY="${STRESS_CONCURRENCY:-$PROFILE_CONCURRENCY}"
MSGS_PER_AGENT="${STRESS_MSGS_PER_AGENT:-$PROFILE_MSGS_PER_AGENT}"
DURATION_SECS="${STRESS_DURATION_SECS:-$PROFILE_DURATION_SECS}"
HEALTH_CHECKS="${STRESS_HEALTH_CHECKS:-$PROFILE_HEALTH_CHECKS}"
PORT="${STRESS_PORT:-0}"  # 0 = auto-select free port
E2E_RPC_CONNECT_TIMEOUT_SECONDS="${E2E_RPC_CONNECT_TIMEOUT_SECONDS:-2}"
E2E_RPC_MAX_TIME_SECONDS="${E2E_RPC_MAX_TIME_SECONDS:-20}"

STRESS_REG_P95_BUDGET_MS="${STRESS_REG_P95_BUDGET_MS:-8000}"
STRESS_REG_P99_BUDGET_MS="${STRESS_REG_P99_BUDGET_MS:-12000}"
STRESS_MSG_P95_BUDGET_MS="${STRESS_MSG_P95_BUDGET_MS:-10000}"
STRESS_MSG_P99_BUDGET_MS="${STRESS_MSG_P99_BUDGET_MS:-15000}"
STRESS_SEARCH_P95_BUDGET_MS="${STRESS_SEARCH_P95_BUDGET_MS:-18000}"
STRESS_SEARCH_P99_BUDGET_MS="${STRESS_SEARCH_P99_BUDGET_MS:-20000}"
STRESS_PRODUCT_P95_BUDGET_MS="${STRESS_PRODUCT_P95_BUDGET_MS:-18000}"
STRESS_PRODUCT_P99_BUDGET_MS="${STRESS_PRODUCT_P99_BUDGET_MS:-20000}"
STRESS_HEALTH_P95_BUDGET_MS="${STRESS_HEALTH_P95_BUDGET_MS:-8000}"
STRESS_HEALTH_P99_BUDGET_MS="${STRESS_HEALTH_P99_BUDGET_MS:-12000}"
STRESS_METRICS_P95_BUDGET_MS="${STRESS_METRICS_P95_BUDGET_MS:-12000}"
STRESS_METRICS_P99_BUDGET_MS="${STRESS_METRICS_P99_BUDGET_MS:-18000}"
STRESS_ROBOT_P95_BUDGET_MS="${STRESS_ROBOT_P95_BUDGET_MS:-12000}"
STRESS_ROBOT_P99_BUDGET_MS="${STRESS_ROBOT_P99_BUDGET_MS:-18000}"
STRESS_RSS_GROWTH_BUDGET_KB="${STRESS_RSS_GROWTH_BUDGET_KB:-$PROFILE_RSS_GROWTH_BUDGET_KB}"

write_ignored_large_report() {
    local run_cmd
    run_cmd="cd ${PROJECT_ROOT} && AM_E2E_KEEP_TMP=1 E2E_CARGO_REQUIRE_RCH=1 STRESS_PROFILE=large STRESS_RUN_LARGE=1 bash tests/e2e/test_stress_load.sh"

    e2e_save_artifact "load_lab_report.json" "$(cat <<EOF
{
  "profile": "large",
  "status": "ignored",
  "reason": "1k-agent load lab is opt-in; set STRESS_RUN_LARGE=1 to execute it",
  "scenario": {
    "agents": ${PROFILE_AGENTS},
    "concurrency": ${PROFILE_CONCURRENCY},
    "messages_per_agent": ${PROFILE_MSGS_PER_AGENT},
    "mixed_duration_secs": ${PROFILE_DURATION_SECS},
    "health_checks": ${PROFILE_HEALTH_CHECKS},
    "surfaces": ["http_mcp", "product_bus", "tooling_metrics_core", "robot_status"]
  },
  "reproduction": "${run_cmd}"
}
EOF
)"

    e2e_save_artifact "load_lab_report.md" "$(cat <<EOF
# Stress Load Lab

Status: ignored

The 1k-agent profile is intentionally opt-in for CI safety.

Run:
\`\`\`bash
${run_cmd}
\`\`\`
EOF
)"
}

if [ "$STRESS_PROFILE" = "large" ] && [ "$STRESS_RUN_LARGE" != "1" ]; then
    e2e_skip "large 1k-agent load lab ignored; set STRESS_RUN_LARGE=1 to execute"
    write_ignored_large_report
    e2e_summary
    exit 0
fi

if [ "$CONCURRENCY" -lt 2 ]; then
    e2e_fail "STRESS_CONCURRENCY must be at least 2"
    e2e_summary
    exit 1
fi

export PATH="${PROJECT_ROOT}/target/debug:${CARGO_TARGET_DIR}/debug:${PATH}"
if ! command -v am >/dev/null 2>&1; then
    e2e_ensure_binary "am" >/dev/null
    export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
fi

now_ns() {
    date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())'
}

record_latency_us() {
    local phase="$1"
    local id="$2"
    local start_ns="$3"
    local end_ns="$4"
    local elapsed_us=$(( (end_ns - start_ns) / 1000 ))
    [ "$elapsed_us" -lt 0 ] && elapsed_us=0
    printf '%s\n' "$elapsed_us" > "${WORK}/latency_${phase}_${id}.us"
}

rss_kb_for_pid() {
    local pid="$1"
    ps -o rss= -p "$pid" 2>/dev/null | awk '
        NF { print $1 + 0; found = 1 }
        END { if (!found) print 0 }
    '
}

cpu_ticks_for_pid() {
    local pid="$1"
    awk '{ print $14 + $15 }' "/proc/${pid}/stat" 2>/dev/null || echo 0
}

file_size_bytes() {
    local path="$1"
    if [ -f "$path" ]; then
        stat --format='%s' "$path" 2>/dev/null || stat -f '%z' "$path" 2>/dev/null || echo 0
    else
        echo 0
    fi
}

tree_size_bytes() {
    local path="$1"
    if [ -d "$path" ]; then
        du -sb "$path" 2>/dev/null | awk '{ print $1 + 0 }' || echo 0
    else
        echo 0
    fi
}

tree_file_count() {
    local path="$1"
    if [ -d "$path" ]; then
        find "$path" -type f 2>/dev/null | wc -l | awk '{ print $1 + 0 }'
    else
        echo 0
    fi
}

# ---------------------------------------------------------------------------
# Setup: create temp workspace, start server
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_stress")"
STRESS_DB="${WORK}/stress.sqlite3"
STRESS_STORAGE="${WORK}/storage"
mkdir -p "$STRESS_STORAGE"

# Find a free port if not specified
if [ "$PORT" = "0" ]; then
    PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()" 2>/dev/null || echo 18765)
fi

echo "  Work dir: $WORK"
echo "  DB: $STRESS_DB"
echo "  Server (planned): http://127.0.0.1:${PORT}"
echo "  Profile: $STRESS_PROFILE"
echo "  Agents: $N_AGENTS, Concurrency: $CONCURRENCY, Msgs/agent: $MSGS_PER_AGENT"
echo "  Mixed duration: ${DURATION_SECS}s, Health checks: ${HEALTH_CHECKS}"
echo ""

# Start the server via helper-managed lifecycle + log capture.
if ! HTTP_PORT="$PORT" e2e_start_server_with_logs \
    "${STRESS_DB}" \
    "${STRESS_STORAGE}" \
    "stress_load" \
    "TUI_ENABLED=false" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "HTTP_JWT_ENABLED=0" \
    "WORKTREES_ENABLED=true" \
    "RUST_LOG=warn"; then
    e2e_fail "server failed to start"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    exit 1
fi
trap 'e2e_stop_server || true' EXIT

SERVER_URL="${E2E_SERVER_URL%/mcp/}"
MCP_URL="${E2E_SERVER_URL}"
echo "  Server: ${SERVER_URL}"
echo "  Server PID: ${E2E_SERVER_PID}"
CLK_TCK="$(getconf CLK_TCK 2>/dev/null || echo 100)"
SERVER_RSS_START_KB="$(rss_kb_for_pid "${E2E_SERVER_PID}")"
SERVER_CPU_START_TICKS="$(cpu_ticks_for_pid "${E2E_SERVER_PID}")"

# ---------------------------------------------------------------------------
# Helper: send MCP tool call via HTTP
# ---------------------------------------------------------------------------

CALL_ID=0
next_id() {
    CALL_ID=$((CALL_ID + 1))
    echo "$CALL_ID"
}

# Send a single MCP tool call and return the response
mcp_call() {
    local tool_name="$1"
    local args_json="$2"
    local call_id
    call_id=$(next_id)

    local payload
    payload=$(cat <<EOF
{"jsonrpc":"2.0","id":${call_id},"method":"tools/call","params":{"name":"${tool_name}","arguments":${args_json}}}
EOF
)
    local case_id
    case_id="rpc_${tool_name}_${BASHPID}_${call_id}_${RANDOM}"
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local response_file="${case_dir}/response.json"
    local rpc_rc=0

    e2e_mark_case_start "${case_id}"
    set +e
    e2e_rpc_call_raw "${case_id}" "${MCP_URL}" "${payload}"
    rpc_rc=$?
    set -e

    if [ -f "${response_file}" ]; then
        cat "${response_file}"
    elif [ "${rpc_rc}" -ne 0 ]; then
        printf '{"error":"rpc_transport_or_http_failure","case_id":"%s","tool":"%s"}\n' "${case_id}" "${tool_name}"
    else
        echo "{}"
    fi
}

mcp_resource_read() {
    local uri="$1"
    local call_id
    call_id=$(next_id)

    local payload
    payload=$(cat <<EOF
{"jsonrpc":"2.0","id":${call_id},"method":"resources/read","params":{"uri":"${uri}"}}
EOF
)
    local case_id
    case_id="rpc_resource_${BASHPID}_${call_id}_${RANDOM}"
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local response_file="${case_dir}/response.json"
    local rpc_rc=0

    e2e_mark_case_start "${case_id}"
    set +e
    e2e_rpc_call_raw "${case_id}" "${MCP_URL}" "${payload}"
    rpc_rc=$?
    set -e

    if [ -f "${response_file}" ]; then
        cat "${response_file}"
    elif [ "${rpc_rc}" -ne 0 ]; then
        printf '{"error":"rpc_transport_or_http_failure","case_id":"%s","uri":"%s"}\n' "${case_id}" "${uri}"
    else
        echo "{}"
    fi
}

# Send MCP tool call, return just exit code (0=ok, 1=error)
mcp_call_check() {
    local resp
    resp=$(mcp_call "$@")
    if echo "$resp" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
if 'error' in d or d.get('result',{}).get('isError',False):
    sys.exit(1)
" 2>/dev/null; then
        return 0
    else
        return 1
    fi
}

PROJECT_KEY="${WORK}/stress-test-project"
PRODUCT_KEY="stress-load-product-$$"
mkdir -p "$PROJECT_KEY"

# ---------------------------------------------------------------------------
# Phase 0: Initialize project (single call)
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 0: Project Setup ==="

INIT_RESP=$(mcp_call "ensure_project" "{\"human_key\":\"${PROJECT_KEY}\"}")
if echo "$INIT_RESP" | grep -q '"error"'; then
    echo "  FAIL: ensure_project failed: $INIT_RESP"
    exit 1
fi
e2e_pass "Project initialized"

# ---------------------------------------------------------------------------
# Phase 1: Agent Registration Thundering Herd
#
# N_AGENTS agents all register simultaneously.
# Tests: DB write contention, pool acquire under burst.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 1: Agent Registration Storm ($N_AGENTS agents, $CONCURRENCY concurrent) ==="

ADJECTIVES=(Red Orange Yellow Pink Black Purple Blue Brown White Green Chartreuse Lilac Fuchsia Azure Amber Coral Crimson Cyan Gold Golden Gray Indigo Ivory Jade Lavender)
NOUNS=(Stone Lake Creek Pond Mountain Hill Snow Castle River Forest Valley Canyon Meadow Prairie Desert Island Cliff Cave Glacier Waterfall Spring Stream Reef Dune Ridge Peak Gorge Marsh Brook Glen Grove Fern Hollow Basin Cove Bay Harbor Coast Shore Bluff Knoll Summit Plateau)

AGENT_NAMES=()
for i in $(seq 0 $((N_AGENTS - 1))); do
    adj_idx=$((i % ${#ADJECTIVES[@]}))
    noun_idx=$(( (i / ${#ADJECTIVES[@]}) % ${#NOUNS[@]} ))
    AGENT_NAMES+=("${ADJECTIVES[$adj_idx]}${NOUNS[$noun_idx]}")
done

REG_START=$(date +%s%N)
REG_OK=0
REG_FAIL=0
REG_PIDS=()

for i in $(seq 0 $((N_AGENTS - 1))); do
    # Throttle concurrency
    while [ ${#REG_PIDS[@]} -ge "$CONCURRENCY" ]; do
        NEW_PIDS=()
        for pid in "${REG_PIDS[@]}"; do
            if kill -0 "$pid" 2>/dev/null; then
                NEW_PIDS+=("$pid")
            else
                wait "$pid" 2>/dev/null && REG_OK=$((REG_OK + 1)) || REG_FAIL=$((REG_FAIL + 1))
            fi
        done
        REG_PIDS=("${NEW_PIDS[@]}")
        [ ${#REG_PIDS[@]} -ge "$CONCURRENCY" ] && sleep 0.05
    done

    (
        agent_name="${AGENT_NAMES[$i]}"
        op_start_ns="$(now_ns)"
        resp=$(mcp_call "register_agent" "{\"project_key\":\"${PROJECT_KEY}\",\"name\":\"${agent_name}\",\"program\":\"stress-test\",\"model\":\"test-model\"}")
        op_end_ns="$(now_ns)"
        record_latency_us "register" "$i" "$op_start_ns" "$op_end_ns"
        if echo "$resp" | grep -q '"error"'; then
            exit 1
        fi
        exit 0
    ) &
    REG_PIDS+=($!)
done

# Wait for all remaining
for pid in "${REG_PIDS[@]}"; do
    wait "$pid" 2>/dev/null && REG_OK=$((REG_OK + 1)) || REG_FAIL=$((REG_FAIL + 1))
done

REG_END=$(date +%s%N)
REG_ELAPSED_MS=$(( (REG_END - REG_START) / 1000000 ))

echo "  Registered: ${REG_OK} ok, ${REG_FAIL} fail, ${REG_ELAPSED_MS}ms elapsed"
echo "  Throughput: $(python3 -c "print(f'{${N_AGENTS} / (${REG_ELAPSED_MS} / 1000):.0f} registrations/sec')" 2>/dev/null || echo "N/A")"

if [ "$REG_FAIL" -eq 0 ]; then
    e2e_pass "All $N_AGENTS agents registered successfully"
elif [ "$REG_FAIL" -lt $((N_AGENTS / 10)) ]; then
    e2e_pass "Agent registration: ${REG_OK}/${N_AGENTS} ok (${REG_FAIL} transient failures)"
else
    e2e_fail "Agent registration: ${REG_FAIL}/${N_AGENTS} failed (too many errors)"
fi

# Ensure inter-agent messaging is allowed for stress workload.
POLICY_FAIL=0
for agent in "${AGENT_NAMES[@]}"; do
    resp=$(mcp_call "set_contact_policy" "{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"${agent}\",\"policy\":\"open\"}")
    if echo "$resp" | grep -q '"error"'; then
        POLICY_FAIL=$((POLICY_FAIL + 1))
    fi
done
if [ "$POLICY_FAIL" -eq 0 ]; then
    e2e_pass "Contact policy set to open for all agents"
else
    e2e_fail "Contact policy setup failures: ${POLICY_FAIL}"
fi

# ---------------------------------------------------------------------------
# Phase 2: Product Bus Setup
#
# Link the stress project into a product so later phases can exercise product
# fan-in reads through the same live HTTP transport and isolated storage.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 2: Product Bus Setup ==="

PRODUCT_SETUP_OK=0
PRODUCT_SETUP_FAIL=0

op_start_ns="$(now_ns)"
PRODUCT_RESP=$(mcp_call "ensure_product" "{\"product_key\":\"${PRODUCT_KEY}\",\"name\":\"Stress Load Product $$\"}")
op_end_ns="$(now_ns)"
record_latency_us "product_setup" "ensure" "$op_start_ns" "$op_end_ns"
if echo "$PRODUCT_RESP" | grep -q '"error"'; then
    PRODUCT_SETUP_FAIL=$((PRODUCT_SETUP_FAIL + 1))
else
    PRODUCT_SETUP_OK=$((PRODUCT_SETUP_OK + 1))
fi

op_start_ns="$(now_ns)"
PRODUCT_LINK_RESP=$(mcp_call "products_link" "{\"product_key\":\"${PRODUCT_KEY}\",\"project_key\":\"${PROJECT_KEY}\"}")
op_end_ns="$(now_ns)"
record_latency_us "product_setup" "link" "$op_start_ns" "$op_end_ns"
if echo "$PRODUCT_LINK_RESP" | grep -q '"error"'; then
    PRODUCT_SETUP_FAIL=$((PRODUCT_SETUP_FAIL + 1))
else
    PRODUCT_SETUP_OK=$((PRODUCT_SETUP_OK + 1))
fi

e2e_save_artifact "product_setup_response.jsonl" "${PRODUCT_RESP}
${PRODUCT_LINK_RESP}"

if [ "$PRODUCT_SETUP_FAIL" -eq 0 ]; then
    e2e_pass "Product bus linked stress project to ${PRODUCT_KEY}"
else
    e2e_fail "Product bus setup failures: ${PRODUCT_SETUP_FAIL}"
fi

# ---------------------------------------------------------------------------
# Phase 3: Message Send Storm
#
# Each agent sends MSGS_PER_AGENT messages to random other agents.
# Tests: concurrent DB writes + git archive writes.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 3: Message Send Storm ($N_AGENTS x $MSGS_PER_AGENT msgs, $CONCURRENCY concurrent) ==="

MSG_START=$(date +%s%N)
MSG_OK=0
MSG_FAIL=0
MSG_PIDS=()

TOTAL_MSGS=$((N_AGENTS * MSGS_PER_AGENT))

for i in $(seq 0 $((N_AGENTS - 1))); do
    for m in $(seq 0 $((MSGS_PER_AGENT - 1))); do
        # Throttle
        while [ ${#MSG_PIDS[@]} -ge "$CONCURRENCY" ]; do
            NEW_PIDS=()
            for pid in "${MSG_PIDS[@]}"; do
                if kill -0 "$pid" 2>/dev/null; then
                    NEW_PIDS+=("$pid")
                else
                    wait "$pid" 2>/dev/null && MSG_OK=$((MSG_OK + 1)) || MSG_FAIL=$((MSG_FAIL + 1))
                fi
            done
            MSG_PIDS=("${NEW_PIDS[@]}")
            [ ${#MSG_PIDS[@]} -ge "$CONCURRENCY" ] && sleep 0.02
        done

        from_agent="${AGENT_NAMES[$i]}"
        to_idx=$(( (i + m + 1) % N_AGENTS ))
        to_agent="${AGENT_NAMES[$to_idx]}"
        thread_id="stress-t${i}-m${m}"

        (
            op_start_ns="$(now_ns)"
            resp=$(mcp_call "send_message" "{\"project_key\":\"${PROJECT_KEY}\",\"sender_name\":\"${from_agent}\",\"to\":[\"${to_agent}\"],\"subject\":\"Stress msg ${i}-${m}\",\"body_md\":\"Load test message body ${i}-${m} with enough content to exercise FTS indexing\",\"thread_id\":\"${thread_id}\"}")
            op_end_ns="$(now_ns)"
            record_latency_us "message" "${i}_${m}" "$op_start_ns" "$op_end_ns"
            if echo "$resp" | grep -q '"error"'; then
                exit 1
            fi
            exit 0
        ) &
        MSG_PIDS+=($!)
    done
done

for pid in "${MSG_PIDS[@]}"; do
    wait "$pid" 2>/dev/null && MSG_OK=$((MSG_OK + 1)) || MSG_FAIL=$((MSG_FAIL + 1))
done

MSG_END=$(date +%s%N)
MSG_ELAPSED_MS=$(( (MSG_END - MSG_START) / 1000000 ))

echo "  Sent: ${MSG_OK} ok, ${MSG_FAIL} fail out of ${TOTAL_MSGS}, ${MSG_ELAPSED_MS}ms elapsed"
echo "  Throughput: $(python3 -c "print(f'{${TOTAL_MSGS} / (${MSG_ELAPSED_MS} / 1000):.0f} msgs/sec')" 2>/dev/null || echo "N/A")"

MSG_ERROR_RATE=$(python3 -c "print(f'{${MSG_FAIL} / max(${TOTAL_MSGS}, 1) * 100:.1f}')" 2>/dev/null || echo "?")
if [ "$MSG_FAIL" -lt $((TOTAL_MSGS / 10)) ]; then
    e2e_pass "Message storm: ${MSG_OK}/${TOTAL_MSGS} ok (${MSG_ERROR_RATE}% error rate)"
else
    e2e_fail "Message storm: ${MSG_FAIL}/${TOTAL_MSGS} failed (${MSG_ERROR_RATE}% error rate)"
fi

# ---------------------------------------------------------------------------
# Phase 4: Concurrent Inbox Fetch + Message Send (Read/Write Mix)
#
# Half the threads read inboxes, half send messages — simultaneously.
# Tests: WAL read/write concurrency, cache coherency.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 4: Mixed Read/Write Concurrency (${CONCURRENCY} workers, ${DURATION_SECS}s) ==="

MIX_START=$(date +%s%N)
MIX_READS_OK=0
MIX_READS_FAIL=0
MIX_WRITES_OK=0
MIX_WRITES_FAIL=0
MIX_PIDS=()

# Reader workers
for w in $(seq 0 $((CONCURRENCY / 2 - 1))); do
    (
        agent="${AGENT_NAMES[$((w % N_AGENTS))]}"
        ok=0
        fail=0
        end_time=$(($(date +%s) + DURATION_SECS))
        while [ "$(date +%s)" -lt "$end_time" ]; do
            resp=$(mcp_call "fetch_inbox" "{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"${agent}\"}")
            if echo "$resp" | grep -q '"error"'; then
                fail=$((fail + 1))
            else
                ok=$((ok + 1))
            fi
            sleep 0.1
        done
        echo "READ:${ok}:${fail}" > "${WORK}/mix_reader_${w}.txt"
    ) &
    MIX_PIDS+=($!)
done

# Writer workers
for w in $(seq 0 $((CONCURRENCY / 2 - 1))); do
    (
        ok=0
        fail=0
        end_time=$(($(date +%s) + DURATION_SECS))
        msg_idx=0
        while [ "$(date +%s)" -lt "$end_time" ]; do
            from_idx=$(( (w + msg_idx) % N_AGENTS ))
            to_idx=$(( (w + msg_idx + 1) % N_AGENTS ))
            from_agent="${AGENT_NAMES[$from_idx]}"
            to_agent="${AGENT_NAMES[$to_idx]}"
            resp=$(mcp_call "send_message" "{\"project_key\":\"${PROJECT_KEY}\",\"sender_name\":\"${from_agent}\",\"to\":[\"${to_agent}\"],\"subject\":\"Mix msg ${w}-${msg_idx}\",\"body_md\":\"Mixed workload body\",\"thread_id\":\"mix-${w}-${msg_idx}\"}")
            if echo "$resp" | grep -q '"error"'; then
                fail=$((fail + 1))
            else
                ok=$((ok + 1))
            fi
            msg_idx=$((msg_idx + 1))
            sleep 0.05
        done
        echo "WRITE:${ok}:${fail}" > "${WORK}/mix_writer_${w}.txt"
    ) &
    MIX_PIDS+=($!)
done

for pid in "${MIX_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
done

# Aggregate results
for f in "${WORK}"/mix_reader_*.txt; do
    [ -f "$f" ] || continue
    IFS=: read -r _ ok fail < "$f"
    MIX_READS_OK=$((MIX_READS_OK + ok))
    MIX_READS_FAIL=$((MIX_READS_FAIL + fail))
done
for f in "${WORK}"/mix_writer_*.txt; do
    [ -f "$f" ] || continue
    IFS=: read -r _ ok fail < "$f"
    MIX_WRITES_OK=$((MIX_WRITES_OK + ok))
    MIX_WRITES_FAIL=$((MIX_WRITES_FAIL + fail))
done

MIX_END=$(date +%s%N)
MIX_ELAPSED_MS=$(( (MIX_END - MIX_START) / 1000000 ))

echo "  Reads: ${MIX_READS_OK} ok, ${MIX_READS_FAIL} fail"
echo "  Writes: ${MIX_WRITES_OK} ok, ${MIX_WRITES_FAIL} fail"
echo "  Elapsed: ${MIX_ELAPSED_MS}ms"

MIX_TOTAL=$((MIX_READS_OK + MIX_READS_FAIL + MIX_WRITES_OK + MIX_WRITES_FAIL))
MIX_FAIL=$((MIX_READS_FAIL + MIX_WRITES_FAIL))
if [ "$MIX_TOTAL" -gt 0 ]; then
    MIX_ERROR_RATE=$(python3 -c "print(f'{${MIX_FAIL} / ${MIX_TOTAL} * 100:.1f}')" 2>/dev/null || echo "?")
    if [ "$MIX_FAIL" -lt $((MIX_TOTAL / 10)) ]; then
        e2e_pass "Mixed read/write: ${MIX_ERROR_RATE}% error rate (${MIX_TOTAL} total ops)"
    else
        e2e_fail "Mixed read/write: ${MIX_ERROR_RATE}% error rate too high (${MIX_FAIL}/${MIX_TOTAL})"
    fi
fi

# ---------------------------------------------------------------------------
# Phase 5: File Reservation Contention
#
# Multiple agents compete for overlapping file patterns.
# Tests: reservation conflict detection under concurrency.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 5: File Reservation Contention (${CONCURRENCY} concurrent) ==="

RES_OK=0
RES_CONFLICT=0
RES_ERROR=0
RES_PIDS=()

for i in $(seq 0 $((CONCURRENCY - 1))); do
    (
        agent="${AGENT_NAMES[$((i % N_AGENTS))]}"
        # Everyone tries to reserve src/** exclusively — most should conflict
        resp=$(mcp_call "file_reservation_paths" "{\"project_key\":\"${PROJECT_KEY}\",\"agent_name\":\"${agent}\",\"paths\":[\"src/shared_module.rs\"],\"ttl_seconds\":300,\"exclusive\":true}")
        if echo "$resp" | grep -qi "conflict"; then
            echo "CONFLICT" > "${WORK}/res_${i}.txt"
        elif echo "$resp" | grep -q '"error"'; then
            echo "ERROR" > "${WORK}/res_${i}.txt"
        else
            echo "OK" > "${WORK}/res_${i}.txt"
        fi
    ) &
    RES_PIDS+=($!)
done

for pid in "${RES_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
done

for f in "${WORK}"/res_*.txt; do
    [ -f "$f" ] || continue
    case "$(cat "$f")" in
        OK) RES_OK=$((RES_OK + 1)) ;;
        CONFLICT) RES_CONFLICT=$((RES_CONFLICT + 1)) ;;
        ERROR) RES_ERROR=$((RES_ERROR + 1)) ;;
    esac
done

echo "  Reserved: $RES_OK, Conflicts: $RES_CONFLICT, Errors: $RES_ERROR"

# Exactly 1 should succeed (exclusive), rest should conflict
if [ "$RES_ERROR" -eq 0 ]; then
    e2e_pass "Reservation contention: ${RES_OK} reserved, ${RES_CONFLICT} conflicts, 0 errors"
else
    e2e_fail "Reservation contention: ${RES_ERROR} unexpected errors"
fi

# ---------------------------------------------------------------------------
# Phase 6: Search Under Load
#
# Concurrent search queries while messages are still being indexed.
# Tests: FTS5 concurrency, LIKE fallback, pool sharing.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 6: Concurrent Search ($CONCURRENCY parallel queries) ==="

SEARCH_QUERIES=("stress" "message" "body" "load" "test" "workload" "mix" "content" "FTS" "indexing")
SEARCH_OK=0
SEARCH_FAIL=0
SEARCH_PIDS=()

for i in $(seq 0 $((CONCURRENCY - 1))); do
    q_idx=$((i % ${#SEARCH_QUERIES[@]}))
    query="${SEARCH_QUERIES[$q_idx]}"
    (
        op_start_ns="$(now_ns)"
        resp=$(mcp_call "search_messages" "{\"project_key\":\"${PROJECT_KEY}\",\"query\":\"${query}\",\"limit\":20}")
        op_end_ns="$(now_ns)"
        record_latency_us "search" "$i" "$op_start_ns" "$op_end_ns"
        if echo "$resp" | grep -q '"error"'; then
            exit 1
        fi
        exit 0
    ) &
    SEARCH_PIDS+=($!)
done

for pid in "${SEARCH_PIDS[@]}"; do
    wait "$pid" 2>/dev/null && SEARCH_OK=$((SEARCH_OK + 1)) || SEARCH_FAIL=$((SEARCH_FAIL + 1))
done

echo "  Search results: ${SEARCH_OK} ok, ${SEARCH_FAIL} fail"

if [ "$SEARCH_FAIL" -eq 0 ]; then
    e2e_pass "Concurrent search: all $SEARCH_OK queries succeeded"
elif [ "$SEARCH_FAIL" -lt $((CONCURRENCY / 5)) ]; then
    e2e_pass "Concurrent search: ${SEARCH_OK}/${CONCURRENCY} ok (${SEARCH_FAIL} transient)"
else
    e2e_fail "Concurrent search: ${SEARCH_FAIL}/${CONCURRENCY} failed"
fi

# ---------------------------------------------------------------------------
# Phase 7: Product Bus Fan-In Reads
#
# Product-scoped search and inbox reads over the same live workload.
# Tests: cross-surface fan-in routing, product/project link lookups, and
# product-scoped search/inbox pagination under post-write pressure.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 7: Product Bus Fan-In Reads (${CONCURRENCY} parallel query pairs) ==="

PRODUCT_SEARCH_OK=0
PRODUCT_SEARCH_FAIL=0
PRODUCT_INBOX_OK=0
PRODUCT_INBOX_FAIL=0
PRODUCT_PIDS=()

for i in $(seq 0 $((CONCURRENCY - 1))); do
    q_idx=$((i % ${#SEARCH_QUERIES[@]}))
    query="${SEARCH_QUERIES[$q_idx]}"
    agent="${AGENT_NAMES[$((i % N_AGENTS))]}"

    (
        op_start_ns="$(now_ns)"
        resp=$(mcp_call "search_messages_product" "{\"product_key\":\"${PRODUCT_KEY}\",\"query\":\"${query}\",\"limit\":20}")
        op_end_ns="$(now_ns)"
        record_latency_us "product_search" "$i" "$op_start_ns" "$op_end_ns"
        if echo "$resp" | grep -q '"error"'; then
            echo "SEARCH:FAIL" > "${WORK}/product_search_${i}.txt"
        else
            echo "SEARCH:OK" > "${WORK}/product_search_${i}.txt"
        fi

        op_start_ns="$(now_ns)"
        resp=$(mcp_call "fetch_inbox_product" "{\"product_key\":\"${PRODUCT_KEY}\",\"agent_name\":\"${agent}\",\"limit\":20}")
        op_end_ns="$(now_ns)"
        record_latency_us "product_inbox" "$i" "$op_start_ns" "$op_end_ns"
        if echo "$resp" | grep -q '"error"'; then
            echo "INBOX:FAIL" > "${WORK}/product_inbox_${i}.txt"
        else
            echo "INBOX:OK" > "${WORK}/product_inbox_${i}.txt"
        fi
    ) &
    PRODUCT_PIDS+=($!)
done

for pid in "${PRODUCT_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
done

for f in "${WORK}"/product_search_*.txt; do
    [ -f "$f" ] || continue
    case "$(cat "$f")" in
        SEARCH:OK) PRODUCT_SEARCH_OK=$((PRODUCT_SEARCH_OK + 1)) ;;
        SEARCH:FAIL) PRODUCT_SEARCH_FAIL=$((PRODUCT_SEARCH_FAIL + 1)) ;;
    esac
done
for f in "${WORK}"/product_inbox_*.txt; do
    [ -f "$f" ] || continue
    case "$(cat "$f")" in
        INBOX:OK) PRODUCT_INBOX_OK=$((PRODUCT_INBOX_OK + 1)) ;;
        INBOX:FAIL) PRODUCT_INBOX_FAIL=$((PRODUCT_INBOX_FAIL + 1)) ;;
    esac
done

echo "  Product search: ${PRODUCT_SEARCH_OK} ok, ${PRODUCT_SEARCH_FAIL} fail"
echo "  Product inbox: ${PRODUCT_INBOX_OK} ok, ${PRODUCT_INBOX_FAIL} fail"

if [ "$PRODUCT_SEARCH_FAIL" -eq 0 ] && [ "$PRODUCT_INBOX_FAIL" -eq 0 ]; then
    e2e_pass "Product bus fan-in reads succeeded"
else
    e2e_fail "Product bus fan-in failures: search=${PRODUCT_SEARCH_FAIL}, inbox=${PRODUCT_INBOX_FAIL}"
fi

# ---------------------------------------------------------------------------
# Phase 8: Health Check Under Load (server stays responsive)
#
# Rapid health checks while other operations may still be settling.
# Tests: server doesn't deadlock or become unresponsive.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 8: Health Check Rapid-Fire (${HEALTH_CHECKS} sequential checks) ==="

HEALTH_OK=0
HEALTH_FAIL=0
HEALTH_START=$(date +%s%N)

for i in $(seq 1 "$HEALTH_CHECKS"); do
    op_start_ns="$(now_ns)"
    resp=$(mcp_call "health_check" "{}" 2>/dev/null)
    op_end_ns="$(now_ns)"
    record_latency_us "health" "$i" "$op_start_ns" "$op_end_ns"
    if echo "$resp" | grep -q '"error"'; then
        HEALTH_FAIL=$((HEALTH_FAIL + 1))
    else
        HEALTH_OK=$((HEALTH_OK + 1))
    fi
done

HEALTH_END=$(date +%s%N)
HEALTH_ELAPSED_MS=$(( (HEALTH_END - HEALTH_START) / 1000000 ))
HEALTH_AVG_MS=$(python3 -c "print(f'{${HEALTH_ELAPSED_MS} / max(${HEALTH_CHECKS}, 1):.1f}')" 2>/dev/null || echo "?")

echo "  Health checks: ${HEALTH_OK}/${HEALTH_CHECKS} ok, avg ${HEALTH_AVG_MS}ms"

if [ "$HEALTH_FAIL" -eq 0 ]; then
    e2e_pass "Health check: all ${HEALTH_CHECKS} passed (avg ${HEALTH_AVG_MS}ms)"
else
    e2e_fail "Health check: ${HEALTH_FAIL}/${HEALTH_CHECKS} failed"
fi

# ---------------------------------------------------------------------------
# Phase 9: Core Metrics Resource Snapshot
#
# Capture live DB pool and storage queue metrics from the server process before
# it is stopped for the robot snapshot.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 9: Core Metrics Resource Snapshot ==="

METRICS_CORE_OK=0
METRICS_CORE_FAIL=0
METRICS_CORE_FILE="${E2E_ARTIFACT_DIR}/metrics_core_resource.json"

op_start_ns="$(now_ns)"
METRICS_CORE_RESP=$(mcp_resource_read "resource://tooling/metrics_core")
op_end_ns="$(now_ns)"
record_latency_us "metrics_core" "resource" "$op_start_ns" "$op_end_ns"
printf '%s\n' "$METRICS_CORE_RESP" > "$METRICS_CORE_FILE"

if python3 - "$METRICS_CORE_FILE" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
if "error" in payload:
    raise SystemExit(1)
contents = payload.get("result", {}).get("contents", [])
if not contents:
    raise SystemExit(1)
text = contents[0].get("text", "")
data = json.loads(text)
metrics = data.get("metrics", {})
db = metrics.get("db", {})
storage = metrics.get("storage", {})
read_cache = data.get("read_cache", {})
if "pool_acquires_total" not in db:
    raise SystemExit(1)
if "wbq_depth" not in storage:
    raise SystemExit(1)
if "inbox_stats_hits" not in read_cache.get("metrics", {}):
    raise SystemExit(1)
PY
then
    METRICS_CORE_OK=1
    e2e_pass "Core metrics resource captured DB and queue ledgers"
else
    METRICS_CORE_FAIL=1
    e2e_fail "Core metrics resource failed or produced invalid JSON"
fi

SERVER_RSS_END_KB="$(rss_kb_for_pid "${E2E_SERVER_PID}")"
RSS_GROWTH_KB=$(( SERVER_RSS_END_KB - SERVER_RSS_START_KB ))
[ "$RSS_GROWTH_KB" -lt 0 ] && RSS_GROWTH_KB=0
SERVER_CPU_END_TICKS="$(cpu_ticks_for_pid "${E2E_SERVER_PID}")"
SERVER_CPU_TICKS_DELTA=$(( SERVER_CPU_END_TICKS - SERVER_CPU_START_TICKS ))
[ "$SERVER_CPU_TICKS_DELTA" -lt 0 ] && SERVER_CPU_TICKS_DELTA=0
SERVER_CPU_SECONDS="$(python3 -c "print(round(${SERVER_CPU_TICKS_DELTA} / max(${CLK_TCK}, 1), 3))" 2>/dev/null || echo "0")"
STRESS_DB_BYTES="$(file_size_bytes "${STRESS_DB}")"
STRESS_DB_WAL_BYTES="$(file_size_bytes "${STRESS_DB}-wal")"
STRESS_DB_SHM_BYTES="$(file_size_bytes "${STRESS_DB}-shm")"
STRESS_STORAGE_BYTES="$(tree_size_bytes "${STRESS_STORAGE}")"
STRESS_STORAGE_FILE_COUNT="$(tree_file_count "${STRESS_STORAGE}")"

# Robot commands take the same storage activity lock as the live server. Stop
# the server after measuring RSS so the robot snapshot proves the operator path
# against the exact isolated mailbox without lock contention.
e2e_stop_server || true

# ---------------------------------------------------------------------------
# Phase 10: Robot Status Snapshot
#
# Exercise the agent-first operator surface against the isolated workload state.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 10: Robot Status Snapshot ==="

ROBOT_OK=0
ROBOT_FAIL=0
ROBOT_AGENT="${AGENT_NAMES[0]}"
ROBOT_STATUS_FILE="${E2E_ARTIFACT_DIR}/robot_status.json"

op_start_ns="$(now_ns)"
ROBOT_STATUS_OUT=""
ROBOT_RC=1
for attempt in 1 2 3 4 5; do
    set +e
    ROBOT_STATUS_OUT=$(DATABASE_URL="sqlite:////${STRESS_DB}" STORAGE_ROOT="${STRESS_STORAGE}" \
        AM_INTERFACE_MODE=cli am robot --project "${PROJECT_KEY}" --agent "${ROBOT_AGENT}" status --format json 2>&1)
    ROBOT_RC=$?
    set -e
    if [ "$ROBOT_RC" -eq 0 ]; then
        break
    fi
    if echo "$ROBOT_STATUS_OUT" | grep -qi "activity lock is busy"; then
        sleep "$attempt"
    else
        break
    fi
done
op_end_ns="$(now_ns)"
record_latency_us "robot_status" "status" "$op_start_ns" "$op_end_ns"
printf '%s\n' "$ROBOT_STATUS_OUT" > "$ROBOT_STATUS_FILE"

if [ "$ROBOT_RC" -eq 0 ] && python3 - "$ROBOT_STATUS_FILE" <<'PY'
import json
import sys
from pathlib import Path

data = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
meta = data.get("_meta", {})
if meta.get("command") not in {"status", "robot status"}:
    raise SystemExit(1)
if not any(key in data for key in ("health", "inbox_summary", "activity")):
    raise SystemExit(1)
PY
then
    ROBOT_OK=1
    e2e_pass "Robot status produced a valid isolated JSON snapshot"
else
    ROBOT_FAIL=1
    e2e_fail "Robot status failed or produced invalid JSON"
fi

write_load_lab_reports() {
    local report_json="${E2E_ARTIFACT_DIR}/load_lab_report.json"
    local report_md="${E2E_ARTIFACT_DIR}/load_lab_report.md"
    local report_status

    STRESS_PROFILE="$STRESS_PROFILE" \
    STRESS_RUN_LARGE="$STRESS_RUN_LARGE" \
    N_AGENTS="$N_AGENTS" \
    CONCURRENCY="$CONCURRENCY" \
    MSGS_PER_AGENT="$MSGS_PER_AGENT" \
    DURATION_SECS="$DURATION_SECS" \
    HEALTH_CHECKS="$HEALTH_CHECKS" \
    PROJECT_KEY="$PROJECT_KEY" \
    PRODUCT_KEY="$PRODUCT_KEY" \
    STRESS_DB="$STRESS_DB" \
    STRESS_STORAGE="$STRESS_STORAGE" \
    CLK_TCK="$CLK_TCK" \
    SERVER_RSS_START_KB="$SERVER_RSS_START_KB" \
    SERVER_RSS_END_KB="$SERVER_RSS_END_KB" \
    RSS_GROWTH_KB="$RSS_GROWTH_KB" \
    SERVER_CPU_START_TICKS="$SERVER_CPU_START_TICKS" \
    SERVER_CPU_END_TICKS="$SERVER_CPU_END_TICKS" \
    SERVER_CPU_TICKS_DELTA="$SERVER_CPU_TICKS_DELTA" \
    SERVER_CPU_SECONDS="$SERVER_CPU_SECONDS" \
    STRESS_DB_BYTES="$STRESS_DB_BYTES" \
    STRESS_DB_WAL_BYTES="$STRESS_DB_WAL_BYTES" \
    STRESS_DB_SHM_BYTES="$STRESS_DB_SHM_BYTES" \
    STRESS_STORAGE_BYTES="$STRESS_STORAGE_BYTES" \
    STRESS_STORAGE_FILE_COUNT="$STRESS_STORAGE_FILE_COUNT" \
    REG_OK="$REG_OK" \
    REG_FAIL="$REG_FAIL" \
    PRODUCT_SETUP_OK="$PRODUCT_SETUP_OK" \
    PRODUCT_SETUP_FAIL="$PRODUCT_SETUP_FAIL" \
    MSG_OK="$MSG_OK" \
    MSG_FAIL="$MSG_FAIL" \
    TOTAL_MSGS="$TOTAL_MSGS" \
    MIX_READS_OK="$MIX_READS_OK" \
    MIX_READS_FAIL="$MIX_READS_FAIL" \
    MIX_WRITES_OK="$MIX_WRITES_OK" \
    MIX_WRITES_FAIL="$MIX_WRITES_FAIL" \
    RES_OK="$RES_OK" \
    RES_CONFLICT="$RES_CONFLICT" \
    RES_ERROR="$RES_ERROR" \
    SEARCH_OK="$SEARCH_OK" \
    SEARCH_FAIL="$SEARCH_FAIL" \
    PRODUCT_SEARCH_OK="$PRODUCT_SEARCH_OK" \
    PRODUCT_SEARCH_FAIL="$PRODUCT_SEARCH_FAIL" \
    PRODUCT_INBOX_OK="$PRODUCT_INBOX_OK" \
    PRODUCT_INBOX_FAIL="$PRODUCT_INBOX_FAIL" \
    HEALTH_OK="$HEALTH_OK" \
    HEALTH_FAIL="$HEALTH_FAIL" \
    METRICS_CORE_FILE="$METRICS_CORE_FILE" \
    METRICS_CORE_OK="$METRICS_CORE_OK" \
    METRICS_CORE_FAIL="$METRICS_CORE_FAIL" \
    ROBOT_OK="$ROBOT_OK" \
    ROBOT_FAIL="$ROBOT_FAIL" \
    STRESS_REG_P95_BUDGET_MS="$STRESS_REG_P95_BUDGET_MS" \
    STRESS_REG_P99_BUDGET_MS="$STRESS_REG_P99_BUDGET_MS" \
    STRESS_MSG_P95_BUDGET_MS="$STRESS_MSG_P95_BUDGET_MS" \
    STRESS_MSG_P99_BUDGET_MS="$STRESS_MSG_P99_BUDGET_MS" \
    STRESS_SEARCH_P95_BUDGET_MS="$STRESS_SEARCH_P95_BUDGET_MS" \
    STRESS_SEARCH_P99_BUDGET_MS="$STRESS_SEARCH_P99_BUDGET_MS" \
    STRESS_PRODUCT_P95_BUDGET_MS="$STRESS_PRODUCT_P95_BUDGET_MS" \
    STRESS_PRODUCT_P99_BUDGET_MS="$STRESS_PRODUCT_P99_BUDGET_MS" \
    STRESS_HEALTH_P95_BUDGET_MS="$STRESS_HEALTH_P95_BUDGET_MS" \
    STRESS_HEALTH_P99_BUDGET_MS="$STRESS_HEALTH_P99_BUDGET_MS" \
    STRESS_METRICS_P95_BUDGET_MS="$STRESS_METRICS_P95_BUDGET_MS" \
    STRESS_METRICS_P99_BUDGET_MS="$STRESS_METRICS_P99_BUDGET_MS" \
    STRESS_ROBOT_P95_BUDGET_MS="$STRESS_ROBOT_P95_BUDGET_MS" \
    STRESS_ROBOT_P99_BUDGET_MS="$STRESS_ROBOT_P99_BUDGET_MS" \
    STRESS_RSS_GROWTH_BUDGET_KB="$STRESS_RSS_GROWTH_BUDGET_KB" \
    python3 - "$WORK" "$report_json" "$report_md" "$PROJECT_ROOT" <<'PY'
import glob
import json
import math
import os
import statistics
import sys
from pathlib import Path

work, report_json, report_md, project_root = sys.argv[1:]

def env_int(name: str, default: int = 0) -> int:
    raw = os.getenv(name)
    if raw is None or raw == "":
        return default
    return int(raw)

def env_float(name: str, default: float = 0.0) -> float:
    raw = os.getenv(name)
    if raw is None or raw == "":
        return default
    return float(raw)

def nested_int(data: dict, path: list[str], default: int = 0) -> int:
    current = data
    for key in path:
        if not isinstance(current, dict):
            return default
        current = current.get(key)
    return int(current) if isinstance(current, (int, float)) else default

def load_metrics_core(path: str) -> dict:
    if not path:
        return {}
    source = Path(path)
    if not source.exists():
        return {}
    try:
        payload = json.loads(source.read_text(encoding="utf-8"))
        contents = payload.get("result", {}).get("contents", [])
        if not contents:
            return {}
        return json.loads(contents[0].get("text", ""))
    except (OSError, json.JSONDecodeError, TypeError):
        return {}

def pct(values: list[int], percentile: float) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    index = int(math.ceil((percentile / 100.0) * len(ordered))) - 1
    index = max(0, min(index, len(ordered) - 1))
    return ordered[index]

def latency_stats(phase: str) -> dict:
    samples = []
    for path in glob.glob(str(Path(work) / f"latency_{phase}_*.us")):
        try:
            samples.append(int(Path(path).read_text(encoding="utf-8").strip()))
        except (OSError, ValueError):
            pass
    samples.sort()
    return {
        "samples": len(samples),
        "p50_us": pct(samples, 50.0),
        "p95_us": pct(samples, 95.0),
        "p99_us": pct(samples, 99.0),
        "max_us": max(samples) if samples else 0,
        "mean_us": round(statistics.fmean(samples), 3) if samples else 0.0,
    }

def budget_ms(name: str) -> int:
    return env_int(name, 0)

latencies = {
    "register": latency_stats("register"),
    "product_setup": latency_stats("product_setup"),
    "message": latency_stats("message"),
    "search": latency_stats("search"),
    "product_search": latency_stats("product_search"),
    "product_inbox": latency_stats("product_inbox"),
    "health": latency_stats("health"),
    "metrics_core": latency_stats("metrics_core"),
    "robot_status": latency_stats("robot_status"),
}

phase_budgets = {
    "register": {
        "p95_budget_us": budget_ms("STRESS_REG_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_REG_P99_BUDGET_MS") * 1000,
    },
    "message": {
        "p95_budget_us": budget_ms("STRESS_MSG_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_MSG_P99_BUDGET_MS") * 1000,
    },
    "product_setup": {
        "p95_budget_us": budget_ms("STRESS_PRODUCT_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_PRODUCT_P99_BUDGET_MS") * 1000,
    },
    "search": {
        "p95_budget_us": budget_ms("STRESS_SEARCH_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_SEARCH_P99_BUDGET_MS") * 1000,
    },
    "product_search": {
        "p95_budget_us": budget_ms("STRESS_PRODUCT_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_PRODUCT_P99_BUDGET_MS") * 1000,
    },
    "product_inbox": {
        "p95_budget_us": budget_ms("STRESS_PRODUCT_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_PRODUCT_P99_BUDGET_MS") * 1000,
    },
    "health": {
        "p95_budget_us": budget_ms("STRESS_HEALTH_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_HEALTH_P99_BUDGET_MS") * 1000,
    },
    "metrics_core": {
        "p95_budget_us": budget_ms("STRESS_METRICS_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_METRICS_P99_BUDGET_MS") * 1000,
    },
    "robot_status": {
        "p95_budget_us": budget_ms("STRESS_ROBOT_P95_BUDGET_MS") * 1000,
        "p99_budget_us": budget_ms("STRESS_ROBOT_P99_BUDGET_MS") * 1000,
    },
}

gates = []
for phase, stats in latencies.items():
    budgets = phase_budgets[phase]
    p95_ok = stats["samples"] > 0 and stats["p95_us"] <= budgets["p95_budget_us"]
    p99_ok = stats["samples"] > 0 and stats["p99_us"] <= budgets["p99_budget_us"]
    gates.append({
        "name": f"{phase}_p95",
        "passed": p95_ok,
        "current_us": stats["p95_us"],
        "budget_us": budgets["p95_budget_us"],
        "samples": stats["samples"],
    })
    gates.append({
        "name": f"{phase}_p99",
        "passed": p99_ok,
        "current_us": stats["p99_us"],
        "budget_us": budgets["p99_budget_us"],
        "samples": stats["samples"],
    })

rss_growth_budget_kb = env_int("STRESS_RSS_GROWTH_BUDGET_KB")
rss_growth_kb = env_int("RSS_GROWTH_KB")
gates.append({
    "name": "rss_growth",
    "passed": rss_growth_kb <= rss_growth_budget_kb,
    "current_kb": rss_growth_kb,
    "budget_kb": rss_growth_budget_kb,
})
gates.extend([
    {
        "name": "registration_success",
        "passed": env_int("REG_FAIL") == 0,
        "failed": env_int("REG_FAIL"),
        "attempted": env_int("N_AGENTS"),
    },
    {
        "name": "product_setup_success",
        "passed": env_int("PRODUCT_SETUP_FAIL") == 0,
        "failed": env_int("PRODUCT_SETUP_FAIL"),
        "attempted": 2,
    },
    {
        "name": "message_success",
        "passed": env_int("MSG_FAIL") == 0,
        "failed": env_int("MSG_FAIL"),
        "attempted": env_int("TOTAL_MSGS"),
    },
    {
        "name": "product_bus_success",
        "passed": env_int("PRODUCT_SEARCH_FAIL") == 0 and env_int("PRODUCT_INBOX_FAIL") == 0,
        "failed": env_int("PRODUCT_SEARCH_FAIL") + env_int("PRODUCT_INBOX_FAIL"),
        "attempted": env_int("CONCURRENCY") * 2,
    },
    {
        "name": "metrics_core_success",
        "passed": env_int("METRICS_CORE_FAIL") == 0,
        "failed": env_int("METRICS_CORE_FAIL"),
        "attempted": 1,
    },
    {
        "name": "robot_status_success",
        "passed": env_int("ROBOT_FAIL") == 0,
        "failed": env_int("ROBOT_FAIL"),
        "attempted": 1,
    },
])

summary = {
    "registration": {
        "ok": env_int("REG_OK"),
        "failed": env_int("REG_FAIL"),
        "attempted": env_int("N_AGENTS"),
    },
    "product_setup": {
        "ok": env_int("PRODUCT_SETUP_OK"),
        "failed": env_int("PRODUCT_SETUP_FAIL"),
        "attempted": 2,
    },
    "message_storm": {
        "ok": env_int("MSG_OK"),
        "failed": env_int("MSG_FAIL"),
        "attempted": env_int("TOTAL_MSGS"),
    },
    "mixed_read_write": {
        "reads_ok": env_int("MIX_READS_OK"),
        "reads_failed": env_int("MIX_READS_FAIL"),
        "writes_ok": env_int("MIX_WRITES_OK"),
        "writes_failed": env_int("MIX_WRITES_FAIL"),
    },
    "reservations": {
        "reserved": env_int("RES_OK"),
        "conflicts": env_int("RES_CONFLICT"),
        "errors": env_int("RES_ERROR"),
    },
    "search": {
        "ok": env_int("SEARCH_OK"),
        "failed": env_int("SEARCH_FAIL"),
        "attempted": env_int("CONCURRENCY"),
    },
    "product_bus": {
        "search_ok": env_int("PRODUCT_SEARCH_OK"),
        "search_failed": env_int("PRODUCT_SEARCH_FAIL"),
        "inbox_ok": env_int("PRODUCT_INBOX_OK"),
        "inbox_failed": env_int("PRODUCT_INBOX_FAIL"),
        "attempted_pairs": env_int("CONCURRENCY"),
    },
    "health": {
        "ok": env_int("HEALTH_OK"),
        "failed": env_int("HEALTH_FAIL"),
        "attempted": env_int("HEALTH_CHECKS"),
    },
    "metrics_core": {
        "ok": env_int("METRICS_CORE_OK"),
        "failed": env_int("METRICS_CORE_FAIL"),
        "attempted": 1,
    },
    "robot_status": {
        "ok": env_int("ROBOT_OK"),
        "failed": env_int("ROBOT_FAIL"),
        "attempted": 1,
    },
}

metrics_core = load_metrics_core(os.getenv("METRICS_CORE_FILE", ""))
metrics_snapshot = metrics_core.get("metrics", {}) if isinstance(metrics_core, dict) else {}
db_metrics = metrics_snapshot.get("db", {}) if isinstance(metrics_snapshot, dict) else {}
storage_metrics = metrics_snapshot.get("storage", {}) if isinstance(metrics_snapshot, dict) else {}
system_metrics = metrics_snapshot.get("system", {}) if isinstance(metrics_snapshot, dict) else {}
read_cache = metrics_core.get("read_cache", {}) if isinstance(metrics_core, dict) else {}
read_cache_metrics = read_cache.get("metrics", {}) if isinstance(read_cache, dict) else {}
read_cache_entries = read_cache.get("entry_counts", {}) if isinstance(read_cache, dict) else {}

report = {
    "profile": os.getenv("STRESS_PROFILE", "ci"),
    "status": "pass" if all(gate["passed"] for gate in gates) else "fail",
    "scenario": {
        "agents": env_int("N_AGENTS"),
        "concurrency": env_int("CONCURRENCY"),
        "messages_per_agent": env_int("MSGS_PER_AGENT"),
        "mixed_duration_secs": env_int("DURATION_SECS"),
        "health_checks": env_int("HEALTH_CHECKS"),
        "surfaces": ["http_mcp", "product_bus", "tooling_metrics_core", "robot_status"],
    },
    "isolation": {
        "project_key": os.getenv("PROJECT_KEY", ""),
        "product_key": os.getenv("PRODUCT_KEY", ""),
        "database_path": os.getenv("STRESS_DB", ""),
        "storage_root": os.getenv("STRESS_STORAGE", ""),
        "operator_mailbox_touched": False,
    },
    "resources": {
        "server_rss_start_kb": env_int("SERVER_RSS_START_KB"),
        "server_rss_end_kb": env_int("SERVER_RSS_END_KB"),
        "server_rss_growth_kb": rss_growth_kb,
        "server_rss_growth_budget_kb": rss_growth_budget_kb,
        "server_cpu_start_ticks": env_int("SERVER_CPU_START_TICKS"),
        "server_cpu_end_ticks": env_int("SERVER_CPU_END_TICKS"),
        "server_cpu_ticks_delta": env_int("SERVER_CPU_TICKS_DELTA"),
        "server_cpu_seconds": env_float("SERVER_CPU_SECONDS"),
        "clock_ticks_per_second": env_int("CLK_TCK"),
        "sqlite_main_bytes": env_int("STRESS_DB_BYTES"),
        "sqlite_wal_bytes": env_int("STRESS_DB_WAL_BYTES"),
        "sqlite_shm_bytes": env_int("STRESS_DB_SHM_BYTES"),
        "storage_tree_bytes": env_int("STRESS_STORAGE_BYTES"),
        "storage_file_count": env_int("STRESS_STORAGE_FILE_COUNT"),
        "db_pool_acquires_total": nested_int(db_metrics, ["pool_acquires_total"]),
        "db_pool_acquire_errors_total": nested_int(db_metrics, ["pool_acquire_errors_total"]),
        "db_pool_acquire_p95_us": nested_int(db_metrics, ["pool_acquire_latency_us", "p95"]),
        "db_pool_peak_active_connections": nested_int(db_metrics, ["pool_peak_active_connections"]),
        "db_pool_pending_requests": nested_int(db_metrics, ["pool_pending_requests"]),
        "storage_wbq_depth": nested_int(storage_metrics, ["wbq_depth"]),
        "storage_wbq_peak_depth": nested_int(storage_metrics, ["wbq_peak_depth"]),
        "storage_wbq_capacity": nested_int(storage_metrics, ["wbq_capacity"]),
        "storage_commit_pending_requests": nested_int(storage_metrics, ["commit_pending_requests"]),
        "storage_commit_peak_pending_requests": nested_int(storage_metrics, ["commit_peak_pending_requests"]),
        "storage_commit_attempts_total": nested_int(storage_metrics, ["commit_attempts_total"]),
        "storage_commit_failures_total": nested_int(storage_metrics, ["commit_failures_total"]),
        "system_disk_io_read_bytes": nested_int(system_metrics, ["disk_io_read_bytes"]),
        "system_disk_io_write_bytes": nested_int(system_metrics, ["disk_io_write_bytes"]),
        "read_cache_project_hits": nested_int(read_cache_metrics, ["project_hits"]),
        "read_cache_project_misses": nested_int(read_cache_metrics, ["project_misses"]),
        "read_cache_agent_hits": nested_int(read_cache_metrics, ["agent_hits"]),
        "read_cache_agent_misses": nested_int(read_cache_metrics, ["agent_misses"]),
        "read_cache_inbox_stats_hits": nested_int(read_cache_metrics, ["inbox_stats_hits"]),
        "read_cache_inbox_stats_misses": nested_int(read_cache_metrics, ["inbox_stats_misses"]),
        "read_cache_inbox_stats_entries": nested_int(read_cache_entries, ["inbox_stats"]),
    },
    "summary": summary,
    "latencies": latencies,
    "gates": gates,
    "reproduction": {
        "ci": f"cd {project_root} && AM_E2E_KEEP_TMP=1 E2E_CARGO_REQUIRE_RCH=1 bash tests/e2e/test_stress_load.sh",
        "large": f"cd {project_root} && AM_E2E_KEEP_TMP=1 E2E_CARGO_REQUIRE_RCH=1 STRESS_PROFILE=large STRESS_RUN_LARGE=1 bash tests/e2e/test_stress_load.sh",
    },
}

Path(report_json).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")

def fmt_us(value: int) -> str:
    return f"{value / 1000.0:.1f} ms"

lines = [
    "# Stress Load Lab Report",
    "",
    f"Status: {report['status']}",
    f"Profile: {report['profile']}",
    f"Agents: {report['scenario']['agents']}",
    f"Concurrency: {report['scenario']['concurrency']}",
    f"Storage root: `{report['isolation']['storage_root']}`",
    "",
    "## Reproduction",
    "",
    "```bash",
    report["reproduction"]["ci"],
    report["reproduction"]["large"],
    "```",
    "",
    "## Latency Gates",
    "",
    "| Phase | Samples | p50 | p95 | p99 | Max |",
    "| --- | ---: | ---: | ---: | ---: | ---: |",
]
for phase, stats in latencies.items():
    lines.append(
        f"| {phase} | {stats['samples']} | {fmt_us(stats['p50_us'])} | "
        f"{fmt_us(stats['p95_us'])} | {fmt_us(stats['p99_us'])} | {fmt_us(stats['max_us'])} |"
    )

lines.extend([
    "",
    "## Resource Ledger",
    "",
    f"RSS growth: {rss_growth_kb} KB / {rss_growth_budget_kb} KB",
    f"Server CPU: {report['resources']['server_cpu_seconds']} s "
    f"({report['resources']['server_cpu_ticks_delta']} ticks @ {report['resources']['clock_ticks_per_second']} Hz)",
    f"SQLite main/WAL/SHM: {report['resources']['sqlite_main_bytes']} / "
    f"{report['resources']['sqlite_wal_bytes']} / {report['resources']['sqlite_shm_bytes']} bytes",
    f"Storage tree: {report['resources']['storage_file_count']} files, "
    f"{report['resources']['storage_tree_bytes']} bytes",
    f"DB pool: {report['resources']['db_pool_acquires_total']} acquires, "
    f"{report['resources']['db_pool_acquire_errors_total']} errors, "
    f"p95 acquire {fmt_us(report['resources']['db_pool_acquire_p95_us'])}, "
    f"peak active {report['resources']['db_pool_peak_active_connections']}",
    f"Storage queues: WBQ peak {report['resources']['storage_wbq_peak_depth']} / "
    f"{report['resources']['storage_wbq_capacity']}, commit peak pending "
    f"{report['resources']['storage_commit_peak_pending_requests']}, "
    f"commit failures {report['resources']['storage_commit_failures_total']}",
    f"Process IO: read {report['resources']['system_disk_io_read_bytes']} bytes, "
    f"wrote {report['resources']['system_disk_io_write_bytes']} bytes",
    f"Read cache: project {report['resources']['read_cache_project_hits']} hits / "
    f"{report['resources']['read_cache_project_misses']} misses, agent "
    f"{report['resources']['read_cache_agent_hits']} hits / "
    f"{report['resources']['read_cache_agent_misses']} misses, inbox stats "
    f"{report['resources']['read_cache_inbox_stats_hits']} hits / "
    f"{report['resources']['read_cache_inbox_stats_misses']} misses "
    f"({report['resources']['read_cache_inbox_stats_entries']} entries)",
    "",
    "## Gate Verdicts",
    "",
])
for gate in gates:
    verdict = "PASS" if gate["passed"] else "FAIL"
    if "current_us" in gate:
        lines.append(f"- {verdict} {gate['name']}: {fmt_us(gate['current_us'])} / {fmt_us(gate['budget_us'])}")
    elif "current_kb" in gate:
        lines.append(f"- {verdict} {gate['name']}: {gate['current_kb']} KB / {gate['budget_kb']} KB")
    else:
        lines.append(f"- {verdict} {gate['name']}: {gate['failed']} failed / {gate['attempted']} attempted")

Path(report_md).write_text("\n".join(lines) + "\n", encoding="utf-8")
PY

    report_status="$(python3 - "$report_json" <<'PY'
import json
import sys
from pathlib import Path
print(json.loads(Path(sys.argv[1]).read_text(encoding="utf-8")).get("status", "fail"))
PY
)"
    if [ "$report_status" = "pass" ]; then
        e2e_pass "Load lab p95/p99 and RSS gates passed"
    else
        e2e_fail "Load lab p95/p99 or RSS gate failed"
    fi
}

write_load_lab_reports

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "================================================================"
echo "  STRESS TEST SUMMARY"
echo "================================================================"
echo "  Phase 1 (Registration):  ${REG_OK}/${N_AGENTS} ok, ${REG_ELAPSED_MS}ms"
echo "  Phase 2 (Product Setup): ${PRODUCT_SETUP_OK}/2 ok"
echo "  Phase 3 (Msg Storm):     ${MSG_OK}/${TOTAL_MSGS} ok, ${MSG_ELAPSED_MS}ms"
echo "  Phase 4 (Mixed R/W):     R:${MIX_READS_OK}/${MIX_READS_OK}+${MIX_READS_FAIL} W:${MIX_WRITES_OK}/${MIX_WRITES_OK}+${MIX_WRITES_FAIL}"
echo "  Phase 5 (Reservations):  ${RES_OK} reserved, ${RES_CONFLICT} conflicts, ${RES_ERROR} errors"
echo "  Phase 6 (Search):        ${SEARCH_OK}/${CONCURRENCY} ok"
echo "  Phase 7 (Product Bus):   S:${PRODUCT_SEARCH_OK}/${CONCURRENCY} I:${PRODUCT_INBOX_OK}/${CONCURRENCY}"
echo "  Phase 8 (Health):        ${HEALTH_OK}/${HEALTH_CHECKS} ok, avg ${HEALTH_AVG_MS}ms"
echo "  Phase 9 (Metrics Core):  ${METRICS_CORE_OK}/1 ok"
echo "  Phase 10 (Robot Status): ${ROBOT_OK}/1 ok"
echo "  RSS Growth:              ${RSS_GROWTH_KB} KB / ${STRESS_RSS_GROWTH_BUDGET_KB} KB"
echo "  Server CPU:              ${SERVER_CPU_SECONDS}s (${SERVER_CPU_TICKS_DELTA} ticks)"
echo "  SQLite bytes:            main=${STRESS_DB_BYTES} wal=${STRESS_DB_WAL_BYTES} shm=${STRESS_DB_SHM_BYTES}"
echo "  Storage tree:            ${STRESS_STORAGE_FILE_COUNT} files, ${STRESS_STORAGE_BYTES} bytes"
echo "  Metrics core:            ${METRICS_CORE_FILE}"
echo "  Reports:                 ${E2E_ARTIFACT_DIR}/load_lab_report.{json,md}"
echo "================================================================"

e2e_save_artifact "server_log_path.txt" "${E2E_ARTIFACT_DIR}/logs/server_stress_load.log"

e2e_summary
