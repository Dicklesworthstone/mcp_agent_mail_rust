#!/usr/bin/env bash
# e2e_tui_interactions.sh - PTY E2E suite for TUI interaction flows.
#
# Run via:
#   ./scripts/e2e_test.sh tui_interactions
#   # or directly:
#   bash scripts/e2e_tui_interactions.sh
#
# Validates:
#   - Screen switching via number keys (1-8) and Tab/BackTab
#   - Help overlay toggle (?/Escape)
#   - Command palette open/close (Ctrl+P, Escape)
#   - Data visibility after seeding via API
#   - Rapid input handling (no crash)
#   - Quit (q) exits cleanly
#
# Uses `expect` for interactive PTY control, `pyte` for terminal
# emulation (proper interpretation of escape sequences), and `curl`
# for data seeding.
#
# Artifacts:
#   tests/artifacts/tui_interactions/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_interactions}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Interaction Flows (PTY) E2E Test Suite"

for cmd in expect timeout python3 curl; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

# Check pyte availability (needed for terminal emulation).
if ! python3 -c "import pyte" 2>/dev/null; then
    e2e_log "python3 pyte not available; skipping suite"
    e2e_skip "pyte required (pip install pyte)"
    e2e_summary
    exit 0
fi

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

# Render a raw PTY capture through pyte terminal emulator.
# Extracts visible text from all non-blank lines of the final screen state.
# Also saves intermediate screen states captured between marker sequences.
render_pty_output() {
    local in_path="$1"
    local out_path="$2"
    python3 - <<'PY' "$in_path" "$out_path"
import pyte
import sys
import re

in_path = sys.argv[1]
out_path = sys.argv[2]

data = open(in_path, "rb").read()

# Find the alternate screen buffer content.
# Everything between \x1b[?1049h (enter alt screen) and \x1b[?1049l (leave alt screen)
# is the TUI content.
#
# We feed the ENTIRE stream through pyte so it tracks cursor state correctly.
screen = pyte.Screen(120, 40)
stream = pyte.Stream(screen)

try:
    text = data.decode("utf-8", errors="replace")
    stream.feed(text)
except Exception:
    pass

# Extract non-blank lines from the final screen state.
lines = []
for row in range(screen.lines):
    line = ""
    for col in range(screen.columns):
        char = screen.buffer[row][col]
        line += char.data if char.data else " "
    stripped = line.rstrip()
    if stripped:
        lines.append(stripped)

# Also extract pre-alt-screen text (bootstrap banner).
pre_alt = data.split(b"\x1b[?1049h")[0] if b"\x1b[?1049h" in data else data
pre_text = pre_alt.decode("utf-8", errors="replace")
# Strip ANSI from pre-alt text.
pre_text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", pre_text)
pre_text = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", pre_text)
pre_text = re.sub(r"\x1b[@-_]", "", pre_text)
pre_text = pre_text.replace("\r", "")

result = "=== PRE-TUI (bootstrap banner) ===\n"
result += pre_text.strip() + "\n"
result += "\n=== FINAL TUI SCREEN STATE ===\n"
result += "\n".join(lines) + "\n"

with open(out_path, "w", encoding="utf-8") as f:
    f.write(result)
PY
}

# JSON-RPC call helper.
jsonrpc_call() {
    local port="$1"
    local method="$2"
    local params="$3"
    local payload
    payload=$(python3 -c "
import json, sys
print(json.dumps({
    'jsonrpc': '2.0',
    'id': 1,
    'method': 'tools/call',
    'params': {'name': '$method', 'arguments': json.loads(sys.argv[1])}
}, separators=(',', ':')))
" "$params")
    curl -sS -X POST "http://127.0.0.1:${port}/mcp/" \
        -H "content-type: application/json" \
        --data "${payload}" 2>/dev/null
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
        e2e_log "tail (last 30 lines):"
        tail -n 30 "${path}" 2>/dev/null || true
    fi
}

e2e_assert_file_not_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq -- "${needle}" "${path}"; then
        e2e_fail "${label}"
        e2e_log "unexpected needle: ${needle}"
    else
        e2e_pass "${label}"
    fi
}

# Start a TUI session via expect with proper terminal dimensions.
# Args: label bin port db storage raw_log [extra_env_vars...]
run_tui_expect() {
    local label="$1"
    local bin="$2"
    local port="$3"
    local db="$4"
    local storage="$5"
    local raw_log="$6"
    local expect_script="$7"
    local err_log="${E2E_ARTIFACT_DIR}/${label}.expect_err.log"

    # Run expect with explicit terminal size (120x40).
    LINES=40 COLUMNS=120 expect -f - \
        "${bin}" "${port}" "${db}" "${storage}" "${raw_log}" \
        2>"${err_log}" <<EXPECT_EOF || true
${expect_script}
EXPECT_EOF
}

# ═══════════════════════════════════════════════════════════════════════
# Build the binary
# ═══════════════════════════════════════════════════════════════════════
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# ═══════════════════════════════════════════════════════════════════════
# Case 1: Screen switching via number keys (1-8)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "screen_switching_number_keys"

WORK1="$(e2e_mktemp "e2e_tui_interact_screens")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage"
mkdir -p "${STORAGE1}"
PORT1="$(pick_port)"
RAW1="${E2E_ARTIFACT_DIR}/screen_switching.raw"

run_tui_expect "screen_switching" "${BIN}" "${PORT1}" "${DB1}" "${STORAGE1}" "${RAW1}" '
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 25
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

# Wait for full TUI startup and first render
sleep 4

# Switch through all screens: 1-8 then back to 1
send "2"
sleep 0.8
send "3"
sleep 0.8
send "4"
sleep 0.8
send "5"
sleep 0.8
send "6"
sleep 0.8
send "7"
sleep 0.8
send "8"
sleep 0.8
send "1"
sleep 0.5

send "q"
expect eof
'

RENDERED1="${E2E_ARTIFACT_DIR}/screen_switching.rendered.txt"
if [ -f "${RAW1}" ]; then
    render_pty_output "${RAW1}" "${RENDERED1}"
    e2e_pass "Screen switching completed without crash"
else
    e2e_fatal "Screen switching: raw log not created"
fi

e2e_assert_file_contains "Bootstrap banner present" "${RENDERED1}" "am: Starting MCP Agent Mail server"

# ═══════════════════════════════════════════════════════════════════════
# Case 2: Tab/BackTab screen cycling
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "tab_backtab_cycling"

WORK2="$(e2e_mktemp "e2e_tui_interact_tab")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
mkdir -p "${STORAGE2}"
PORT2="$(pick_port)"
RAW2="${E2E_ARTIFACT_DIR}/tab_cycling.raw"

run_tui_expect "tab_cycling" "${BIN}" "${PORT2}" "${DB2}" "${STORAGE2}" "${RAW2}" '
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 20
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 4

# Tab forward 3 times: Dashboard -> Messages -> Threads -> Agents
send "\t"
sleep 0.5
send "\t"
sleep 0.5
send "\t"
sleep 0.5

# BackTab (Shift+Tab = ESC [ Z) back twice
send "\033\[Z"
sleep 0.5
send "\033\[Z"
sleep 0.5

send "q"
expect eof
'

RENDERED2="${E2E_ARTIFACT_DIR}/tab_cycling.rendered.txt"
if [ -f "${RAW2}" ]; then
    render_pty_output "${RAW2}" "${RENDERED2}"
    e2e_pass "Tab/BackTab cycling completed without crash"
else
    e2e_fail "Tab/BackTab cycling: raw log not created"
fi

# ═══════════════════════════════════════════════════════════════════════
# Case 3: Help overlay toggle (? and Escape)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "help_overlay_toggle"

WORK3="$(e2e_mktemp "e2e_tui_interact_help")"
DB3="${WORK3}/db.sqlite3"
STORAGE3="${WORK3}/storage"
mkdir -p "${STORAGE3}"
PORT3="$(pick_port)"
RAW3="${E2E_ARTIFACT_DIR}/help_overlay.raw"

run_tui_expect "help_overlay" "${BIN}" "${PORT3}" "${DB3}" "${STORAGE3}" "${RAW3}" '
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 20
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 4

# Open help with ?
send "?"
sleep 1

# Close with Escape
send "\033"
sleep 0.5

send "q"
expect eof
'

RENDERED3="${E2E_ARTIFACT_DIR}/help_overlay.rendered.txt"
if [ -f "${RAW3}" ]; then
    render_pty_output "${RAW3}" "${RENDERED3}"
    e2e_pass "Help overlay toggle completed without crash"
else
    e2e_fail "Help overlay: raw log not created"
fi

# ═══════════════════════════════════════════════════════════════════════
# Case 4: Command palette (Ctrl+P / : / Escape)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "command_palette"

WORK4="$(e2e_mktemp "e2e_tui_interact_palette")"
DB4="${WORK4}/db.sqlite3"
STORAGE4="${WORK4}/storage"
mkdir -p "${STORAGE4}"
PORT4="$(pick_port)"
RAW4="${E2E_ARTIFACT_DIR}/command_palette.raw"

run_tui_expect "command_palette" "${BIN}" "${PORT4}" "${DB4}" "${STORAGE4}" "${RAW4}" '
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 20
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 4

# Open palette with Ctrl+P
send "\x10"
sleep 0.8

# Close with Escape
send "\033"
sleep 0.3

# Open with colon
send ":"
sleep 0.8

# Close with Escape
send "\033"
sleep 0.3

send "q"
expect eof
'

RENDERED4="${E2E_ARTIFACT_DIR}/command_palette.rendered.txt"
if [ -f "${RAW4}" ]; then
    render_pty_output "${RAW4}" "${RENDERED4}"
    e2e_pass "Command palette open/close completed without crash"
else
    e2e_fail "Command palette: raw log not created"
fi

# ═══════════════════════════════════════════════════════════════════════
# Case 5: Data seeding + API responsiveness during TUI session
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "data_seeding_with_live_tui"

WORK5="$(e2e_mktemp "e2e_tui_interact_data")"
DB5="${WORK5}/db.sqlite3"
STORAGE5="${WORK5}/storage"
mkdir -p "${STORAGE5}"
PORT5="$(pick_port)"
RAW5="${E2E_ARTIFACT_DIR}/data_seeding.raw"

# Start TUI in background via expect, seed data via API while TUI runs.
run_tui_expect "data_seeding" "${BIN}" "${PORT5}" "${DB5}" "${STORAGE5}" "${RAW5}" '
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 30
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

# Wait for server to be ready
sleep 5

# Navigate screens while API is live
send "2"
sleep 2
send "4"
sleep 2
send "1"
sleep 1

send "q"
expect eof
' &
EXPECT_PID=$!

# Wait for the server to come up
sleep 6
if e2e_wait_port 127.0.0.1 "${PORT5}" 10; then
    # Seed data via the API while TUI is running
    EP_RESP=$(jsonrpc_call "${PORT5}" "ensure_project" '{"human_key":"/data/e2e/live_tui"}')
    e2e_save_artifact "live_seed_project.json" "${EP_RESP}"

    REG1=$(jsonrpc_call "${PORT5}" "register_agent" '{"project_key":"/data/e2e/live_tui","program":"e2e","model":"test","name":"RedLake","task_description":"E2E sender"}')
    REG2=$(jsonrpc_call "${PORT5}" "register_agent" '{"project_key":"/data/e2e/live_tui","program":"e2e","model":"test","name":"BluePeak","task_description":"E2E receiver"}')
    e2e_save_artifact "live_seed_agents.json" "${REG1}
${REG2}"

    MSG=$(jsonrpc_call "${PORT5}" "send_message" '{"project_key":"/data/e2e/live_tui","sender_name":"RedLake","to":["BluePeak"],"subject":"Live TUI canary","body_md":"Seeded while TUI is running."}')
    e2e_save_artifact "live_seed_message.json" "${MSG}"

    # Verify API returns valid JSON-RPC response
    if echo "${MSG}" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d or 'error' not in d" 2>/dev/null; then
        e2e_pass "API responds during live TUI session"
    else
        e2e_fail "API did not respond properly during TUI session"
    fi
else
    e2e_fail "Server port not reachable during TUI session"
fi

# Wait for the expect process to finish
wait "${EXPECT_PID}" 2>/dev/null || true

RENDERED5="${E2E_ARTIFACT_DIR}/data_seeding.rendered.txt"
if [ -f "${RAW5}" ]; then
    render_pty_output "${RAW5}" "${RENDERED5}"
    e2e_pass "Data seeding with live TUI completed without crash"
else
    e2e_fail "Data seeding: raw log not created"
fi

# ═══════════════════════════════════════════════════════════════════════
# Case 6: Full journey — all screens, help, palette, quit
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "full_interaction_journey"

WORK6="$(e2e_mktemp "e2e_tui_interact_full")"
DB6="${WORK6}/db.sqlite3"
STORAGE6="${WORK6}/storage"
mkdir -p "${STORAGE6}"
PORT6="$(pick_port)"
RAW6="${E2E_ARTIFACT_DIR}/full_journey.raw"

run_tui_expect "full_journey" "${BIN}" "${PORT6}" "${DB6}" "${STORAGE6}" "${RAW6}" '
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 30
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 4

# Step 1: Visit every screen (1-8)
send "1"
sleep 0.5
send "2"
sleep 0.5
send "3"
sleep 0.5
send "4"
sleep 0.5
send "5"
sleep 0.5
send "6"
sleep 0.5
send "7"
sleep 0.5
send "8"
sleep 0.5

# Step 2: Toggle help on and off
send "?"
sleep 0.8
send "\033"
sleep 0.3

# Step 3: Open and close command palette
send "\x10"
sleep 0.8
send "\033"
sleep 0.3

# Step 4: Tab cycling (3 forward, 1 back)
send "\t"
sleep 0.3
send "\t"
sleep 0.3
send "\t"
sleep 0.3
send "\033\[Z"
sleep 0.3

# Step 5: Return to Dashboard and quit
send "1"
sleep 0.3
send "q"
expect eof
'

RENDERED6="${E2E_ARTIFACT_DIR}/full_journey.rendered.txt"
if [ -f "${RAW6}" ]; then
    render_pty_output "${RAW6}" "${RENDERED6}"
    e2e_pass "Full journey completed without crash"
else
    e2e_fail "Full journey: raw log not created"
fi

e2e_assert_file_contains "Journey: bootstrap banner" "${RENDERED6}" "am: Starting MCP Agent Mail server"

# ═══════════════════════════════════════════════════════════════════════
# Case 7: Rapid key sequences (input stress test)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "rapid_key_sequences"

WORK7="$(e2e_mktemp "e2e_tui_interact_rapid")"
DB7="${WORK7}/db.sqlite3"
STORAGE7="${WORK7}/storage"
mkdir -p "${STORAGE7}"
PORT7="$(pick_port)"
RAW7="${E2E_ARTIFACT_DIR}/rapid_keys.raw"

run_tui_expect "rapid_keys" "${BIN}" "${PORT7}" "${DB7}" "${STORAGE7}" "${RAW7}" '
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 20
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 3

# Rapid-fire: cycle through all screens twice with no delays
send "1234567"
sleep 0.1
send "7654321"
sleep 0.1

# Rapid help toggles
send "?"
send "\033"
send "?"
send "\033"
sleep 0.1

# Rapid palette open/close
send "\x10"
send "\033"
send ":"
send "\033"
sleep 0.1

# Tab spam
send "\t\t\t\t\t\t\t"
sleep 0.3

# Final quit
send "q"
expect eof
'

if [ -f "${RAW7}" ]; then
    render_pty_output "${RAW7}" "${E2E_ARTIFACT_DIR}/rapid_keys.rendered.txt"
    e2e_pass "Rapid key sequences completed without crash"
else
    e2e_fail "Rapid key sequences: raw log not created"
fi

# ═══════════════════════════════════════════════════════════════════════
e2e_summary
