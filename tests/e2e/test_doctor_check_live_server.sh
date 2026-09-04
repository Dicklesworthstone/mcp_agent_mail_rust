#!/usr/bin/env bash
# test_doctor_check_live_server.sh — GH#138 standalone doctor/live-server contract.
# @tags: reliability, doctor, live-server, locks, linux
#
# A detector-only `am doctor check ... --json` may hold the mailbox file open,
# but it is a reader: it must not become a competing owner, trigger recovery,
# or make the live service's health and JSON-RPC surfaces unavailable.

set -euo pipefail

E2E_SUITE="doctor_check_live_server"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "GH#138 — Standalone Doctor Check Beside a Live Server"

for cmd in curl jq python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done
if [ "$(uname -s)" != "Linux" ] || [ ! -d /proc/self/fd ]; then
    e2e_skip "Linux /proc fd inspection required"
    e2e_summary
    exit 0
fi

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
AM_BIN="$(command -v am 2>/dev/null || true)"
if [ -z "${AM_BIN}" ] || [ ! -x "${AM_BIN}" ]; then
    e2e_fail "could not build/locate the am binary"
    e2e_summary
    exit 1
fi

WORK="$(e2e_mktemp doctor_check_live_server)"
DB_PATH="${WORK}/mailbox.sqlite3"
STORAGE_ROOT="${WORK}/storage"
PROJECT_PATH="${WORK}/project"
HOME_DIR="${WORK}/home"
mkdir -p "${STORAGE_ROOT}" "${PROJECT_PATH}" "${HOME_DIR}"
export HOME="${HOME_DIR}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT
export DATABASE_URL="sqlite:///${DB_PATH}"

DOCTOR_PID=""
cleanup() {
    if [ -n "${DOCTOR_PID}" ] && kill -0 "${DOCTOR_PID}" 2>/dev/null; then
        kill -CONT "${DOCTOR_PID}" 2>/dev/null || true
        kill "${DOCTOR_PID}" 2>/dev/null || true
        wait "${DOCTOR_PID}" 2>/dev/null || true
    fi
    e2e_stop_server || true
}
trap cleanup EXIT

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "doctor_check_live" \
    "TUI_ENABLED=false" \
    "INTEGRITY_CHECK_ON_STARTUP=true" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0"; then
    e2e_fail "live server failed to start"
    e2e_summary
    exit 1
fi
BASE_URL="${E2E_SERVER_URL%/mcp/}"
BASE_URL="${BASE_URL%/mcp}"

rpc_ok() {
    local case_id="$1" label="$2" tool="$3" args="$4"
    e2e_rpc_call "${case_id}" "${E2E_SERVER_URL}" "${tool}" "${args}" || true
    e2e_rpc_assert_success "${case_id}" "${label}"
}

e2e_case_banner "Populate a healthy live mailbox"
rpc_ok "doctor_live_project" "project creation succeeds" "ensure_project" \
    "{\"human_key\":\"${PROJECT_PATH}\"}"
rpc_ok "doctor_live_sender" "sender creation succeeds" "create_agent_identity" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"codex-cli\",\"model\":\"gpt-5.6\",\"name_hint\":\"BlueLake\",\"task_description\":\"GH#138 sender\"}"
rpc_ok "doctor_live_recipient" "recipient creation succeeds" "create_agent_identity" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"claude-code\",\"model\":\"fable-5.1\",\"name_hint\":\"GreenStone\",\"task_description\":\"GH#138 recipient\"}"
rpc_ok "doctor_live_sender_policy" "sender policy opens" "set_contact_policy" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BlueLake\",\"policy\":\"open\"}"
rpc_ok "doctor_live_recipient_policy" "recipient policy opens" "set_contact_policy" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GreenStone\",\"policy\":\"open\"}"

BODY="$(python3 - <<'PY'
print("x" * 65536)
PY
)"
for i in $(seq 1 48); do
    args_json="$(python3 - "${PROJECT_PATH}" "${i}" "${BODY}" <<'PY'
import json
import sys
print(json.dumps({
    "project_key": sys.argv[1],
    "sender_name": "BlueLake",
    "to": ["GreenStone"],
    "subject": f"GH#138 live doctor fixture {sys.argv[2]}",
    "body_md": sys.argv[3],
    "thread_id": "gh-138-live-doctor",
}))
PY
)"
    e2e_rpc_call "doctor_live_seed_${i}" "${E2E_SERVER_URL}" "send_message" "${args_json}" || true
    if ! python3 - "${E2E_ARTIFACT_DIR}/doctor_live_seed_${i}/response.json" <<'PY'
import json
import pathlib
import sys
payload = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if "error" in payload or (payload.get("result") or {}).get("isError") is True:
    raise SystemExit(1)
PY
    then
        e2e_fail "fixture message ${i}/48 failed"
        e2e_summary
        exit 1
    fi
done
e2e_pass "48 archive-backed messages seeded"

DOCTOR_OUT="${E2E_ARTIFACT_DIR}/doctor_check.json"
DOCTOR_ERR="${E2E_ARTIFACT_DIR}/doctor_check.err"
e2e_case_banner "Freeze doctor check while its read-only mailbox descriptor is open"
if command -v nice >/dev/null 2>&1; then
    nice -n 19 "${AM_BIN}" doctor check "${PROJECT_PATH}" --json >"${DOCTOR_OUT}" 2>"${DOCTOR_ERR}" &
else
    "${AM_BIN}" doctor check "${PROJECT_PATH}" --json >"${DOCTOR_OUT}" 2>"${DOCTOR_ERR}" &
fi
DOCTOR_PID=$!

if python3 - "${DOCTOR_PID}" "${DB_PATH}" <<'PY'
import glob
import os
import signal
import sys
import time
pid = int(sys.argv[1])
want = os.path.realpath(sys.argv[2])
deadline = time.monotonic() + 30.0
while time.monotonic() < deadline:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        break
    for fd in glob.glob(f"/proc/{pid}/fd/*"):
        try:
            if os.path.realpath(fd) == want:
                os.kill(pid, signal.SIGSTOP)
                print(fd)
                raise SystemExit(0)
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass
    time.sleep(0.0005)
raise SystemExit(1)
PY
then
    e2e_pass "doctor was stopped with the live mailbox open"
else
    e2e_fail "doctor exited before the harness observed its mailbox descriptor"
    printf '      doctor stderr: %s\n' "$(tail -5 "${DOCTOR_ERR}" 2>/dev/null | tr '\n' ' ' | head -c 500)"
    e2e_summary
    exit 1
fi

LOCKS_OUT="${E2E_ARTIFACT_DIR}/locks_with_doctor_reader.json"
if "${AM_BIN}" doctor locks --json >"${LOCKS_OUT}" 2>"${LOCKS_OUT}.err" \
    && jq -e '.owner_state.class == "live" and .disposition == "active_other_owner"' "${LOCKS_OUT}" >/dev/null; then
    e2e_pass "stopped doctor remains a reader; live server is the sole owner"
else
    e2e_fail "doctor descriptor was classified as a competing owner"
    printf '      locks: %s\n' "$(cat "${LOCKS_OUT}" "${LOCKS_OUT}.err" 2>/dev/null | tr '\n' ' ' | head -c 700)"
fi

for i in 1 2 3 4 5; do
    status="$(curl -sS --max-time 5 -o "${E2E_ARTIFACT_DIR}/health_${i}.json" \
        -w '%{http_code}' "${BASE_URL}/health" 2>"${E2E_ARTIFACT_DIR}/health_${i}.err" || true)"
    if [ "${status}" = "200" ]; then
        e2e_pass "health remains 200 with doctor reader held open (${i}/5)"
    else
        e2e_fail "health returned ${status:-transport-error} with doctor reader held open (${i}/5)"
    fi
done

rpc_ok "doctor_live_tools_list" "JSON-RPC tools/list remains responsive" "health_check" "{}"
rpc_ok "doctor_live_fetch_inbox" "JSON-RPC mailbox read remains responsive" "fetch_inbox" \
    "{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"GreenStone\",\"limit\":5,\"include_bodies\":false}"

kill -CONT "${DOCTOR_PID}"
set +e
wait "${DOCTOR_PID}"
DOCTOR_RC=$?
set -e
DOCTOR_PID=""
if [ "${DOCTOR_RC}" -eq 0 ] && jq -e 'type == "object"' "${DOCTOR_OUT}" >/dev/null 2>&1; then
    e2e_pass "doctor check completes successfully after overlap"
else
    e2e_fail "doctor check failed after overlap (exit ${DOCTOR_RC})"
    printf '      doctor stderr: %s\n' "$(tail -8 "${DOCTOR_ERR}" 2>/dev/null | tr '\n' ' ' | head -c 700)"
fi

SERVER_LOG="${E2E_ARTIFACT_DIR}/logs/server_doctor_check_live.log"
if [ -f "${SERVER_LOG}" ] \
    && ! grep -Fq "database is busy (recovery in progress)" "${SERVER_LOG}" \
    && ! grep -F "mailbox mutation refused" "${SERVER_LOG}" | grep -Fq "doctor check"; then
    e2e_pass "server logged no doctor-induced recovery contention"
else
    e2e_fail "server logged doctor-induced recovery contention"
    tail -20 "${SERVER_LOG}" 2>/dev/null || true
fi

e2e_summary
