#!/usr/bin/env bash
# test_tui_liveness.sh - E2E for the TUI/loop liveness contract (Track I)
# @tags: reliability, track-i, track-n, tui, liveness
#
# Bead: br-bvq1x.14.11 (N11). Proves the I1-I6 liveness surfaces end to end:
#   I1: loop heartbeats advance under normal operation (live server), and the
#       deterministic injected render-stall fixture captures staleness without
#       hiding input progress (L3 unit fixture).
#   I2: robot health surfaces per-loop state; the stalled verdict attaches the
#       exact headless fallback command (unit verdict tests + live checks).
#   I3: ATC tick-budget overrun debt is recorded by the L3 replay fixture and
#       the tick-budget surface is exposed in health output.
#   I4: process-owner runtime state and divergences are surfaced; the live
#       server's own PID is detected as an actual listener process.
#   I5: the commit-coalescer heartbeat is part of the loop roster and becomes
#       observed once archive writes flow.
#   I6: am tui-dump / am status --json stay usable with NO live server (local
#       fallback, exit 0) and against a live one (source=live, exit 0).
#
# Phases:
#   Phase 0: deterministic unit fixtures (L3 stall/budget + verdict mapping)
#   Phase 1: offline CLI surface - non-fatal fallbacks, exit codes (I2/I6)
#   Phase 2: live headless server - heartbeats advance, live shapes (I1-I6)
#   Phase 3: cleanup

export E2E_SUITE="tui_liveness"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI / Loop Liveness E2E Test Suite (Track I)"

json_field() {
    # json_field <file> <python-expression-over-d>
    python3 -c "
import json, sys
with open('$1') as f:
    d = json.load(f)
print($2)
" 2>/dev/null
}

# ── Phase 0: Deterministic liveness fixtures ─────────────────────────

e2e_section "Phase 0: L3 render-stall heartbeat fixture (I1)"
if e2e_run_cargo test -p mcp-agent-mail-server --lib 'tui_bridge::tests::loop_heartbeat' -- --test-threads=4 2>&1 | tee "${E2E_ARTIFACT_DIR}/heartbeat_fixtures.log" | tail -5 | grep -q 'test result: ok' \
    && ! grep -qE 'test result: ok\. 0 passed' "${E2E_ARTIFACT_DIR}/heartbeat_fixtures.log"; then
    PASS_COUNT=$(grep 'test result: ok' "${E2E_ARTIFACT_DIR}/heartbeat_fixtures.log" | grep -oP '\d+ passed' | head -1)
    e2e_pass "tui_bridge loop-heartbeat fixtures (incl. 186ms injected render stall): ${PASS_COUNT}"
else
    e2e_fail "tui_bridge loop-heartbeat fixtures failed — see ${E2E_ARTIFACT_DIR}/heartbeat_fixtures.log"
fi

e2e_section "Phase 0: ATC tick-budget overrun replay fixture (I3)"
if e2e_run_cargo test -p mcp-agent-mail-server --lib 'atc_replay_tick_fixture_records_budget_overrun_debt' 2>&1 | tee "${E2E_ARTIFACT_DIR}/atc_budget_fixture.log" | tail -5 | grep -q 'test result: ok' \
    && ! grep -qE 'test result: ok\. 0 passed' "${E2E_ARTIFACT_DIR}/atc_budget_fixture.log"; then
    e2e_pass "ATC replay fixture records the 186ms-vs-budget overrun debt"
else
    e2e_fail "ATC budget-overrun replay fixture failed — see ${E2E_ARTIFACT_DIR}/atc_budget_fixture.log"
fi

e2e_section "Phase 0: liveness verdict mapping (I2)"
if e2e_run_cargo test -p mcp-agent-mail-cli --lib 'tui_liveness_report' 2>&1 | tee "${E2E_ARTIFACT_DIR}/verdict_mapping.log" | tail -5 | grep -q 'test result: ok' \
    && ! grep -qE 'test result: ok\. 0 passed' "${E2E_ARTIFACT_DIR}/verdict_mapping.log"; then
    PASS_COUNT=$(grep 'test result: ok' "${E2E_ARTIFACT_DIR}/verdict_mapping.log" | grep -oP '\d+ passed' | head -1)
    e2e_pass "per-loop verdict mapping + headless-fallback attachment: ${PASS_COUNT}"
else
    e2e_fail "liveness verdict mapping tests failed — see ${E2E_ARTIFACT_DIR}/verdict_mapping.log"
fi

# ── Phase 1: Offline CLI surface (no live server) ────────────────────

e2e_section "Phase 1: offline liveness surfaces stay non-fatal (I2/I6)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

OFFLINE_WORK="$(e2e_mktemp "e2e_tui_liveness_offline")"
mkdir -p "${OFFLINE_WORK}/storage" "${OFFLINE_WORK}/repo"
# Port 1 is never listening: forces the live-fetch path to fail fast so the
# local fallback contract is what gets exercised.
OFFLINE_ENV=(
    "DATABASE_URL=sqlite:///${OFFLINE_WORK}/db.sqlite3"
    "STORAGE_ROOT=${OFFLINE_WORK}/storage"
    "HTTP_HOST=127.0.0.1"
    "HTTP_PORT=1"
    "AM_INTERFACE_MODE=cli"
)

# With NO resolvable mailbox at all, tui-dump must still exit 0 and say so
# honestly (I6: the escape hatch never hard-fails).
TUIDUMP_UNAVAIL_JSON="${E2E_ARTIFACT_DIR}/tui_dump_unavailable.json"
if env "${OFFLINE_ENV[@]}" am tui-dump --format json >"${TUIDUMP_UNAVAIL_JSON}" 2>/dev/null; then
    e2e_pass "am tui-dump exits 0 with no resolvable mailbox (I6)"
else
    e2e_fail "am tui-dump exited non-zero with no resolvable mailbox (violates I6 always-exit-0)"
fi
if [ "$(json_field "${TUIDUMP_UNAVAIL_JSON}" "d['source'] == 'unavailable' and len(d.get('_alerts', [])) > 0")" = "True" ]; then
    e2e_pass "unresolvable mailbox is reported honestly (source='unavailable' + alert)"
else
    e2e_fail "unresolvable-mailbox shape wrong — see ${TUIDUMP_UNAVAIL_JSON}"
fi

# Seed a minimal mailbox (project + auto-named agent) so the local-fallback
# read-out and status snapshot have something real to read.
if env "${OFFLINE_ENV[@]}" am macros start-session --project "${OFFLINE_WORK}/repo" \
    --program e2e-liveness --model probe \
    >"${E2E_ARTIFACT_DIR}/offline_seed.out" 2>"${E2E_ARTIFACT_DIR}/offline_seed.stderr"; then
    e2e_pass "offline mailbox seeded via macros start-session"
else
    e2e_fail "failed to seed offline mailbox — see ${E2E_ARTIFACT_DIR}/offline_seed.stderr"
fi

HEALTH_JSON="${E2E_ARTIFACT_DIR}/robot_health_offline.json"
if env "${OFFLINE_ENV[@]}" am robot health --format json >"${HEALTH_JSON}" 2>"${E2E_ARTIFACT_DIR}/robot_health_offline.stderr"; then
    e2e_pass "am robot health exits 0 with no live server"
else
    e2e_fail "am robot health exited non-zero with no live server"
fi

if [ "$(json_field "${HEALTH_JSON}" "d['tui_liveness']['source']")" = "unreachable" ]; then
    e2e_pass "offline tui_liveness.source is 'unreachable' (honest, non-fatal)"
else
    e2e_fail "offline tui_liveness.source not 'unreachable' — see ${HEALTH_JSON}"
fi

for key in tui_liveness tui_tick process_owner process_owner_divergences; do
    if [ "$(json_field "${HEALTH_JSON}" "'${key}' in d")" = "True" ]; then
        e2e_pass "robot health surfaces '${key}'"
    else
        e2e_fail "robot health missing '${key}' — see ${HEALTH_JSON}"
    fi
done

TUIDUMP_JSON="${E2E_ARTIFACT_DIR}/tui_dump_offline.json"
if env "${OFFLINE_ENV[@]}" am tui-dump --format json --project "${OFFLINE_WORK}/repo" \
    >"${TUIDUMP_JSON}" 2>"${E2E_ARTIFACT_DIR}/tui_dump_offline.stderr"; then
    e2e_pass "am tui-dump exits 0 with no live server (I6 contract)"
else
    e2e_fail "am tui-dump exited non-zero with no live server (violates I6 always-exit-0)"
fi

if [ "$(json_field "${TUIDUMP_JSON}" "d['source']")" = "local-fallback" ]; then
    e2e_pass "offline tui-dump falls back to source='local-fallback'"
else
    e2e_fail "offline tui-dump source is not 'local-fallback' — see ${TUIDUMP_JSON}"
fi

if [ "$(json_field "${TUIDUMP_JSON}" "'tui_liveness' in d and 'status' in d and d['tui_liveness']['source'] == 'unreachable'")" = "True" ]; then
    e2e_pass "offline tui-dump carries tui_liveness (unreachable) + status snapshot"
else
    e2e_fail "offline tui-dump missing tui_liveness/status — see ${TUIDUMP_JSON}"
fi

STATUS_JSON="${E2E_ARTIFACT_DIR}/status_offline.json"
if env "${OFFLINE_ENV[@]}" am status --json --project "${OFFLINE_WORK}/repo" >"${STATUS_JSON}" 2>/dev/null \
    && python3 -c "import json;json.load(open('${STATUS_JSON}'))" 2>/dev/null; then
    e2e_pass "am status --json works with no live server (I6)"
else
    e2e_fail "am status --json failed with no live server"
fi

# ── Phase 2: Live headless server ────────────────────────────────────

e2e_section "Phase 2: live headless server liveness (I1/I2/I4/I5/I6)"

LIVE_WORK="$(e2e_mktemp "e2e_tui_liveness_live")"
LIVE_DB="${LIVE_WORK}/live.sqlite3"
LIVE_STORAGE="${LIVE_WORK}/storage"
mkdir -p "${LIVE_STORAGE}"

if e2e_start_server_with_logs "${LIVE_DB}" "${LIVE_STORAGE}" "tui_liveness_live" \
    "HTTP_BEARER_TOKEN=" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0"; then
    e2e_pass "Headless server started (pid=${E2E_SERVER_PID})"
else
    e2e_fail "Headless server failed to start"
    e2e_summary
    exit 1
fi

LIVE_PORT="${E2E_SERVER_URL#http://127.0.0.1:}"
LIVE_PORT="${LIVE_PORT%%/*}"
WS_STATE_URL="http://127.0.0.1:${LIVE_PORT}/mail/ws-state?limit=1&system_health=1"
LIVE_ENV=(
    "DATABASE_URL=sqlite:///${LIVE_DB}"
    "STORAGE_ROOT=${LIVE_STORAGE}"
    "HTTP_HOST=127.0.0.1"
    "HTTP_PORT=${LIVE_PORT}"
    "AM_INTERFACE_MODE=cli"
)

# Touch the MCP surface so the mcp_api heartbeat has at least one observation.
curl -sS -X POST "${E2E_SERVER_URL}" -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
    >"${E2E_ARTIFACT_DIR}/health_check_call.log" 2>&1 || true

WS1_JSON="${E2E_ARTIFACT_DIR}/ws_state_poll1.json"
if curl -sS "${WS_STATE_URL}" >"${WS1_JSON}" 2>/dev/null \
    && python3 -c "import json;json.load(open('${WS1_JSON}'))" 2>/dev/null; then
    e2e_pass "/mail/ws-state?system_health=1 returns JSON"
else
    e2e_fail "/mail/ws-state probe failed — see ${WS1_JSON}"
fi

LOOP_KINDS="$(json_field "${WS1_JSON}" "sorted(e['kind'] for e in d['system_health']['loop_heartbeats'])")"
e2e_log "loop roster: ${LOOP_KINDS}"
for kind in mcp_api commit_coalescer db_poll render input; do
    if [[ "${LOOP_KINDS}" == *"'${kind}'"* ]]; then
        e2e_pass "loop_heartbeats roster includes '${kind}'"
    else
        e2e_fail "loop_heartbeats roster missing '${kind}' — see ${WS1_JSON}"
    fi
done

if [ "$(json_field "${WS1_JSON}" "any(e['kind'] == 'mcp_api' and e['observed'] and not e['stale'] for e in d['system_health']['loop_heartbeats'])")" = "True" ]; then
    e2e_pass "mcp_api heartbeat observed and not stale after MCP call (I1)"
else
    e2e_fail "mcp_api heartbeat not observed/fresh after MCP call — see ${WS1_JSON}"
fi

# I1: heartbeats ADVANCE — a second MCP call must move mcp_api's last tick.
TICK1="$(json_field "${WS1_JSON}" "next(e['last_tick_micros'] for e in d['system_health']['loop_heartbeats'] if e['kind'] == 'mcp_api')")"
sleep 1
curl -sS -X POST "${E2E_SERVER_URL}" -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"health_check","arguments":{}}}' \
    >/dev/null 2>&1 || true
WS2_JSON="${E2E_ARTIFACT_DIR}/ws_state_poll2.json"
curl -sS "${WS_STATE_URL}" >"${WS2_JSON}" 2>/dev/null || true
TICK2="$(json_field "${WS2_JSON}" "next(e['last_tick_micros'] for e in d['system_health']['loop_heartbeats'] if e['kind'] == 'mcp_api')")"
if [ -n "${TICK1}" ] && [ -n "${TICK2}" ] && [ "${TICK2}" -gt "${TICK1}" ] 2>/dev/null; then
    e2e_pass "mcp_api heartbeat advanced between polls (${TICK1} -> ${TICK2}) (I1)"
else
    e2e_fail "mcp_api heartbeat did not advance (tick1=${TICK1}, tick2=${TICK2})"
fi

LIVE_HEALTH_JSON="${E2E_ARTIFACT_DIR}/robot_health_live.json"
if env "${LIVE_ENV[@]}" am robot health --format json >"${LIVE_HEALTH_JSON}" 2>"${E2E_ARTIFACT_DIR}/robot_health_live.stderr"; then
    e2e_pass "am robot health exits 0 against live server"
else
    e2e_fail "am robot health exited non-zero against live server"
fi

if [ "$(json_field "${LIVE_HEALTH_JSON}" "d['tui_liveness']['source']")" = "live" ]; then
    e2e_pass "live tui_liveness.source is 'live' (I2)"
else
    e2e_fail "live tui_liveness.source not 'live' — see ${LIVE_HEALTH_JSON}"
fi

# Headless loops (render/input/db_poll) are unobserved, not stalled: the
# verdict must be 'alive' with NO stalled loops and NO fallback command.
if [ "$(json_field "${LIVE_HEALTH_JSON}" "d['tui_liveness']['overall']")" = "alive" ]; then
    e2e_pass "live overall verdict is 'alive' (unobserved != stalled) (I2)"
else
    e2e_fail "live overall verdict not 'alive' — see ${LIVE_HEALTH_JSON}"
fi

if [ "$(json_field "${LIVE_HEALTH_JSON}" "d['tui_liveness'].get('stalled_loops') == [] and d['tui_liveness'].get('headless_fallback_command') is None")" = "True" ]; then
    e2e_pass "healthy server advertises no headless-fallback command (I2 exactness)"
else
    e2e_fail "healthy server advertised stalled loops / fallback command — see ${LIVE_HEALTH_JSON}"
fi

if [ "$(json_field "${LIVE_HEALTH_JSON}" "len(d['tui_liveness']['loops']) >= 5")" = "True" ]; then
    e2e_pass "per-loop state list covers the full roster (I2)"
else
    e2e_fail "per-loop state list too short — see ${LIVE_HEALTH_JSON}"
fi

# I4: our live server PID must show up as an actual listener process.
if [ "$(json_field "${LIVE_HEALTH_JSON}" "any(p.get('pid') == ${E2E_SERVER_PID} for p in d['process_owner'].get('actual_processes', []))")" = "True" ]; then
    e2e_pass "process_owner detects the live server pid ${E2E_SERVER_PID} (I4)"
else
    e2e_fail "process_owner did not list live server pid ${E2E_SERVER_PID} — see ${LIVE_HEALTH_JSON}"
fi

if [ "$(json_field "${LIVE_HEALTH_JSON}" "isinstance(d.get('process_owner_divergences'), list)")" = "True" ]; then
    e2e_pass "process_owner_divergences surfaced as a list (I4)"
else
    e2e_fail "process_owner_divergences missing/malformed — see ${LIVE_HEALTH_JSON}"
fi

# I5: archive write via MCP → commit coalescer must tick.
curl -sS -X POST "${E2E_SERVER_URL}" -H "Content-Type: application/json" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${LIVE_WORK}/repo\"}}}" \
    >"${E2E_ARTIFACT_DIR}/ensure_project_call.log" 2>&1 || true
sleep 2
WS3_JSON="${E2E_ARTIFACT_DIR}/ws_state_poll3.json"
curl -sS "${WS_STATE_URL}" >"${WS3_JSON}" 2>/dev/null || true
if [ "$(json_field "${WS3_JSON}" "any(e['kind'] == 'commit_coalescer' and e['observed'] for e in d['system_health']['loop_heartbeats'])")" = "True" ]; then
    e2e_pass "commit_coalescer heartbeat observed after archive write (I5)"
else
    e2e_fail "commit_coalescer heartbeat never observed after archive write — see ${WS3_JSON}"
fi

# I6: tui-dump against the live server reports source=live and exits 0.
TUIDUMP_LIVE_JSON="${E2E_ARTIFACT_DIR}/tui_dump_live.json"
if env "${LIVE_ENV[@]}" am tui-dump --format json >"${TUIDUMP_LIVE_JSON}" 2>/dev/null; then
    e2e_pass "am tui-dump exits 0 against live server (I6)"
else
    e2e_fail "am tui-dump exited non-zero against live server"
fi

if [ "$(json_field "${TUIDUMP_LIVE_JSON}" "d['source']")" = "live" ]; then
    e2e_pass "live tui-dump reports source='live' (I6)"
else
    e2e_fail "live tui-dump source not 'live' — see ${TUIDUMP_LIVE_JSON}"
fi

# ── Phase 3: Cleanup ─────────────────────────────────────────────────

e2e_section "Phase 3: Cleanup"
e2e_stop_server
if [ -n "${E2E_SERVER_PID:-}" ] && kill -0 "${E2E_SERVER_PID}" 2>/dev/null; then
    e2e_fail "Server process ${E2E_SERVER_PID} still running after stop"
else
    e2e_pass "Server stopped cleanly"
fi

e2e_summary
