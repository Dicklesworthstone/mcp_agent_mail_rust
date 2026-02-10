#!/usr/bin/env bash
# e2e_tui_a11y.sh - Accessibility + keyboard-only E2E suite (br-3vwi.10.13)
#
# Run via:
#   ./scripts/e2e_test.sh tui_a11y
#
# Validates:
# - Keyboard-only navigation across key TUI surfaces
# - Key hint bar can be toggled on/off via palette
# - Theme palette contrast thresholds (logged via --nocapture)
#
# Artifacts:
#   tests/artifacts/tui_a11y/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_a11y}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Accessibility (Keyboard + Contrast) E2E Test Suite"

for cmd in expect timeout python3 curl; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

# Check pyte availability (needed for terminal emulation of PTY logs).
if ! python3 -c "import pyte" 2>/dev/null; then
    e2e_log "python3 pyte not available; skipping suite"
    e2e_skip "pyte required (pip install pyte)"
    e2e_summary
    exit 0
fi

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

# Render a raw PTY capture through pyte terminal emulator.
# Extracts visible text from all non-blank lines of the final screen state.
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

screen = pyte.Screen(120, 40)
stream = pyte.Stream(screen)

try:
    text = data.decode("utf-8", errors="replace")
    stream.feed(text)
except Exception:
    pass

lines = []
for row in range(screen.lines):
    line = ""
    for col in range(screen.columns):
        char = screen.buffer[row][col]
        line += char.data if char.data else " "
    stripped = line.rstrip()
    if stripped:
        lines.append(stripped)

pre_alt = data.split(b"\x1b[?1049h")[0] if b"\x1b[?1049h" in data else data
pre_text = pre_alt.decode("utf-8", errors="replace")
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

strip_ansi_stream() {
    local in_path="$1"
    local out_path="$2"
    python3 - <<'PY' "$in_path" "$out_path"
import re
import sys

in_path = sys.argv[1]
out_path = sys.argv[2]

data = open(in_path, "rb").read().decode("utf-8", errors="replace")

# Strip common ANSI escape sequences.
data = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", data)
data = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", data)
data = re.sub(r"\x1b[@-_]", "", data)
data = data.replace("\r", "")

with open(out_path, "w", encoding="utf-8") as f:
    f.write(data)
PY
}

extract_focus_trace_from_raw() {
    local raw_path="$1"
    local out_path="$2"
    python3 - <<'PY' "$raw_path" "$out_path"
import json
import re
import sys
import pyte

raw_path = sys.argv[1]
out_path = sys.argv[2]

data = open(raw_path, "rb").read()
text = data.decode("utf-8", errors="replace")

screens = ["Search", "Explorer", "Analytics", "Tool Metrics"]
seen = []
seen_set = set()
pat_by_screen = {
    screen: re.compile(re.escape(screen) + r"\s*\|\s*mode:", re.IGNORECASE)
    for screen in screens
}

screen = pyte.Screen(120, 40)
stream = pyte.Stream(screen)

def bottom_line() -> str:
    row = screen.lines - 1
    out = []
    for col in range(screen.columns):
        ch = screen.buffer[row][col]
        out.append(ch.data if ch.data else " ")
    return "".join(out)

# Feed incrementally so we can detect transitions as they happen.
chunk_size = 512
for i in range(0, len(text), chunk_size):
    stream.feed(text[i : i + chunk_size])
    bl = bottom_line()
    for name, pat in pat_by_screen.items():
        if name not in seen_set and pat.search(bl):
            seen_set.add(name)
            seen.append(name)

with open(out_path, "w", encoding="utf-8") as f:
    for idx, name in enumerate(seen, start=1):
        f.write(
            json.dumps(
                {"step": idx, "screen": name, "via": "keyboard"},
                separators=(",", ":"),
            )
            + "\n"
        )
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

# Start a TUI session via expect with proper terminal dimensions.
# Args: label bin port db storage raw_log expect_script
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

# Build the binary
BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# ═══════════════════════════════════════════════════════════════════════
# Case 1: Theme contrast metrics (logged via --nocapture)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "contrast_metrics"

set +e
CONTRAST_OUT="$(
    cargo test -p mcp-agent-mail-server theme_palettes_meet_min_contrast_thresholds -- --nocapture 2>&1
)"
CONTRAST_RC=$?
set -e

e2e_save_artifact "case_01_contrast_metrics.txt" "${CONTRAST_OUT}"
e2e_assert_exit_code "contrast metrics test exits 0" "0" "${CONTRAST_RC}"
e2e_assert_contains "contrast metrics include theme lines" "${CONTRAST_OUT}" "theme="

# ═══════════════════════════════════════════════════════════════════════
# Case 2: Keyboard-only navigation across core screens (Search/Explorer/Analytics/Tools)
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "keyboard_only_core_screens"

WORK2="$(e2e_mktemp "e2e_tui_a11y_core")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
mkdir -p "${STORAGE2}"
PORT2="$(pick_port)"
RAW2="${E2E_ARTIFACT_DIR}/core_screens.raw"

# Focus trace + key trace (simple, deterministic)
EXPECT_SCRIPT_CORE='
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 35
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    TUI_KEY_HINTS=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 5

proc tab_to_screen {label max_steps} {
    set pat [format {%s \| mode:} $label]
    for {set i 0} {$i < $max_steps} {incr i} {
        send "\t"
        sleep 0.35
        expect -timeout 2 {
            -re $pat { return 1 }
            timeout {}
        }
    }
    return 0
}

# Wait for the dashboard chrome to appear first.
expect -timeout 8 -re {Dashboard \| mode:} {}

# Keyboard-only navigation across core screens.
# Prefer Tab cycling (more deterministic than palette search selection).
tab_to_screen "Search" 10
sleep 0.5
tab_to_screen "Explorer" 16
sleep 0.5
tab_to_screen "Analytics" 6
sleep 0.5
tab_to_screen "Tool Metrics" 24
sleep 0.6

# Basic keyboard interaction on Tools screen
send "j"
sleep 0.2
send "k"
sleep 0.2
send "v"
sleep 0.2
send "v"
sleep 0.2

send "q"
expect eof
'

run_tui_expect "core_screens" "${BIN}" "${PORT2}" "${DB2}" "${STORAGE2}" "${RAW2}" "${EXPECT_SCRIPT_CORE}" &
EXPECT_PID2=$!

sleep 6
if e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    EP="$(jsonrpc_call "${PORT2}" "ensure_project" '{"human_key":"/data/e2e/tui_a11y"}')"
    REG1="$(jsonrpc_call "${PORT2}" "register_agent" '{"project_key":"/data/e2e/tui_a11y","program":"e2e","model":"test","name":"A11yFox","task_description":"seed"}')"
    MSG="$(jsonrpc_call "${PORT2}" "send_message" '{"project_key":"/data/e2e/tui_a11y","sender_name":"A11yFox","to":["A11yFox"],"subject":"A11Y seed","body_md":"seeded"}')"
    e2e_save_artifact "case_02_seed_project.json" "${EP}"
    e2e_save_artifact "case_02_seed_agent.json" "${REG1}"
    e2e_save_artifact "case_02_seed_message.json" "${MSG}"
    e2e_pass "seeded data during live TUI session"
else
    e2e_fail "server port not reachable during live TUI session"
fi

wait "${EXPECT_PID2}" 2>/dev/null || true

RENDERED2="${E2E_ARTIFACT_DIR}/core_screens.rendered.txt"
TRANSCRIPT2="${E2E_ARTIFACT_DIR}/core_screens.transcript.txt"
if [ -f "${RAW2}" ]; then
    render_pty_output "${RAW2}" "${RENDERED2}"
    strip_ansi_stream "${RAW2}" "${TRANSCRIPT2}"
    extract_focus_trace_from_raw "${RAW2}" "${E2E_ARTIFACT_DIR}/trace/core_focus_trace.jsonl"
    e2e_pass "keyboard-only navigation completed without crash"
else
    e2e_fail "keyboard-only navigation: raw log not created"
fi

TRACE2="${E2E_ARTIFACT_DIR}/trace/core_focus_trace.jsonl"
e2e_assert_file_contains "visited Search" "${TRACE2}" "\"screen\":\"Search\""
e2e_assert_file_contains "visited Explorer" "${TRACE2}" "\"screen\":\"Explorer\""
e2e_assert_file_contains "visited Analytics" "${TRACE2}" "\"screen\":\"Analytics\""
e2e_assert_file_contains "visited Tool Metrics" "${TRACE2}" "\"screen\":\"Tool Metrics\""

# ═══════════════════════════════════════════════════════════════════════
# Case 3: Key hints are visible by default in a screen with bindings
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "key_hints_default_visible"

WORK3="$(e2e_mktemp "e2e_tui_a11y_hints_default")"
DB3="${WORK3}/db.sqlite3"
STORAGE3="${WORK3}/storage"
mkdir -p "${STORAGE3}"
PORT3="$(pick_port)"
RAW3="${E2E_ARTIFACT_DIR}/key_hints_default.raw"

EXPECT_SCRIPT_HINTS_DEFAULT='
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
    TUI_KEY_HINTS=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 5

send "\020"
sleep 0.4
send "\025"
sleep 0.1
foreach c [split "go to tool metrics" ""] {
    send $c
    sleep 0.03
}
send "\r"
sleep 0.8
send "\033"
sleep 0.4

send "q"
expect eof
'

run_tui_expect "key_hints_default" "${BIN}" "${PORT3}" "${DB3}" "${STORAGE3}" "${RAW3}" "${EXPECT_SCRIPT_HINTS_DEFAULT}" || true

RENDERED3="${E2E_ARTIFACT_DIR}/key_hints_default.rendered.txt"
if [ -f "${RAW3}" ]; then
    render_pty_output "${RAW3}" "${RENDERED3}"
    e2e_pass "key hints default flow completed without crash"
else
    e2e_fail "key hints default: raw log not created"
fi

e2e_assert_file_contains "key hints visible" "${RENDERED3}" "Navigate tools"

# ═══════════════════════════════════════════════════════════════════════
# Case 4: Key hints toggle affects status bar content
# ═══════════════════════════════════════════════════════════════════════
e2e_case_banner "toggle_key_hints"

WORK4="$(e2e_mktemp "e2e_tui_a11y_hints")"
DB4="${WORK4}/db.sqlite3"
STORAGE4="${WORK4}/storage"
mkdir -p "${STORAGE4}"
PORT4="$(pick_port)"
RAW4="${E2E_ARTIFACT_DIR}/key_hints.raw"

EXPECT_SCRIPT_HINTS='
set bin [lindex $argv 0]
set port [lindex $argv 1]
set db [lindex $argv 2]
set storage [lindex $argv 3]
set raw_log [lindex $argv 4]

log_file -noappend $raw_log
set timeout 35
set stty_init "rows 40 columns 120"

spawn env DATABASE_URL=sqlite:////$db \
    STORAGE_ROOT=$storage \
    HTTP_HOST=127.0.0.1 \
    HTTP_PORT=$port \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_JWT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    TUI_KEY_HINTS=1 \
    LINES=40 COLUMNS=120 \
    $bin serve --host 127.0.0.1 --port $port

sleep 5

# Navigate to Tool Metrics, verify key hints visible, then toggle them off via palette.
send "\020"
sleep 0.4
send "\025"
sleep 0.1
foreach c [split "go to tool metrics" ""] {
    send $c
    sleep 0.03
}
send "\r"
sleep 0.8
send "\033"
sleep 0.4

# Toggle key hints via palette action.
send "\020"
sleep 0.4
send "\025"
sleep 0.1
foreach c [split "toggle key hints" ""] {
    send $c
    sleep 0.03
}
sleep 0.4
send "\r"
sleep 1.2

send "q"
expect eof
'

run_tui_expect "key_hints" "${BIN}" "${PORT4}" "${DB4}" "${STORAGE4}" "${RAW4}" "${EXPECT_SCRIPT_HINTS}" || true

RENDERED4="${E2E_ARTIFACT_DIR}/key_hints.rendered.txt"
if [ -f "${RAW4}" ]; then
    render_pty_output "${RAW4}" "${RENDERED4}"
    e2e_pass "key hints toggle completed without crash"
else
    e2e_fail "key hints toggle: raw log not created"
fi

# After toggling off, the Tool Metrics hint label should not appear.
e2e_assert_file_not_contains "key hints hidden" "${RENDERED4}" "Navigate tools"

e2e_summary
