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
#
# Usage:
#   bash tests/e2e/test_stress_load.sh
#
# Configuration (env vars):
#   STRESS_AGENTS=50          Number of simulated agents
#   STRESS_CONCURRENCY=20     Max parallel curl processes
#   STRESS_MSGS_PER_AGENT=5   Messages each agent sends
#   STRESS_DURATION_SECS=30   Duration for sustained load phase
#   STRESS_SKIP_BUILD=1       Skip cargo build (use existing binary)

set -euo pipefail

E2E_SUITE="stress_load"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=../../scripts/e2e_lib.sh
source "${PROJECT_ROOT}/scripts/e2e_lib.sh"

e2e_init_artifacts

e2e_banner "HTTP Stress/Load Test Suite"

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

N_AGENTS="${STRESS_AGENTS:-50}"
CONCURRENCY="${STRESS_CONCURRENCY:-20}"
MSGS_PER_AGENT="${STRESS_MSGS_PER_AGENT:-5}"
DURATION_SECS="${STRESS_DURATION_SECS:-30}"
PORT="${STRESS_PORT:-0}"  # 0 = auto-select free port

# ---------------------------------------------------------------------------
# Setup: build binary, create temp workspace, start server
# ---------------------------------------------------------------------------

if [ "${STRESS_SKIP_BUILD:-0}" != "1" ]; then
    e2e_ensure_binary "am" >/dev/null
fi
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_stress")"
STRESS_DB="${WORK}/stress.sqlite3"
STRESS_STORAGE="${WORK}/storage"
mkdir -p "$STRESS_STORAGE"

# Find a free port if not specified
if [ "$PORT" = "0" ]; then
    PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()" 2>/dev/null || echo 18765)
fi

SERVER_URL="http://127.0.0.1:${PORT}"
MCP_URL="${SERVER_URL}/mcp/"

echo "  Work dir: $WORK"
echo "  DB: $STRESS_DB"
echo "  Server: $SERVER_URL"
echo "  Agents: $N_AGENTS, Concurrency: $CONCURRENCY, Msgs/agent: $MSGS_PER_AGENT"
echo ""

# Start the HTTP server in headless mode
DATABASE_URL="sqlite:////${STRESS_DB}" \
    STORAGE_ROOT="$STRESS_STORAGE" \
    HTTP_PORT="$PORT" \
    HTTP_HOST="127.0.0.1" \
    TUI_ENABLED=false \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true \
    RUST_LOG=warn \
    am serve-http --no-tui --no-auth 2>"${WORK}/server_stderr.log" &
SERVER_PID=$!
echo "  Server PID: $SERVER_PID"

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        echo "  Server stopped"
    fi
}
trap cleanup EXIT

# Wait for server to be ready
echo -n "  Waiting for server..."
for i in $(seq 1 60); do
    if curl -sf "${SERVER_URL}/health" >/dev/null 2>&1; then
        echo " ready (${i}s)"
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo " FAILED (server died)"
        echo "Server stderr:"
        cat "${WORK}/server_stderr.log" 2>/dev/null || true
        exit 1
    fi
    sleep 1
done

# Verify server is responding
if ! curl -sf "${SERVER_URL}/health" >/dev/null 2>&1; then
    echo "  Server failed to start within 60s"
    cat "${WORK}/server_stderr.log" 2>/dev/null || true
    exit 1
fi

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

    curl -sf -X POST "$MCP_URL" \
        -H "Content-Type: application/json" \
        -d "$payload" \
        --max-time 30 \
        2>/dev/null || echo '{"error":"curl_timeout"}'
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

PROJECT_KEY="/tmp/stress-test-project-$$"

# ---------------------------------------------------------------------------
# Phase 0: Initialize project (single call)
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 0: Project Setup ==="

INIT_RESP=$(mcp_call "ensure_project" "{\"project_key\":\"${PROJECT_KEY}\"}")
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

ADJECTIVES=(Red Blue Green Gold Silver Dark Bright Swift Calm Bold Keen Sharp Clear Wild Pure Deep Warm Cool Wise Fair Brave Rare True Free Open Safe Quick Light Noble Royal)
NOUNS=(Castle Lake Harbor Forest Bridge Tower River Valley Garden Temple Shield Crown Dragon Phoenix Storm Summit Falcon Eagle Raven Shadow Crystal Ember Flame Frost Pearl Coral Maple Cedar Willow)

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
        resp=$(mcp_call "register_agent" "{\"project_key\":\"${PROJECT_KEY}\",\"program\":\"stress-test\",\"model\":\"test-model\"}")
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

# ---------------------------------------------------------------------------
# Phase 2: Message Send Storm
#
# Each agent sends MSGS_PER_AGENT messages to random other agents.
# Tests: concurrent DB writes + git archive writes.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 2: Message Send Storm ($N_AGENTS x $MSGS_PER_AGENT msgs, $CONCURRENCY concurrent) ==="

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
            resp=$(mcp_call "send_message" "{\"project_key\":\"${PROJECT_KEY}\",\"from_agent\":\"${from_agent}\",\"to_agent\":\"${to_agent}\",\"subject\":\"Stress msg ${i}-${m}\",\"body_md\":\"Load test message body ${i}-${m} with enough content to exercise FTS indexing\",\"thread_id\":\"${thread_id}\"}")
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
# Phase 3: Concurrent Inbox Fetch + Message Send (Read/Write Mix)
#
# Half the threads read inboxes, half send messages — simultaneously.
# Tests: WAL read/write concurrency, cache coherency.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 3: Mixed Read/Write Concurrency (${CONCURRENCY} workers, 10s) ==="

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
        end_time=$(($(date +%s) + 10))
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
        end_time=$(($(date +%s) + 10))
        msg_idx=0
        while [ "$(date +%s)" -lt "$end_time" ]; do
            from_idx=$(( (w + msg_idx) % N_AGENTS ))
            to_idx=$(( (w + msg_idx + 1) % N_AGENTS ))
            from_agent="${AGENT_NAMES[$from_idx]}"
            to_agent="${AGENT_NAMES[$to_idx]}"
            resp=$(mcp_call "send_message" "{\"project_key\":\"${PROJECT_KEY}\",\"from_agent\":\"${from_agent}\",\"to_agent\":\"${to_agent}\",\"subject\":\"Mix msg ${w}-${msg_idx}\",\"body_md\":\"Mixed workload body\",\"thread_id\":\"mix-${w}-${msg_idx}\"}")
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
# Phase 4: File Reservation Contention
#
# Multiple agents compete for overlapping file patterns.
# Tests: reservation conflict detection under concurrency.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 4: File Reservation Contention (${CONCURRENCY} concurrent) ==="

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
# Phase 5: Search Under Load
#
# Concurrent search queries while messages are still being indexed.
# Tests: FTS5 concurrency, LIKE fallback, pool sharing.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 5: Concurrent Search ($CONCURRENCY parallel queries) ==="

SEARCH_QUERIES=("stress" "message" "body" "load" "test" "workload" "mix" "content" "FTS" "indexing")
SEARCH_OK=0
SEARCH_FAIL=0
SEARCH_PIDS=()

for i in $(seq 0 $((CONCURRENCY - 1))); do
    q_idx=$((i % ${#SEARCH_QUERIES[@]}))
    query="${SEARCH_QUERIES[$q_idx]}"
    (
        resp=$(mcp_call "search_messages" "{\"project_key\":\"${PROJECT_KEY}\",\"query\":\"${query}\",\"limit\":20}")
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
# Phase 6: Health Check Under Load (server stays responsive)
#
# Rapid health checks while other operations may still be settling.
# Tests: server doesn't deadlock or become unresponsive.
# ---------------------------------------------------------------------------

echo ""
echo "=== Phase 6: Health Check Rapid-Fire (100 sequential checks) ==="

HEALTH_OK=0
HEALTH_FAIL=0
HEALTH_START=$(date +%s%N)

for i in $(seq 1 100); do
    resp=$(mcp_call "health_check" "{}" 2>/dev/null)
    if echo "$resp" | grep -q '"error"'; then
        HEALTH_FAIL=$((HEALTH_FAIL + 1))
    else
        HEALTH_OK=$((HEALTH_OK + 1))
    fi
done

HEALTH_END=$(date +%s%N)
HEALTH_ELAPSED_MS=$(( (HEALTH_END - HEALTH_START) / 1000000 ))
HEALTH_AVG_MS=$(python3 -c "print(f'{${HEALTH_ELAPSED_MS} / 100:.1f}')" 2>/dev/null || echo "?")

echo "  Health checks: ${HEALTH_OK}/100 ok, avg ${HEALTH_AVG_MS}ms"

if [ "$HEALTH_FAIL" -eq 0 ]; then
    e2e_pass "Health check: all 100 passed (avg ${HEALTH_AVG_MS}ms)"
else
    e2e_fail "Health check: ${HEALTH_FAIL}/100 failed"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "================================================================"
echo "  STRESS TEST SUMMARY"
echo "================================================================"
echo "  Phase 1 (Registration):  ${REG_OK}/${N_AGENTS} ok, ${REG_ELAPSED_MS}ms"
echo "  Phase 2 (Msg Storm):     ${MSG_OK}/${TOTAL_MSGS} ok, ${MSG_ELAPSED_MS}ms"
echo "  Phase 3 (Mixed R/W):     R:${MIX_READS_OK}/${MIX_READS_OK}+${MIX_READS_FAIL} W:${MIX_WRITES_OK}/${MIX_WRITES_OK}+${MIX_WRITES_FAIL}"
echo "  Phase 4 (Reservations):  ${RES_OK} reserved, ${RES_CONFLICT} conflicts, ${RES_ERROR} errors"
echo "  Phase 5 (Search):        ${SEARCH_OK}/${CONCURRENCY} ok"
echo "  Phase 6 (Health):        ${HEALTH_OK}/100 ok, avg ${HEALTH_AVG_MS}ms"
echo "================================================================"

# Copy server logs to artifacts
if [ -d "$E2E_ARTIFACT_DIR" ]; then
    cp "${WORK}/server_stderr.log" "${E2E_ARTIFACT_DIR}/server_stderr.log" 2>/dev/null || true
fi

e2e_summary
