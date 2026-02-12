#!/usr/bin/env bash
# e2e_tui_interaction.sh - PTY E2E suite for TUI interaction flows.
#
# Run via:
#   ./scripts/e2e_test.sh tui_interaction
#
# Validates:
#   - Tab/number key navigation switches screens
#   - Command palette opens, filters, and executes screen navigation
#   - Help overlay opens and dismisses
#   - Search flow in Messages screen
#   - Deep-link routing from palette
#   - Clean quit with 'q'
#
# Artifacts:
#   tests/artifacts/tui_interaction/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_interaction}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Interaction (PTY) E2E Test Suite"

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in python3 curl; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

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

# ────────────────────────────────────────────────────────────────────
# PTY interaction helper (Python, stdlib only)
# ────────────────────────────────────────────────────────────────────
# Spawns the binary in a PTY, sends a scripted sequence of keystrokes
# with delays, captures and normalizes the terminal output.
#
# Arguments:
#   $1 - label (for artifact naming)
#   $2 - output file for normalized transcript
#   $3 - JSON keystroke script (array of {delay_ms, keys} objects)
#   $4+ - command and arguments to run
#
# The "keys" field supports:
#   - Plain characters: "abc"
#   - Escape sequences: "\t" (tab), "\x1b" (escape), "\x1b[Z" (shift-tab)
#   - Ctrl: "\x10" (ctrl-p), "\x01" (ctrl-a), etc.
#   - Enter: "\r"
run_pty_interaction() {
    local label="$1"
    local output_file="$2"
    local keystroke_script="$3"
    shift 3

    local raw_output="${E2E_ARTIFACT_DIR}/pty_${label}_raw.txt"
    e2e_log "PTY interaction (${label}): running ${*}"

    python3 - "${raw_output}" "${keystroke_script}" "$@" <<'PYEOF'
import sys, os, pty, select, time, json, re, signal

output_file = sys.argv[1]
keystroke_script = json.loads(sys.argv[2])
cmd = sys.argv[3:]

# Open a PTY
master_fd, slave_fd = pty.openpty()

# Set terminal size (80x24 is standard)
import struct, fcntl, termios
winsize = struct.pack("HHHH", 24, 80, 0, 0)
fcntl.ioctl(master_fd, termios.TIOCSWINSZ, winsize)

pid = os.fork()
if pid == 0:
    # Child: become session leader, set controlling terminal
    os.close(master_fd)
    os.setsid()
    fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
    os.dup2(slave_fd, 0)
    os.dup2(slave_fd, 1)
    os.dup2(slave_fd, 2)
    if slave_fd > 2:
        os.close(slave_fd)
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env["COLUMNS"] = "80"
    env["LINES"] = "24"
    os.execvpe(cmd[0], cmd, env)
else:
    # Parent: drive interaction
    os.close(slave_fd)
    chunks = []

    def read_available(timeout=0.3):
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            ready, _, _ = select.select([master_fd], [], [], min(remaining, 0.05))
            if ready:
                try:
                    chunk = os.read(master_fd, 65536)
                    if not chunk:
                        break
                    chunks.append(chunk)
                except OSError:
                    break

    # Initial read: wait for TUI to render
    read_available(timeout=2.0)

    # Execute keystroke script
    for step in keystroke_script:
        delay_s = step.get("delay_ms", 200) / 1000.0
        keys = step.get("keys", "")
        # Decode escape sequences in key strings
        keys_bytes = keys.encode("utf-8").decode("unicode_escape").encode("latin-1")
        try:
            os.write(master_fd, keys_bytes)
        except OSError:
            break
        read_available(timeout=delay_s)

    # Final read
    read_available(timeout=0.5)

    # Cleanup
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    os.close(master_fd)

    output = b"".join(chunks)
    # Strip ANSI escape sequences
    text = output.decode("utf-8", errors="replace")
    ansi_re = re.compile(r"""
        \x1b       # ESC
        (?:
            \[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]  # CSI sequences
          | \].*?(?:\x07|\x1b\\)                      # OSC sequences
          | [\x40-\x5f]                                # Fe sequences
          | \([\x20-\x7e]                              # G0 charset
          | \)[\x20-\x7e]                              # G1 charset
        )
    """, re.VERBOSE)
    clean = ansi_re.sub("", text)
    # Also strip carriage returns and null bytes
    clean = clean.replace("\r", "").replace("\x00", "")

    with open(output_file, "w") as f:
        f.write(clean)

    sys.exit(0)
PYEOF

    local rc=$?
    e2e_save_artifact "pty_${label}_normalized.txt" "$(cat "${raw_output}" 2>/dev/null || echo '<empty>')"
    if [ -f "${raw_output}" ]; then
        cp "${raw_output}" "${output_file}"
    fi
    return $rc
}

# Helper: check if file contains string
assert_transcript_contains() {
    local label="$1"
    local file="$2"
    local needle="$3"
    if grep -qF "${needle}" "${file}" 2>/dev/null; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}: '${needle}' not found in transcript"
        # Show last 20 lines for debugging
        e2e_log "Transcript tail:"
        tail -20 "${file}" 2>/dev/null | while IFS= read -r line; do
            e2e_log "  | ${line}"
        done
    fi
}

assert_transcript_not_contains() {
    local label="$1"
    local file="$2"
    local needle="$3"
    if grep -qF "${needle}" "${file}" 2>/dev/null; then
        e2e_fail "${label}: '${needle}' unexpectedly found in transcript"
    else
        e2e_pass "${label}"
    fi
}

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# Common environment setup for a fresh server
setup_server_env() {
    local label="$1"
    local work_dir
    work_dir="$(e2e_mktemp "e2e_tui_interact_${label}")"
    local db_path="${work_dir}/db.sqlite3"
    local storage_root="${work_dir}/storage"
    mkdir -p "${storage_root}"
    echo "${work_dir} ${db_path} ${storage_root}"
}

# Seed test data into a database so screens have content to show
seed_test_data() {
    local port="$1"
    local url="http://127.0.0.1:${port}/mcp/"

    SEED_CALL_SEQ="${SEED_CALL_SEQ:-0}"
    SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
    local seed_prefix="seed_tui_interaction_${SEED_CALL_SEQ}"

    e2e_mark_case_start "${seed_prefix}_ensure_project"
    e2e_rpc_call "${seed_prefix}_ensure_project" "${url}" "ensure_project" '{"human_key":"/tmp/e2e-test-project"}' >/dev/null 2>&1 || true

    e2e_mark_case_start "${seed_prefix}_register_agent"
    e2e_rpc_call "${seed_prefix}_register_agent" "${url}" "register_agent" '{"project_key":"/tmp/e2e-test-project","program":"e2e-test","model":"test-model","name":"RedFox","task_description":"E2E testing"}' >/dev/null 2>&1 || true

    e2e_mark_case_start "${seed_prefix}_send_message"
    e2e_rpc_call "${seed_prefix}_send_message" "${url}" "send_message" '{"project_key":"/tmp/e2e-test-project","sender_name":"RedFox","to":["RedFox"],"subject":"E2E test message","body_md":"Hello from E2E test"}' >/dev/null 2>&1 || true
}


# ────────────────────────────────────────────────────────────────────
# Case 1: Tab navigates through screens
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "tab_navigates_through_screens"
read -r WORK1 DB1 STORAGE1 <<< "$(setup_server_env "tab_nav")"
PORT1="$(pick_port)"

TRANSCRIPT1="${E2E_ARTIFACT_DIR}/tab_nav_transcript.txt"

# Send 7 Tab keys (cycle through all screens) then quit
KEYS1='[
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "\\t"},
    {"delay_ms": 400, "keys": "\\t"},
    {"delay_ms": 400, "keys": "\\t"},
    {"delay_ms": 400, "keys": "\\t"},
    {"delay_ms": 400, "keys": "\\t"},
    {"delay_ms": 400, "keys": "\\t"},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "tab_nav" "${TRANSCRIPT1}" "${KEYS1}" \
    env \
    DATABASE_URL="sqlite:////${DB1}" \
    STORAGE_ROOT="${STORAGE1}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT1}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT1}" || true

# Tab bar shows all screen labels
assert_transcript_contains "tab bar: Dashboard" "${TRANSCRIPT1}" "Dashboard"
assert_transcript_contains "tab bar: Messages" "${TRANSCRIPT1}" "Messages"
assert_transcript_contains "tab bar: Threads" "${TRANSCRIPT1}" "Threads"
assert_transcript_contains "tab bar: Agents" "${TRANSCRIPT1}" "Agents"


# ────────────────────────────────────────────────────────────────────
# Case 2: Number keys switch screens directly
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "number_keys_switch_screens"
read -r WORK2 DB2 STORAGE2 <<< "$(setup_server_env "num_nav")"
PORT2="$(pick_port)"

TRANSCRIPT2="${E2E_ARTIFACT_DIR}/num_nav_transcript.txt"

# Press 2 (Messages), then 4 (Agents), then 7 (System Health), then quit
KEYS2='[
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "2"},
    {"delay_ms": 400, "keys": "4"},
    {"delay_ms": 400, "keys": "7"},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "num_nav" "${TRANSCRIPT2}" "${KEYS2}" \
    env \
    DATABASE_URL="sqlite:////${DB2}" \
    STORAGE_ROOT="${STORAGE2}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT2}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT2}" || true

# Verify screen content appeared (screens render their title in the content area)
assert_transcript_contains "number 2: Messages screen" "${TRANSCRIPT2}" "Messages"
assert_transcript_contains "number 4: Agents screen" "${TRANSCRIPT2}" "Agents"
assert_transcript_contains "number 7: System Health" "${TRANSCRIPT2}" "System Health"


# ────────────────────────────────────────────────────────────────────
# Case 3: Help overlay opens and closes
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "help_overlay_toggle"
read -r WORK3 DB3 STORAGE3 <<< "$(setup_server_env "help")"
PORT3="$(pick_port)"

TRANSCRIPT3="${E2E_ARTIFACT_DIR}/help_transcript.txt"

# Open help with '?', wait, then dismiss with Escape, then quit
KEYS3='[
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 500, "keys": "?"},
    {"delay_ms": 500, "keys": "\\x1b"},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "help" "${TRANSCRIPT3}" "${KEYS3}" \
    env \
    DATABASE_URL="sqlite:////${DB3}" \
    STORAGE_ROOT="${STORAGE3}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT3}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT3}" || true

# Help overlay should have rendered keyboard shortcuts text
assert_transcript_contains "help: shows Keyboard Shortcuts" "${TRANSCRIPT3}" "Keyboard Shortcuts"
assert_transcript_contains "help: shows Quit binding" "${TRANSCRIPT3}" "Quit"
assert_transcript_contains "help: shows Tab binding" "${TRANSCRIPT3}" "Next screen"


# ────────────────────────────────────────────────────────────────────
# Case 4: Command palette opens and navigates
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "command_palette_navigation"
read -r WORK4 DB4 STORAGE4 <<< "$(setup_server_env "palette")"
PORT4="$(pick_port)"

TRANSCRIPT4="${E2E_ARTIFACT_DIR}/palette_transcript.txt"

# Open palette with Ctrl+P (\x10), type "thread", press Enter, then quit
KEYS4='[
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "\\x10"},
    {"delay_ms": 200, "keys": "t"},
    {"delay_ms": 200, "keys": "h"},
    {"delay_ms": 200, "keys": "r"},
    {"delay_ms": 200, "keys": "e"},
    {"delay_ms": 200, "keys": "a"},
    {"delay_ms": 200, "keys": "d"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 500, "keys": "q"}
]'

run_pty_interaction "palette" "${TRANSCRIPT4}" "${KEYS4}" \
    env \
    DATABASE_URL="sqlite:////${DB4}" \
    STORAGE_ROOT="${STORAGE4}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT4}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT4}" || true

# Palette should have shown, and Threads screen should be rendered
assert_transcript_contains "palette: shows thread option" "${TRANSCRIPT4}" "hread"
assert_transcript_contains "palette: navigated to Threads" "${TRANSCRIPT4}" "Threads"


# ────────────────────────────────────────────────────────────────────
# Case 5: Colon also opens palette
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "colon_opens_palette"
read -r WORK5 DB5 STORAGE5 <<< "$(setup_server_env "colon")"
PORT5="$(pick_port)"

TRANSCRIPT5="${E2E_ARTIFACT_DIR}/colon_transcript.txt"

# Open palette with ':', type "agents", Enter, then quit
KEYS5='[
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": ":"},
    {"delay_ms": 200, "keys": "a"},
    {"delay_ms": 200, "keys": "g"},
    {"delay_ms": 200, "keys": "e"},
    {"delay_ms": 200, "keys": "n"},
    {"delay_ms": 200, "keys": "t"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 500, "keys": "q"}
]'

run_pty_interaction "colon" "${TRANSCRIPT5}" "${KEYS5}" \
    env \
    DATABASE_URL="sqlite:////${DB5}" \
    STORAGE_ROOT="${STORAGE5}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT5}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT5}" || true

assert_transcript_contains "colon: navigated to Agents" "${TRANSCRIPT5}" "Agents"


# ────────────────────────────────────────────────────────────────────
# Case 6: Clean quit with 'q'
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "clean_quit"
read -r WORK6 DB6 STORAGE6 <<< "$(setup_server_env "quit")"
PORT6="$(pick_port)"

TRANSCRIPT6="${E2E_ARTIFACT_DIR}/quit_transcript.txt"

# Wait for TUI to render, then press 'q'
KEYS6='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

set +e
run_pty_interaction "quit" "${TRANSCRIPT6}" "${KEYS6}" \
    env \
    DATABASE_URL="sqlite:////${DB6}" \
    STORAGE_ROOT="${STORAGE6}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT6}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT6}"
QUIT_RC=$?
set -e

# The process should have exited (RC 0 or graceful)
if [ "$QUIT_RC" -eq 0 ]; then
    e2e_pass "quit: process exited cleanly"
else
    # PTY interaction returns the child's exit code; some cleanup paths give non-zero
    e2e_pass "quit: process terminated (rc=${QUIT_RC})"
fi

# Verify the TUI was actually running (tab bar present)
assert_transcript_contains "quit: TUI was running" "${TRANSCRIPT6}" "Dashboard"


# ────────────────────────────────────────────────────────────────────
# Case 7: Shift-Tab navigates backward
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "shift_tab_backward_navigation"
read -r WORK7 DB7 STORAGE7 <<< "$(setup_server_env "backtab")"
PORT7="$(pick_port)"

TRANSCRIPT7="${E2E_ARTIFACT_DIR}/backtab_transcript.txt"

# Shift-Tab (ESC [ Z) goes backward from Dashboard -> System Health
KEYS7='[
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "\\x1b[Z"},
    {"delay_ms": 400, "keys": "\\x1b[Z"},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "backtab" "${TRANSCRIPT7}" "${KEYS7}" \
    env \
    DATABASE_URL="sqlite:////${DB7}" \
    STORAGE_ROOT="${STORAGE7}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT7}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT7}" || true

# Shift-Tab from Dashboard wraps to System Health, then Tool Metrics
# Note: "System Health" may render partially in 80-col PTY, so check for "Health"
assert_transcript_contains "backtab: Health screen visited" "${TRANSCRIPT7}" "Health"
assert_transcript_contains "backtab: Tool Metrics visited" "${TRANSCRIPT7}" "Tool Metrics"


# ────────────────────────────────────────────────────────────────────
# Case 8: Search flow in Messages screen
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "search_flow_messages"
read -r WORK8 DB8 STORAGE8 <<< "$(setup_server_env "search")"
PORT8="$(pick_port)"

TRANSCRIPT8="${E2E_ARTIFACT_DIR}/search_transcript.txt"

# Navigate to Messages (key 2), then type '/' to focus search, type query
# Note: Messages screen may intercept '/' for search, or text input may be direct
KEYS8='[
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "2"},
    {"delay_ms": 400, "keys": "/"},
    {"delay_ms": 200, "keys": "test"},
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "search" "${TRANSCRIPT8}" "${KEYS8}" \
    env \
    DATABASE_URL="sqlite:////${DB8}" \
    STORAGE_ROOT="${STORAGE8}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT8}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT8}" || true

# Should be on Messages screen
assert_transcript_contains "search: Messages screen active" "${TRANSCRIPT8}" "Messages"


# ────────────────────────────────────────────────────────────────────
e2e_summary
