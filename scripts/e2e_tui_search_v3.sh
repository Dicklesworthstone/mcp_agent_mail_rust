#!/usr/bin/env bash
# e2e_tui_search_v3.sh - PTY E2E suite for TUI Search V3 cockpit screen.
#
# Run via (authoritative):
#   am e2e run --project . tui_search_v3
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh tui_search_v3
#
# Validates:
#   - Search screen layout (query bar, facets, results, detail)
#   - Tab bar rendering with all screen labels
#   - All 8 facet labels and their default values
#   - Query bar hint chips (scope:, type: after "/" key)
#   - Query syntax help popup (? key)
#   - Search execution against seeded data (multiple queries)
#   - Results list rendering with [M] markers and importance indicators
#   - Detail panel fields (Type, Title, From, ID, Importance, Ack, etc.)
#   - Results navigation (j/k, G/g) with detail updates
#   - Empty database graceful rendering
#
# Target: >= 70 assertions with per-step transcript artifacts.
#
# Artifacts:
#   tests/artifacts/tui_search_v3/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_search_v3}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Search V3 Cockpit (PTY) E2E Test Suite"

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

# Set terminal size (100x40 for search screen needs vertical space)
import struct, fcntl, termios
winsize = struct.pack("HHHH", 40, 100, 0, 0)
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
    env["COLUMNS"] = "100"
    env["LINES"] = "40"
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
    read_available(timeout=2.5)

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
        # Show last 30 lines for debugging
        e2e_log "Transcript tail:"
        tail -30 "${file}" 2>/dev/null | while IFS= read -r line; do
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
    work_dir="$(e2e_mktemp "e2e_tui_search_${label}")"
    local db_path="${work_dir}/db.sqlite3"
    local storage_root="${work_dir}/storage"
    mkdir -p "${storage_root}"
    echo "${work_dir} ${db_path} ${storage_root}"
}

# Build env var args for the serve command
serve_env_args() {
    local db_path="$1"
    local storage_root="$2"
    local port="$3"
    echo "DATABASE_URL=sqlite:////${db_path}" \
         "STORAGE_ROOT=${storage_root}" \
         "HTTP_HOST=127.0.0.1" \
         "HTTP_PORT=${port}" \
         "HTTP_RBAC_ENABLED=0" \
         "HTTP_RATE_LIMIT_ENABLED=0" \
         "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1"
}

# Seed comprehensive search data into a database via headless server
seed_search_data() {
    local db_path="$1"
    local storage_root="$2"
    local port
    port="$(pick_port)"
    local url="http://127.0.0.1:${port}/mcp/"

    e2e_log "Seeding search data on port ${port}..."

    # Start headless server
    env \
        DATABASE_URL="sqlite:////${db_path}" \
        STORAGE_ROOT="${storage_root}" \
        HTTP_HOST="127.0.0.1" \
        HTTP_PORT="${port}" \
        HTTP_RBAC_ENABLED=0 \
        HTTP_RATE_LIMIT_ENABLED=0 \
        HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
        "${BIN}" serve --no-tui --host 127.0.0.1 --port "${port}" &
    local bg_pid=$!
    sleep 2

    # Ensure project
    e2e_rpc_call "seed_project" "${url}" "ensure_project" \
        '{"human_key":"/tmp/e2e-search-v3"}' >/dev/null 2>&1 || true

    # Register agents
    e2e_rpc_call "seed_agent1" "${url}" "register_agent" \
        '{"project_key":"/tmp/e2e-search-v3","program":"e2e","model":"test","name":"RedFox","task_description":"Search E2E"}' \
        >/dev/null 2>&1 || true

    e2e_rpc_call "seed_agent2" "${url}" "register_agent" \
        '{"project_key":"/tmp/e2e-search-v3","program":"e2e","model":"test","name":"BluePeak","task_description":"Search E2E helper"}' \
        >/dev/null 2>&1 || true

    # Send messages with varying importance and content
    e2e_rpc_call "seed_msg1" "${url}" "send_message" \
        '{"project_key":"/tmp/e2e-search-v3","sender_name":"RedFox","to":["BluePeak"],"subject":"Deploy plan alpha","body_md":"Deploy to production next week.","importance":"normal"}' \
        >/dev/null 2>&1 || true

    e2e_rpc_call "seed_msg2" "${url}" "send_message" \
        '{"project_key":"/tmp/e2e-search-v3","sender_name":"RedFox","to":["BluePeak"],"subject":"Build failed in staging","body_md":"The build pipeline is broken.","importance":"high"}' \
        >/dev/null 2>&1 || true

    e2e_rpc_call "seed_msg3" "${url}" "send_message" \
        '{"project_key":"/tmp/e2e-search-v3","sender_name":"BluePeak","to":["RedFox"],"subject":"Migration urgent review","body_md":"Please review the DB migration ASAP.","importance":"urgent","ack_required":true}' \
        >/dev/null 2>&1 || true

    e2e_rpc_call "seed_msg4" "${url}" "send_message" \
        '{"project_key":"/tmp/e2e-search-v3","sender_name":"RedFox","to":["BluePeak"],"subject":"Search test message alpha","body_md":"First search test content for validation."}' \
        >/dev/null 2>&1 || true

    e2e_rpc_call "seed_msg5" "${url}" "send_message" \
        '{"project_key":"/tmp/e2e-search-v3","sender_name":"BluePeak","to":["RedFox"],"subject":"Search test message beta","body_md":"Second search test content for validation."}' \
        >/dev/null 2>&1 || true

    # Stop headless server
    kill "${bg_pid}" 2>/dev/null || true
    wait "${bg_pid}" 2>/dev/null || true
    e2e_log "Seeding complete."
}


# ────────────────────────────────────────────────────────────────────
# Case 1: Navigate to Search screen and verify initial layout
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "search_screen_initial_layout"
read -r WORK1 DB1 STORAGE1 <<< "$(setup_server_env "layout")"
PORT1="$(pick_port)"

TRANSCRIPT1="${E2E_ARTIFACT_DIR}/layout_transcript.txt"

# Press "5" to navigate to Search screen, wait for render, then quit
KEYS1='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 600, "keys": "5"},
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "layout" "${TRANSCRIPT1}" "${KEYS1}" \
    env \
    DATABASE_URL="sqlite:////${DB1}" \
    STORAGE_ROOT="${STORAGE1}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT1}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT1}" || true

# Core layout elements
assert_transcript_contains "layout: Search in title"     "${TRANSCRIPT1}" "Search"
assert_transcript_contains "layout: No results message"  "${TRANSCRIPT1}" "No results"
assert_transcript_contains "layout: Detail hint"         "${TRANSCRIPT1}" "Select a result"
assert_transcript_contains "layout: f:facets shortcut"   "${TRANSCRIPT1}" "f:facets"
assert_transcript_contains "layout: L:query shortcut"    "${TRANSCRIPT1}" "L:query"
# Tab bar labels
assert_transcript_contains "layout: Dashboard tab"       "${TRANSCRIPT1}" "1:Dashboard"
assert_transcript_contains "layout: Search tab"          "${TRANSCRIPT1}" "5:Search"
assert_transcript_contains "layout: Reservations tab"    "${TRANSCRIPT1}" "6:Reservations"


# ────────────────────────────────────────────────────────────────────
# Case 2: All 8 facet labels visible
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "facet_labels"
read -r WORK2 DB2 STORAGE2 <<< "$(setup_server_env "labels")"
PORT2="$(pick_port)"

TRANSCRIPT2="${E2E_ARTIFACT_DIR}/labels_transcript.txt"

# Navigate to Search (5) and verify facet labels
KEYS2='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "labels" "${TRANSCRIPT2}" "${KEYS2}" \
    env \
    DATABASE_URL="sqlite:////${DB2}" \
    STORAGE_ROOT="${STORAGE2}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT2}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT2}" || true

assert_transcript_contains "labels: Global scope"  "${TRANSCRIPT2}" "[Global]"
assert_transcript_contains "labels: Doc Type"     "${TRANSCRIPT2}" "Doc Type"
assert_transcript_contains "labels: Search Mode"  "${TRANSCRIPT2}" "Search Mode"
assert_transcript_contains "labels: Importance"   "${TRANSCRIPT2}" "Importance"
assert_transcript_contains "labels: Ack Required" "${TRANSCRIPT2}" "Ack Required"
assert_transcript_contains "labels: Sort Order"   "${TRANSCRIPT2}" "Sort Order"
assert_transcript_contains "labels: Search Field" "${TRANSCRIPT2}" "Search Field"
assert_transcript_contains "labels: Explain"      "${TRANSCRIPT2}" "Explain"


# ────────────────────────────────────────────────────────────────────
# Case 3: Facet default values
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "facet_defaults"
read -r WORK3 DB3 STORAGE3 <<< "$(setup_server_env "defaults")"
PORT3="$(pick_port)"

TRANSCRIPT3="${E2E_ARTIFACT_DIR}/defaults_transcript.txt"

KEYS3='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "defaults" "${TRANSCRIPT3}" "${KEYS3}" \
    env \
    DATABASE_URL="sqlite:////${DB3}" \
    STORAGE_ROOT="${STORAGE3}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT3}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT3}" || true

assert_transcript_contains "defaults: Global scope"     "${TRANSCRIPT3}" "[Global]"
assert_transcript_contains "defaults: Messages doctype"  "${TRANSCRIPT3}" "[Messages]"
assert_transcript_contains "defaults: Auto mode"         "${TRANSCRIPT3}" "[Auto]"
assert_transcript_contains "defaults: Any importance"    "${TRANSCRIPT3}" "[Any]"
assert_transcript_contains "defaults: Newest sort"       "${TRANSCRIPT3}" "[Newest]"
assert_transcript_contains "defaults: Subject+Body"      "${TRANSCRIPT3}" "[Subject+Body]"
assert_transcript_contains "defaults: Off explain"       "${TRANSCRIPT3}" "[Off]"


# ────────────────────────────────────────────────────────────────────
# Case 4: Query bar hint chips appear when editing
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "query_bar_hints"
read -r WORK4 DB4 STORAGE4 <<< "$(setup_server_env "hints")"
PORT4="$(pick_port)"

TRANSCRIPT4="${E2E_ARTIFACT_DIR}/hints_transcript.txt"

# Press "/" to enter query bar, wait, then Escape and quit
KEYS4='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 400, "keys": "\\x1b"},
    {"delay_ms": 400, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "hints" "${TRANSCRIPT4}" "${KEYS4}" \
    env \
    DATABASE_URL="sqlite:////${DB4}" \
    STORAGE_ROOT="${STORAGE4}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT4}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT4}" || true

# Hint chips appear in the query bar hint line
assert_transcript_contains "hints: scope chip"     "${TRANSCRIPT4}" "scope:Global"
assert_transcript_contains "hints: type chip"      "${TRANSCRIPT4}" "type:Messages"
# Facets are still visible
assert_transcript_contains "hints: facets visible" "${TRANSCRIPT4}" "[Auto]"
assert_transcript_contains "hints: explain visible" "${TRANSCRIPT4}" "[Off]"


# ────────────────────────────────────────────────────────────────────
# Case 5: Query syntax help popup
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "query_syntax_help"
read -r WORK5 DB5 STORAGE5 <<< "$(setup_server_env "syntax_help")"
PORT5="$(pick_port)"

TRANSCRIPT5="${E2E_ARTIFACT_DIR}/syntax_help_transcript.txt"

# Navigate to Search (5), press "/" to focus, press "?" for help
KEYS5='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 500, "keys": "?"},
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 300, "keys": "\\x1b"},
    {"delay_ms": 300, "keys": "\\x1b"},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "syntax_help" "${TRANSCRIPT5}" "${KEYS5}" \
    env \
    DATABASE_URL="sqlite:////${DB5}" \
    STORAGE_ROOT="${STORAGE5}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT5}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT5}" || true

assert_transcript_contains "help: popup title"    "${TRANSCRIPT5}" "Query Syntax Help"
assert_transcript_contains "help: AND/OR"         "${TRANSCRIPT5}" "AND/OR"
assert_transcript_contains "help: Quotes"         "${TRANSCRIPT5}" "Quotes"
assert_transcript_contains "help: Prefix"         "${TRANSCRIPT5}" "Prefix"
assert_transcript_contains "help: NOT"            "${TRANSCRIPT5}" "NOT"
assert_transcript_contains "help: Column"         "${TRANSCRIPT5}" "Column"
assert_transcript_contains "help: close hint"     "${TRANSCRIPT5}" "close"


# ────────────────────────────────────────────────────────────────────
# Seed shared data for cases 6-12
# ────────────────────────────────────────────────────────────────────
e2e_log "Setting up shared seeded environment..."
read -r WORK_SEED DB_SEED STORAGE_SEED <<< "$(setup_server_env "seeded")"
seed_search_data "${DB_SEED}" "${STORAGE_SEED}"


# ────────────────────────────────────────────────────────────────────
# Case 6: Search for "Deploy" - results and detail
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "search_deploy"
PORT6="$(pick_port)"

TRANSCRIPT6="${E2E_ARTIFACT_DIR}/deploy_transcript.txt"

KEYS6='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "Deploy"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "deploy" "${TRANSCRIPT6}" "${KEYS6}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT6}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT6}" || true

# Search results should contain Deploy message
assert_transcript_contains "deploy: subject"          "${TRANSCRIPT6}" "Deploy"
assert_transcript_contains "deploy: Type field"       "${TRANSCRIPT6}" "Type:"
assert_transcript_contains "deploy: Title field"      "${TRANSCRIPT6}" "Title:"
assert_transcript_contains "deploy: ID field"         "${TRANSCRIPT6}" "ID:"
assert_transcript_contains "deploy: From field"       "${TRANSCRIPT6}" "From:"
assert_transcript_contains "deploy: agent name"       "${TRANSCRIPT6}" "RedFox"
assert_transcript_contains "deploy: scope chip"       "${TRANSCRIPT6}" "scope:Global"


# ────────────────────────────────────────────────────────────────────
# Case 7: Search for "Migration" - urgent message detail
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "search_migration"
PORT7="$(pick_port)"

TRANSCRIPT7="${E2E_ARTIFACT_DIR}/migration_transcript.txt"

KEYS7='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "Migration"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "migration" "${TRANSCRIPT7}" "${KEYS7}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT7}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT7}" || true

assert_transcript_contains "migration: subject"       "${TRANSCRIPT7}" "Migration"
assert_transcript_contains "migration: sender"        "${TRANSCRIPT7}" "BluePeak"
assert_transcript_contains "migration: Importance"    "${TRANSCRIPT7}" "Importance:"
assert_transcript_contains "migration: Ack field"     "${TRANSCRIPT7}" "Ack:"
assert_transcript_contains "migration: urgent value"  "${TRANSCRIPT7}" "urgent"
assert_transcript_contains "migration: Body section"  "${TRANSCRIPT7}" "Body"
assert_transcript_contains "migration: Project field" "${TRANSCRIPT7}" "Project:"
assert_transcript_contains "migration: Score field"   "${TRANSCRIPT7}" "Score:"


# ────────────────────────────────────────────────────────────────────
# Case 8: Search for "Search test" - multiple results
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "search_multiresult"
PORT8="$(pick_port)"

TRANSCRIPT8="${E2E_ARTIFACT_DIR}/multiresult_transcript.txt"

KEYS8='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "Search test"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "multiresult" "${TRANSCRIPT8}" "${KEYS8}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT8}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT8}" || true

# Both search test messages appear
assert_transcript_contains "multi: alpha subject"    "${TRANSCRIPT8}" "alpha"
assert_transcript_contains "multi: beta subject"     "${TRANSCRIPT8}" "beta"
assert_transcript_contains "multi: Search test"      "${TRANSCRIPT8}" "Search test"
assert_transcript_contains "multi: Title field"      "${TRANSCRIPT8}" "Title:"
assert_transcript_contains "multi: message marker"   "${TRANSCRIPT8}" "[M]"


# ────────────────────────────────────────────────────────────────────
# Case 9: All messages visible when searching broadly
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "search_all_messages"
PORT9="$(pick_port)"

TRANSCRIPT9="${E2E_ARTIFACT_DIR}/allmsg_transcript.txt"

# Search for a term that appears in ALL messages (or leave empty for all)
# Use "*" which matches everything via FTS
KEYS9='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "test OR Deploy OR Build OR Migration"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "allmsg" "${TRANSCRIPT9}" "${KEYS9}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT9}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT9}" || true

assert_transcript_contains "all: Deploy message"     "${TRANSCRIPT9}" "Deploy"
assert_transcript_contains "all: Build message"      "${TRANSCRIPT9}" "Build"
assert_transcript_contains "all: Migration message"  "${TRANSCRIPT9}" "Migration"
assert_transcript_contains "all: test messages"      "${TRANSCRIPT9}" "Search test"
assert_transcript_contains "all: marker"             "${TRANSCRIPT9}" "[M]"
assert_transcript_contains "all: type chip"          "${TRANSCRIPT9}" "type:Messages"


# ────────────────────────────────────────────────────────────────────
# Case 10: Detail panel shows all metadata fields
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "detail_panel_fields"
PORT10="$(pick_port)"

TRANSCRIPT10="${E2E_ARTIFACT_DIR}/detail_transcript.txt"

# Search for "Migration" to get the urgent message with rich metadata
KEYS10='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "Migration"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "detail" "${TRANSCRIPT10}" "${KEYS10}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT10}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT10}" || true

# Full detail panel metadata
assert_transcript_contains "detail: Type field"       "${TRANSCRIPT10}" "Type:"
assert_transcript_contains "detail: Title field"      "${TRANSCRIPT10}" "Title:"
assert_transcript_contains "detail: ID field"         "${TRANSCRIPT10}" "ID:"
assert_transcript_contains "detail: From field"       "${TRANSCRIPT10}" "From:"
assert_transcript_contains "detail: Importance field" "${TRANSCRIPT10}" "Importance:"
assert_transcript_contains "detail: Ack field"        "${TRANSCRIPT10}" "Ack:"
assert_transcript_contains "detail: Time field"       "${TRANSCRIPT10}" "Time:"
assert_transcript_contains "detail: Project field"    "${TRANSCRIPT10}" "Project:"
assert_transcript_contains "detail: Score field"      "${TRANSCRIPT10}" "Score:"
assert_transcript_contains "detail: Body section"     "${TRANSCRIPT10}" "Body"
assert_transcript_contains "detail: Message type"     "${TRANSCRIPT10}" "Message"
assert_transcript_contains "detail: ID number"        "${TRANSCRIPT10}" "#"


# ────────────────────────────────────────────────────────────────────
# Case 11: Results navigation with j/k updates detail
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "results_navigation"
PORT11="$(pick_port)"

TRANSCRIPT11="${E2E_ARTIFACT_DIR}/nav_transcript.txt"

# Search for "Search test" (matches 2), then navigate with j/k
KEYS11='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "Search test"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 400, "keys": "j"},
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "k"},
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "G"},
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 400, "keys": "g"},
    {"delay_ms": 500, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "nav" "${TRANSCRIPT11}" "${KEYS11}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT11}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT11}" || true

# Both messages visible in results
assert_transcript_contains "nav: alpha result"        "${TRANSCRIPT11}" "alpha"
assert_transcript_contains "nav: beta result"         "${TRANSCRIPT11}" "beta"
# Detail rendered for navigated items
assert_transcript_contains "nav: detail rendered"     "${TRANSCRIPT11}" "Title:"
assert_transcript_contains "nav: ID rendered"         "${TRANSCRIPT11}" "ID:"
assert_transcript_contains "nav: From rendered"       "${TRANSCRIPT11}" "From:"


# ────────────────────────────────────────────────────────────────────
# Case 12: Search with seeded data - importance indicators
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "importance_indicators"
PORT12="$(pick_port)"

TRANSCRIPT12="${E2E_ARTIFACT_DIR}/importance_transcript.txt"

# Search for all messages to see importance markers
KEYS12='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "Deploy OR Build OR Migration OR test"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "importance" "${TRANSCRIPT12}" "${KEYS12}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT12}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT12}" || true

# Urgent messages show !! marker, high shows !
assert_transcript_contains "imp: urgent marker"       "${TRANSCRIPT12}" "!!"
assert_transcript_contains "imp: high marker"         "${TRANSCRIPT12}" "!"
assert_transcript_contains "imp: normal message"      "${TRANSCRIPT12}" "Deploy"
# Agent names visible in results
assert_transcript_contains "imp: Search test msg"      "${TRANSCRIPT12}" "Search test"
assert_transcript_contains "imp: BluePeak agent"      "${TRANSCRIPT12}" "BluePeak"


# ────────────────────────────────────────────────────────────────────
# Case 13: Search for "Build" - high importance detail
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "search_build"
PORT13="$(pick_port)"

TRANSCRIPT13="${E2E_ARTIFACT_DIR}/build_transcript.txt"

KEYS13='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 500, "keys": "/"},
    {"delay_ms": 200, "keys": "Build"},
    {"delay_ms": 400, "keys": "\\r"},
    {"delay_ms": 1000, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "build" "${TRANSCRIPT13}" "${KEYS13}" \
    env \
    DATABASE_URL="sqlite:////${DB_SEED}" \
    STORAGE_ROOT="${STORAGE_SEED}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT13}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT13}" || true

assert_transcript_contains "build: subject"           "${TRANSCRIPT13}" "Build"
assert_transcript_contains "build: staging text"      "${TRANSCRIPT13}" "staging"
assert_transcript_contains "build: RedFox sender"     "${TRANSCRIPT13}" "RedFox"
assert_transcript_contains "build: From field"        "${TRANSCRIPT13}" "From:"
assert_transcript_contains "build: high importance"   "${TRANSCRIPT13}" "high"


# ────────────────────────────────────────────────────────────────────
# Case 14: Empty database - graceful rendering
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "empty_database"
read -r WORK14 DB14 STORAGE14 <<< "$(setup_server_env "empty")"
PORT14="$(pick_port)"

TRANSCRIPT14="${E2E_ARTIFACT_DIR}/empty_transcript.txt"

# Navigate to Search on empty DB, no query submitted
KEYS14='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "empty" "${TRANSCRIPT14}" "${KEYS14}" \
    env \
    DATABASE_URL="sqlite:////${DB14}" \
    STORAGE_ROOT="${STORAGE14}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT14}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT14}" || true

# Empty DB should still render search screen gracefully
assert_transcript_contains "empty: No results"       "${TRANSCRIPT14}" "No results"
assert_transcript_contains "empty: Detail hint"      "${TRANSCRIPT14}" "Select a result"
assert_transcript_contains "empty: facets visible"   "${TRANSCRIPT14}" "[Auto]"


# ────────────────────────────────────────────────────────────────────
# Case 15: Tab bar shows all screen names
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "tab_bar_screens"
read -r WORK15 DB15 STORAGE15 <<< "$(setup_server_env "tabs")"
PORT15="$(pick_port)"

TRANSCRIPT15="${E2E_ARTIFACT_DIR}/tabs_transcript.txt"

# Navigate to Search and verify tab bar
KEYS15='[
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 500, "keys": "5"},
    {"delay_ms": 800, "keys": ""},
    {"delay_ms": 300, "keys": "q"}
]'

run_pty_interaction "tabs" "${TRANSCRIPT15}" "${KEYS15}" \
    env \
    DATABASE_URL="sqlite:////${DB15}" \
    STORAGE_ROOT="${STORAGE15}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT15}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT15}" || true

assert_transcript_contains "tabs: Dashboard"     "${TRANSCRIPT15}" "1:Dashboard"
assert_transcript_contains "tabs: Messages"      "${TRANSCRIPT15}" "2:Messages"
assert_transcript_contains "tabs: Threads"       "${TRANSCRIPT15}" "3:Threads"
assert_transcript_contains "tabs: Agents"        "${TRANSCRIPT15}" "4:Agents"
assert_transcript_contains "tabs: Search"        "${TRANSCRIPT15}" "5:Search"
assert_transcript_contains "tabs: Reservations"  "${TRANSCRIPT15}" "6:Reservations"
assert_transcript_contains "tabs: Tool Metrics"  "${TRANSCRIPT15}" "7:Tool Metrics"


# ────────────────────────────────────────────────────────────────────
e2e_summary
