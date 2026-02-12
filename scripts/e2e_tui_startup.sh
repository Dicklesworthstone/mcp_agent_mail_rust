#!/usr/bin/env bash
# e2e_tui_startup.sh - PTY E2E suite for zero-friction TUI startup contract.
#
# Run via:
#   ./scripts/e2e_test.sh tui_startup
#   bash scripts/e2e_tui_startup.sh --showcase
#
# Validates:
#   - `mcp-agent-mail serve` starts server+TUI and reaches ready state.
#   - Startup bootstrap banner shows resolved config and sources.
#   - Bearer token auto-discovered from user env file.
#   - Both MCP and API mode bootstraps work.
#   - Token masking: raw secrets never appear in output.
#   - Missing/invalid config produces actionable remediation.
#
# Artifacts:
#   tests/artifacts/tui_startup/<timestamp>/*
#   tests/artifacts/tui_showcase/<timestamp>/*  (when --showcase)

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHOWCASE_MODE=0

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    cat <<'EOF'
Usage:
  bash scripts/e2e_tui_startup.sh
  bash scripts/e2e_tui_startup.sh --showcase

Modes:
  default     Run startup contract assertions (existing tui_startup suite).
  --showcase  Run deterministic demo orchestration across startup, search,
              interactions, security/redaction, macro tools/playback, and
              cross-terminal compatibility.
EOF
    exit 0
fi

if [ "${1:-}" = "--showcase" ]; then
    SHOWCASE_MODE=1
    shift
fi

if [ "${SHOWCASE_MODE}" -eq 1 ]; then
    E2E_SUITE="${E2E_SUITE:-tui_showcase}"
else
    E2E_SUITE="${E2E_SUITE:-tui_startup}"
fi

# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
if [ "${SHOWCASE_MODE}" -eq 1 ]; then
    e2e_banner "TUI Showcase Demo (Deterministic) E2E Orchestration"
else
    e2e_banner "TUI Startup (PTY) E2E Test Suite"
fi

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

e2e_fatal() {
    local msg="$1"
    e2e_fail "${msg}"
    e2e_summary || true
    exit 1
}

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

e2e_assert_file_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq -- "${needle}" "${path}"; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        e2e_log "missing needle: ${needle}"
        e2e_log "in file: ${path}"
        e2e_log "tail (last 80 lines):"
        tail -n 80 "${path}" 2>/dev/null || true
    fi
}

e2e_assert_file_not_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq -- "${needle}" "${path}"; then
        e2e_fail "${label}"
        e2e_log "unexpected needle: ${needle}"
        e2e_log "in file: ${path}"
        e2e_log "matches:"
        grep -Fn -- "${needle}" "${path}" | head -n 10 || true
    else
        e2e_pass "${label}"
    fi
}

normalize_transcript() {
    local in_path="$1"
    local out_path="$2"
    python3 - <<'PY' "$in_path" "$out_path"
import re
import sys

in_path = sys.argv[1]
out_path = sys.argv[2]

data = open(in_path, "rb").read()

# Strip OSC sequences (BEL or ST terminator).
data = re.sub(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", b"", data)
# Strip CSI sequences (colors + cursor movement).
data = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data)
# Strip single-character ESC sequences (best-effort).
data = re.sub(rb"\x1b[@-_]", b"", data)

text = data.decode("utf-8", errors="replace")

# Remove util-linux `script` wrapper lines for stable assertions.
lines = []
for line in text.splitlines():
    if line.startswith("Script started on "):
        continue
    if line.startswith("Script done on "):
        continue
    lines.append(line)
text = "\n".join(lines) + "\n"

with open(out_path, "w", encoding="utf-8") as f:
    f.write(text)
PY
}

start_server_pty() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5

    local typescript="${E2E_ARTIFACT_DIR}/server_${label}.typescript"
    e2e_log "Starting PTY server (${label}): 127.0.0.1:${port}"
    e2e_log "  typescript: ${typescript}"

    local timeout_s="${AM_E2E_SERVER_TIMEOUT_S:-15}"

    (
        script -q -f -c "env \
DATABASE_URL=sqlite:////${db_path} \
STORAGE_ROOT=${storage_root} \
HTTP_HOST=127.0.0.1 \
HTTP_PORT=${port} \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_JWT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
${*} \
timeout ${timeout_s}s ${bin} serve --host 127.0.0.1 --port ${port}" \
            "${typescript}"
    ) >/dev/null 2>&1 &

    echo $!
}

# Headless mode (--no-tui) captures stderr directly (no PTY needed).
start_server_headless() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5

    local logfile="${E2E_ARTIFACT_DIR}/server_${label}.log"
    e2e_log "Starting headless server (${label}): 127.0.0.1:${port}"

    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"
        export HTTP_RBAC_ENABLED=0
        export HTTP_RATE_LIMIT_ENABLED=0
        export HTTP_JWT_ENABLED=0
        export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1
        while [ $# -gt 0 ]; do
            export "$1"
            shift
        done
        timeout 15s "${bin}" serve --host 127.0.0.1 --port "${port}" --no-tui
    ) >"${logfile}" 2>&1 &

    echo $!
}

stop_server() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

showcase_assert_file_exists() {
    local label="$1"
    local path="$2"
    if [ -f "${path}" ]; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        e2e_log "missing file: ${path}"
    fi
}

showcase_assert_summary_green() {
    local label="$1"
    local summary_path="$2"
    if [ ! -f "${summary_path}" ]; then
        e2e_fail "${label}"
        e2e_log "missing summary: ${summary_path}"
        return
    fi

    if python3 - "${summary_path}" <<'PY'
import json
import sys

summary_path = sys.argv[1]
with open(summary_path, "r", encoding="utf-8") as f:
    summary = json.load(f)

fail = int(summary.get("fail", 0))
skip = int(summary.get("skip", 0))
if fail != 0:
    raise SystemExit(1)
print(f"fail={fail} skip={skip}")
PY
    then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
    fi
}

showcase_require_prereqs() {
    local missing=0
    for cmd in script timeout python3 curl tmux expect cargo; do
        if ! command -v "${cmd}" >/dev/null 2>&1; then
            e2e_fail "showcase prerequisite missing: ${cmd}"
            missing=1
        else
            e2e_pass "showcase prerequisite available: ${cmd}"
        fi
    done

    if python3 -c "import pyte" >/dev/null 2>&1; then
        e2e_pass "showcase prerequisite available: python3 module pyte"
    else
        e2e_fail "showcase prerequisite missing: python3 module pyte"
        missing=1
    fi

    return "${missing}"
}

showcase_verify_suite_artifacts() {
    local suite="$1"
    local suite_dir="$2"
    local summary="${suite_dir}/summary.json"

    showcase_assert_file_exists "${suite}: summary.json exists" "${summary}"
    showcase_assert_file_exists "${suite}: bundle.json exists" "${suite_dir}/bundle.json"
    showcase_assert_summary_green "${suite}: summary reports zero failures" "${summary}"

    case "${suite}" in
        tui_startup)
            showcase_assert_file_exists "${suite}: PTY transcript exists" "${suite_dir}/server_tui_ready.normalized.txt"
            e2e_assert_file_contains "${suite}: masked token shown" "${suite_dir}/server_token_auto.log" "****"
            e2e_assert_file_not_contains "${suite}: raw token redacted" "${suite_dir}/server_token_auto.log" "test-secret-token-e2e-12345"
            ;;
        search_cockpit)
            showcase_assert_file_exists "${suite}: keyword search artifact exists" "${suite_dir}/case_01_keyword.txt"
            showcase_assert_file_exists "${suite}: boolean search artifact exists" "${suite_dir}/case_04_boolean.txt"
            showcase_assert_file_exists "${suite}: thread summary artifact exists" "${suite_dir}/case_07_thread_summary.txt"
            ;;
        tui_interactions)
            showcase_assert_file_exists "${suite}: analytics rendered transcript exists" "${suite_dir}/analytics_widgets.rendered.txt"
            showcase_assert_file_exists "${suite}: analytics action trace exists" "${suite_dir}/trace/analytics_widgets_timeline.tsv"
            e2e_assert_file_contains "${suite}: action trace has ToolMetrics step" "${suite_dir}/trace/analytics_widgets_timeline.tsv" "ToolMetrics"
            ;;
        security_privacy)
            showcase_assert_file_exists "${suite}: hostile markdown artifact exists" "${suite_dir}/case_06_hostile_md.txt"
            showcase_assert_file_exists "${suite}: secret body artifact exists" "${suite_dir}/case_09_secret_body.txt"
            showcase_assert_file_exists "${suite}: search scope artifact exists" "${suite_dir}/case_01_search_scope.txt"
            ;;
        macros)
            showcase_assert_file_exists "${suite}: start session artifact exists" "${suite_dir}/case_01_start_session.txt"
            showcase_assert_file_exists "${suite}: reservation cycle artifact exists" "${suite_dir}/case_02_reservation_cycle.txt"
            showcase_assert_file_exists "${suite}: slot conflict artifact exists" "${suite_dir}/case_05_slot_conflict.txt"
            ;;
        tui_compat_matrix)
            showcase_assert_file_exists "${suite}: tmux layout trace exists" "${suite_dir}/profiles/tmux_screen_resize_matrix/layout_trace.tsv"
            showcase_assert_file_exists "${suite}: tmux layout trace json exists" "${suite_dir}/profiles/tmux_screen_resize_matrix/layout_trace.json"
            e2e_assert_file_contains "${suite}: matrix includes tool metrics screen capture" "${suite_dir}/profiles/tmux_screen_resize_matrix/layout_trace.tsv" "tool_metrics"
            ;;
    esac
}

showcase_run_suite() {
    local suite="$1"
    local reason="$2"
    local log_path="${E2E_ARTIFACT_DIR}/showcase/logs/${suite}.log"
    local suite_dir="${E2E_PROJECT_ROOT}/tests/artifacts/${suite}/${SHOWCASE_TIMESTAMP}"
    local rc=0

    e2e_case_banner "showcase_suite_${suite}"
    e2e_log "Running suite ${suite}: ${reason}"

    (
        cd "${E2E_PROJECT_ROOT}"
        AM_E2E_KEEP_TMP=1 \
        E2E_CLOCK_MODE="${SHOWCASE_CLOCK_MODE}" \
        E2E_SEED="${SHOWCASE_SEED}" \
        E2E_TIMESTAMP="${SHOWCASE_TIMESTAMP}" \
        E2E_RUN_STARTED_AT="${SHOWCASE_RUN_STARTED_AT}" \
        E2E_RUN_START_EPOCH_S="${SHOWCASE_RUN_START_EPOCH_S}" \
        bash "./scripts/e2e_test.sh" "${suite}"
    ) >"${log_path}" 2>&1 || rc=$?

    if [ "${rc}" -eq 0 ]; then
        e2e_pass "${suite}: suite command exited 0"
    else
        e2e_fail "${suite}: suite command failed (rc=${rc})"
        e2e_log "suite log: ${log_path}"
    fi

    if [ -d "${suite_dir}" ]; then
        e2e_pass "${suite}: artifact directory created"
    else
        e2e_fail "${suite}: artifact directory missing"
        e2e_log "expected artifact directory: ${suite_dir}"
    fi

    showcase_verify_suite_artifacts "${suite}" "${suite_dir}"
    printf "%s\t%s\t%s\t%s\n" "${suite}" "${rc}" "${suite_dir}" "${log_path}" >> "${SHOWCASE_INDEX_TSV}"
}

showcase_find_latest_macro_playback_dir() {
    local pattern="$1"
    find "${E2E_PROJECT_ROOT}/tests/artifacts/tui/macro_replay" \
        -mindepth 1 -maxdepth 1 -type d -name "${pattern}" 2>/dev/null | sort | tail -n 1
}

showcase_run_macro_playback_forensics() {
    local replay_root="${E2E_PROJECT_ROOT}/tests/artifacts/tui/macro_replay"
    local log_path="${E2E_ARTIFACT_DIR}/showcase/logs/macro_playback_forensics.log"
    local rc=0

    e2e_case_banner "showcase_macro_playback_forensics"
    mkdir -p "${replay_root}"

    local before_count
    before_count="$(find "${replay_root}" -mindepth 1 -maxdepth 1 -type d -name '*_record_save_load_replay' 2>/dev/null | wc -l | tr -d '[:space:]')"

    (
        cd "${E2E_PROJECT_ROOT}"
        cargo test -p mcp-agent-mail-server operator_macro_record_save_load_replay_forensics -- --nocapture
    ) >"${log_path}" 2>&1 || rc=$?

    if [ "${rc}" -eq 0 ]; then
        e2e_pass "macro playback forensics test exits 0"
    else
        e2e_fail "macro playback forensics test failed (rc=${rc})"
        e2e_log "macro playback log: ${log_path}"
    fi

    local after_count
    after_count="$(find "${replay_root}" -mindepth 1 -maxdepth 1 -type d -name '*_record_save_load_replay' 2>/dev/null | wc -l | tr -d '[:space:]')"
    if [ "${after_count}" -gt "${before_count}" ]; then
        e2e_pass "macro playback created a new replay artifact directory"
    else
        e2e_skip "macro playback directory count unchanged; reusing latest artifact"
    fi

    SHOWCASE_MACRO_REPLAY_DIR="$(showcase_find_latest_macro_playback_dir '*_record_save_load_replay')"
    if [ -n "${SHOWCASE_MACRO_REPLAY_DIR}" ] && [ -d "${SHOWCASE_MACRO_REPLAY_DIR}" ]; then
        e2e_pass "macro playback artifact directory resolved"
        showcase_assert_file_exists "macro playback report exists" "${SHOWCASE_MACRO_REPLAY_DIR}/report.json"
        showcase_assert_file_exists "macro playback recorded steps exist" "${SHOWCASE_MACRO_REPLAY_DIR}/steps/step_0001_record.json"
        showcase_assert_file_exists "macro playback replay steps exist" "${SHOWCASE_MACRO_REPLAY_DIR}/steps/step_0001_play.json"
        e2e_copy_artifact "${SHOWCASE_MACRO_REPLAY_DIR}/report.json" "showcase/macro_playback/report.json"
        e2e_copy_artifact "${SHOWCASE_MACRO_REPLAY_DIR}/steps" "showcase/macro_playback/steps"
    else
        e2e_fail "macro playback artifact directory not found"
    fi

    printf "%s\t%s\t%s\t%s\n" \
        "macro_playback_forensics" \
        "${rc}" \
        "${SHOWCASE_MACRO_REPLAY_DIR:-<missing>}" \
        "${log_path}" >> "${SHOWCASE_INDEX_TSV}"
}

showcase_write_manifest() {
    local manifest="${E2E_ARTIFACT_DIR}/showcase/manifest.json"
    python3 - "${SHOWCASE_INDEX_TSV}" "${manifest}" "${SHOWCASE_REPRO_COMMAND}" "${SHOWCASE_TIMESTAMP}" "${SHOWCASE_SEED}" "${SHOWCASE_CLOCK_MODE}" <<'PY'
import csv
import json
import pathlib
import sys

index_path = pathlib.Path(sys.argv[1])
manifest_path = pathlib.Path(sys.argv[2])
repro_cmd = sys.argv[3]
timestamp = sys.argv[4]
seed = sys.argv[5]
clock_mode = sys.argv[6]

rows = []
with index_path.open("r", encoding="utf-8") as f:
    reader = csv.DictReader(f, delimiter="\t")
    for row in reader:
        rows.append(row)

status = "pass"
for row in rows:
    if row.get("rc", "0") != "0":
        status = "fail"
        break

manifest = {
    "schema": "tui_showcase.v1",
    "status": status,
    "timestamp": timestamp,
    "clock_mode": clock_mode,
    "seed": seed,
    "repro_command": repro_cmd,
    "stages": rows,
}

manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True), encoding="utf-8")
PY
    showcase_assert_file_exists "showcase manifest written" "${manifest}"
}

run_showcase() {
    SHOWCASE_CLOCK_MODE="${AM_TUI_SHOWCASE_CLOCK_MODE:-deterministic}"
    SHOWCASE_SEED="${AM_TUI_SHOWCASE_SEED:-${E2E_SEED}}"
    SHOWCASE_TIMESTAMP="${AM_TUI_SHOWCASE_TIMESTAMP:-${E2E_TIMESTAMP}}"
    SHOWCASE_RUN_STARTED_AT="${AM_TUI_SHOWCASE_RUN_STARTED_AT:-${E2E_RUN_STARTED_AT}}"
    SHOWCASE_RUN_START_EPOCH_S="${AM_TUI_SHOWCASE_RUN_START_EPOCH_S:-${E2E_RUN_START_EPOCH_S}}"
    SHOWCASE_SUITES="${AM_TUI_SHOWCASE_SUITES:-tui_startup,search_cockpit,tui_interactions,security_privacy,macros,tui_compat_matrix}"
    SHOWCASE_INDEX_TSV="${E2E_ARTIFACT_DIR}/showcase/index.tsv"
    SHOWCASE_MACRO_REPLAY_DIR=""

    e2e_case_banner "showcase_reset_setup"
    mkdir -p "${E2E_ARTIFACT_DIR}/showcase/logs"
    {
        echo "SHOWCASE_CLOCK_MODE=${SHOWCASE_CLOCK_MODE}"
        echo "SHOWCASE_SEED=${SHOWCASE_SEED}"
        echo "SHOWCASE_TIMESTAMP=${SHOWCASE_TIMESTAMP}"
        echo "SHOWCASE_RUN_STARTED_AT=${SHOWCASE_RUN_STARTED_AT}"
        echo "SHOWCASE_RUN_START_EPOCH_S=${SHOWCASE_RUN_START_EPOCH_S}"
        echo "SHOWCASE_SUITES=${SHOWCASE_SUITES}"
    } > "${E2E_ARTIFACT_DIR}/showcase/reset_setup.env"
    printf "suite\trc\tartifact_dir\tlog_path\n" > "${SHOWCASE_INDEX_TSV}"
    e2e_pass "showcase reset/setup context captured"

    e2e_case_banner "showcase_prerequisites"
    if ! showcase_require_prereqs; then
        e2e_fatal "showcase prerequisites missing; install required commands/modules"
    fi

    for suite in ${SHOWCASE_SUITES//,/ }; do
        case "${suite}" in
            tui_startup)
                showcase_run_suite "${suite}" "bootstrap banner + token redaction sanity"
                ;;
            search_cockpit)
                showcase_run_suite "${suite}" "search explorer deterministic query corpus"
                ;;
            tui_interactions)
                showcase_run_suite "${suite}" "explorer + analytics + widgets seeded interaction flow"
                ;;
            security_privacy)
                showcase_run_suite "${suite}" "security/redaction/privacy behavior validation"
                ;;
            macros)
                showcase_run_suite "${suite}" "macro helper workflows + build slot lifecycle"
                ;;
            tui_compat_matrix)
                showcase_run_suite "${suite}" "cross-terminal profiles with resize/unicode matrix"
                ;;
            *)
                e2e_fail "unknown showcase suite: ${suite}"
                printf "%s\t%s\t%s\t%s\n" "${suite}" "1" "<unknown>" "<none>" >> "${SHOWCASE_INDEX_TSV}"
                ;;
        esac
    done

    showcase_run_macro_playback_forensics

    e2e_case_banner "showcase_teardown_handoff"
    SHOWCASE_REPRO_COMMAND="cd ${E2E_PROJECT_ROOT} && AM_E2E_KEEP_TMP=1 E2E_CLOCK_MODE=${SHOWCASE_CLOCK_MODE} E2E_SEED=${SHOWCASE_SEED} E2E_TIMESTAMP=${SHOWCASE_TIMESTAMP} E2E_RUN_STARTED_AT=${SHOWCASE_RUN_STARTED_AT} E2E_RUN_START_EPOCH_S=${SHOWCASE_RUN_START_EPOCH_S} bash scripts/e2e_tui_startup.sh --showcase"
    e2e_save_artifact "showcase/repro_command.txt" "${SHOWCASE_REPRO_COMMAND}"
    e2e_save_artifact "showcase/teardown.txt" "Teardown is no-op by design. Artifacts are intentionally retained for handoff review under tests/artifacts."
    showcase_write_manifest
    e2e_pass "showcase handoff artifacts generated"
}

if [ "${SHOWCASE_MODE}" -eq 1 ]; then
    run_showcase
    e2e_summary
    if [ "${_E2E_FAIL}" -gt 0 ]; then
        exit 1
    fi
    exit 0
fi

for cmd in script timeout python3 curl; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

TOOLS_LIST_PAYLOAD='{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}'

tools_list_call() {
    local case_id="$1"
    local url="$2"
    shift 2
    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call_raw "${case_id}" "${url}" "${TOOLS_LIST_PAYLOAD}" "$@"; then
        :
    fi
}

# ────────────────────────────────────────────────────────────────────
# Case 1: Default startup shows bootstrap banner (headless for easy capture)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "bootstrap_banner_shows_config_sources"
WORK1="$(e2e_mktemp "e2e_tui_startup_banner")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage"
mkdir -p "${STORAGE1}"
PORT1="$(pick_port)"

PID1="$(start_server_headless "banner" "${PORT1}" "${DB1}" "${STORAGE1}" "${BIN}")"
if ! e2e_wait_port 127.0.0.1 "${PORT1}" 10; then
    stop_server "${PID1}"
    e2e_fatal "server failed to start (port not open)"
fi
sleep 0.3
stop_server "${PID1}"
sleep 0.3

LOG1="${E2E_ARTIFACT_DIR}/server_banner.log"
e2e_assert_file_contains "banner title present" "${LOG1}" "am: Starting MCP Agent Mail server"
e2e_assert_file_contains "host line present" "${LOG1}" "host:"
e2e_assert_file_contains "port line present" "${LOG1}" "port:"
e2e_assert_file_contains "path line present" "${LOG1}" "path:"
e2e_assert_file_contains "auth line present" "${LOG1}" "auth:"
e2e_assert_file_contains "db line present" "${LOG1}" "db:"
e2e_assert_file_contains "storage line present" "${LOG1}" "storage:"
e2e_assert_file_contains "mode line present" "${LOG1}" "mode:"
e2e_assert_file_contains "headless mode shown" "${LOG1}" "HTTP (headless)"
e2e_assert_file_contains "port shows correct value" "${LOG1}" "${PORT1}"

# ────────────────────────────────────────────────────────────────────
# Case 2: PTY mode reaches ready state (server+TUI startup)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "pty_tui_reaches_ready_state"
WORK2="$(e2e_mktemp "e2e_tui_startup_pty")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
mkdir -p "${STORAGE2}"
PORT2="$(pick_port)"

PID2="$(start_server_pty "tui_ready" "${PORT2}" "${DB2}" "${STORAGE2}" "${BIN}" "LOG_RICH_ENABLED=true")"
if ! e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    stop_server "${PID2}"
    e2e_fatal "TUI server failed to reach ready state (port not open after 10s)"
fi

# Verify the server responds to MCP tools/list
tools_list_call "case2_tools_list" "http://127.0.0.1:${PORT2}/mcp/"
TOOLS_LIST="$(e2e_rpc_read_response "case2_tools_list")"
TOOLS_LIST_STATUS="$(e2e_rpc_read_status "case2_tools_list")"
CURL_RC=1
if [ "${TOOLS_LIST_STATUS}" = "200" ]; then
    CURL_RC=0
fi
e2e_save_artifact "tui_ready_tools_list.json" "${TOOLS_LIST:-<empty>}"

if [ "$CURL_RC" -eq 0 ] && echo "${TOOLS_LIST}" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    e2e_pass "server responds to tools/list via /mcp/"
else
    e2e_fail "server did not respond to tools/list"
fi

stop_server "${PID2}"
sleep 0.3

NORM2="${E2E_ARTIFACT_DIR}/server_tui_ready.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/server_tui_ready.typescript" "${NORM2}"
e2e_assert_file_contains "bootstrap banner in PTY" "${NORM2}" "am: Starting MCP Agent Mail server"

# ────────────────────────────────────────────────────────────────────
# Case 3: API mode bootstrap works
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "api_mode_bootstrap"
WORK3="$(e2e_mktemp "e2e_tui_startup_api")"
DB3="${WORK3}/db.sqlite3"
STORAGE3="${WORK3}/storage"
mkdir -p "${STORAGE3}"
PORT3="$(pick_port)"

PID3="$(start_server_headless "api_mode" "${PORT3}" "${DB3}" "${STORAGE3}" "${BIN}" "HTTP_PATH=/api/")"
if ! e2e_wait_port 127.0.0.1 "${PORT3}" 10; then
    stop_server "${PID3}"
    e2e_fatal "API mode server failed to start"
fi

# Verify API path responds
tools_list_call "case3_api_tools_list" "http://127.0.0.1:${PORT3}/api/"
API_RESP="$(e2e_rpc_read_response "case3_api_tools_list")"
API_STATUS="$(e2e_rpc_read_status "case3_api_tools_list")"
API_RC=1
if [ "${API_STATUS}" = "200" ]; then
    API_RC=0
fi
e2e_save_artifact "api_mode_tools_list.json" "${API_RESP:-<empty>}"

if [ "$API_RC" -eq 0 ] && echo "${API_RESP}" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    e2e_pass "API mode responds to tools/list via /api/"
else
    e2e_fail "API mode did not respond to tools/list"
fi

stop_server "${PID3}"
sleep 0.3

LOG3="${E2E_ARTIFACT_DIR}/server_api_mode.log"
e2e_assert_file_contains "API mode banner shows /api/" "${LOG3}" "/api/"

# ────────────────────────────────────────────────────────────────────
# Case 4: Bearer token auto-discovery from user env file
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "bearer_token_auto_discovery"
WORK4="$(e2e_mktemp "e2e_tui_startup_token")"
DB4="${WORK4}/db.sqlite3"
STORAGE4="${WORK4}/storage"
mkdir -p "${STORAGE4}"
PORT4="$(pick_port)"

# Create a fake user env file with a bearer token
USER_ENV_DIR="${WORK4}/.mcp_agent_mail"
mkdir -p "${USER_ENV_DIR}"
echo 'HTTP_BEARER_TOKEN=test-secret-token-e2e-12345' > "${USER_ENV_DIR}/.env"

# Start server with HOME pointing to our temp dir (so it finds our .env)
PID4="$(start_server_headless "token_auto" "${PORT4}" "${DB4}" "${STORAGE4}" "${BIN}" "HOME=${WORK4}" "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0")"
if ! e2e_wait_port 127.0.0.1 "${PORT4}" 10; then
    stop_server "${PID4}"
    e2e_fatal "token auto-discovery server failed to start"
fi

# Verify unauthenticated request is rejected
tools_list_call "case4_unauth_tools_list" "http://127.0.0.1:${PORT4}/mcp/"
UNAUTH_RESP="$(e2e_rpc_read_status "case4_unauth_tools_list")"
if [ "${UNAUTH_RESP}" = "401" ] || [ "${UNAUTH_RESP}" = "403" ]; then
    e2e_pass "unauthenticated request rejected (${UNAUTH_RESP})"
else
    e2e_fail "expected 401/403 for unauthenticated, got ${UNAUTH_RESP}"
fi

# Verify authenticated request succeeds
tools_list_call "case4_auth_tools_list" "http://127.0.0.1:${PORT4}/mcp/" \
    "Authorization: Bearer test-secret-token-e2e-12345"
AUTH_RESP="$(e2e_rpc_read_response "case4_auth_tools_list")"
AUTH_STATUS="$(e2e_rpc_read_status "case4_auth_tools_list")"
AUTH_RC=1
if [ "${AUTH_STATUS}" = "200" ]; then
    AUTH_RC=0
fi
e2e_save_artifact "token_auth_tools_list.json" "${AUTH_RESP:-<empty>}"

if [ "$AUTH_RC" -eq 0 ] && echo "${AUTH_RESP}" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    e2e_pass "authenticated request with auto-discovered token succeeds"
else
    e2e_fail "authenticated request failed"
fi

stop_server "${PID4}"
sleep 0.3

LOG4="${E2E_ARTIFACT_DIR}/server_token_auto.log"
# Verify token is masked in bootstrap banner (raw token never shown)
e2e_assert_file_not_contains "raw token not in output" "${LOG4}" "test-secret-token-e2e-12345"
e2e_assert_file_contains "masked token shown" "${LOG4}" "****"
e2e_assert_file_contains "token source shown" "${LOG4}" ".mcp_agent_mail/.env"

# ────────────────────────────────────────────────────────────────────
# Case 5: MCP mode default (no explicit path)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "mcp_default_path"
WORK5="$(e2e_mktemp "e2e_tui_startup_mcp_default")"
DB5="${WORK5}/db.sqlite3"
STORAGE5="${WORK5}/storage"
mkdir -p "${STORAGE5}"
PORT5="$(pick_port)"

PID5="$(start_server_headless "mcp_default" "${PORT5}" "${DB5}" "${STORAGE5}" "${BIN}")"
if ! e2e_wait_port 127.0.0.1 "${PORT5}" 10; then
    stop_server "${PID5}"
    e2e_fatal "MCP default server failed to start"
fi
sleep 0.3
stop_server "${PID5}"
sleep 0.3

LOG5="${E2E_ARTIFACT_DIR}/server_mcp_default.log"
e2e_assert_file_contains "default path is /mcp/" "${LOG5}" "/mcp/"
e2e_assert_file_contains "default source shown" "${LOG5}" "(default)"

# ────────────────────────────────────────────────────────────────────
# Case 6: Clean shell (no pre-exported vars) startup
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "clean_shell_startup"
WORK6="$(e2e_mktemp "e2e_tui_startup_clean")"
DB6="${WORK6}/db.sqlite3"
STORAGE6="${WORK6}/storage"
mkdir -p "${STORAGE6}"
PORT6="$(pick_port)"

# Use env -i to strip all environment variables, providing only essentials
LOG6="${E2E_ARTIFACT_DIR}/server_clean_shell.log"
(
    env -i \
        PATH="${PATH}" \
        HOME="${WORK6}" \
        DATABASE_URL="sqlite:////${DB6}" \
        STORAGE_ROOT="${STORAGE6}" \
        HTTP_HOST="127.0.0.1" \
        HTTP_PORT="${PORT6}" \
        HTTP_RBAC_ENABLED=0 \
        HTTP_RATE_LIMIT_ENABLED=0 \
        HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
        timeout 15s "${BIN}" serve --host 127.0.0.1 --port "${PORT6}" --no-tui
) >"${LOG6}" 2>&1 &
PID6=$!

if ! e2e_wait_port 127.0.0.1 "${PORT6}" 10; then
    stop_server "${PID6}"
    e2e_fatal "clean shell server failed to start"
fi
sleep 0.3
stop_server "${PID6}"
sleep 0.3

e2e_assert_file_contains "clean shell: banner present" "${LOG6}" "am: Starting MCP Agent Mail server"
e2e_assert_file_contains "clean shell: no auth shown" "${LOG6}" "none"

# ────────────────────────────────────────────────────────────────────
e2e_summary
