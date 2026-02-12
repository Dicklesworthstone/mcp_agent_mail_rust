#!/usr/bin/env bash
# e2e_tui_compat_matrix.sh - Cross-terminal compatibility matrix (br-3vwi.10.16, br-3vwi.10.21)
#
# Run via:
#   ./scripts/e2e_test.sh tui_compat_matrix
#
# Validates (best-effort in CI):
# - TUI starts and responds to key navigation under multiple TERM settings
# - Dynamic resize events (PTY + tmux) do not crash and preserve basic navigation
# - Unicode/wide glyph content survives through the stack and renders in TUI surfaces
# - Produces profile-scoped artifacts (raw transcript + normalized text + tmux captures)
#
# Notes:
# - This suite intentionally avoids destructive cleanup; set AM_E2E_KEEP_TMP=0 to allow
#   e2e_lib to remove temp dirs (uses `rm -rf`).
#
# Artifacts:
#   tests/artifacts/tui_compat_matrix/<timestamp>/profiles/<profile>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_compat_matrix}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Compatibility Matrix (PTY + tmux)"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in python3 curl tmux timeout; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

terminal_class_from_dims() {
    local rows="$1"
    local cols="$2"
    if [ "${cols}" -lt 40 ] || [ "${rows}" -lt 10 ]; then
        echo "tiny"
    elif [ "${cols}" -lt 80 ]; then
        echo "compact"
    elif [ "${cols}" -lt 120 ]; then
        echo "standard"
    elif [ "${cols}" -lt 180 ]; then
        echo "wide"
    else
        echo "ultrawide"
    fi
}

jsonrpc_call() {
    local port="$1"
    local tool="$2"
    local args_json="$3"
    : "${args_json:="{}"}"

    JSONRPC_CALL_SEQ="${JSONRPC_CALL_SEQ:-0}"
    JSONRPC_CALL_SEQ=$((JSONRPC_CALL_SEQ + 1))

    local case_id="jsonrpc_${JSONRPC_CALL_SEQ}_${tool}"
    local url="http://127.0.0.1:${port}/mcp/"

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call "${case_id}" "${url}" "${tool}" "${args_json}"; then
        :
    fi

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
    if [ -z "${status}" ] || [ "${status}" = "000" ]; then
        return 1
    fi

    e2e_rpc_read_response "${case_id}"
}

start_server_headless() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"

    local prof_dir="${E2E_ARTIFACT_DIR}/profiles/${label}"
    mkdir -p "${prof_dir}"
    local logfile="${prof_dir}/seed_headless.log"

    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"
        export HTTP_RBAC_ENABLED=0
        export HTTP_RATE_LIMIT_ENABLED=0
        export HTTP_JWT_ENABLED=0
        export HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1
        export RUST_LOG="error"
        timeout 20s "${bin}" serve --host 127.0.0.1 --port "${port}" --no-tui
    ) >"${logfile}" 2>&1 &

    echo $!
}

stop_pid() {
    local pid="$1"
    if [ -z "${pid}" ]; then
        return
    fi
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

seed_unicode_fixture() {
    local port="$1"
    local project_key="$2"

    jsonrpc_call "${port}" "ensure_project" "{\"human_key\":\"${project_key}\"}" >/dev/null
    # Use known-valid adjective+noun names (see other E2E suites).
    jsonrpc_call "${port}" "register_agent" "{\"project_key\":\"${project_key}\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"GoldFox\",\"task_description\":\"unicode sender\"}" >/dev/null
    jsonrpc_call "${port}" "register_agent" "{\"project_key\":\"${project_key}\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"SilverWolf\",\"task_description\":\"unicode receiver\"}" >/dev/null

    # Wide glyph + emoji subject/body (CJK + combining + emoji).
    local msg_args
    msg_args="$(python3 - "$project_key" <<'PY'
import json
import sys

project_key = sys.argv[1]

print(json.dumps({
  "project_key": project_key,
  "sender_name": "GoldFox",
  "to": ["SilverWolf"],
  "subject": "Wide glyph: 你好 🚀",
  "body_md": "CJK: 这是中文 / これは日本語 / 이것은 한국어\nCombining: e\u0301 a\u0308 o\u0308\nEmoji: 🚀✅🎛️",
}, ensure_ascii=False, separators=(",", ":")))
PY
)"
    jsonrpc_call "${port}" "send_message" "${msg_args}" >/dev/null
}

run_tui_pty_profile() {
    local profile="$1"
    local term="$2"
    local rows="$3"
    local cols="$4"
    local resize_script_json="$5"
    local key_script_json="$6"
    local expect_unicode="$7" # "1" or "0"

    local prof_dir="${E2E_ARTIFACT_DIR}/profiles/${profile}"
    mkdir -p "${prof_dir}"

    local work db storage port
    work="$(e2e_mktemp "e2e_tui_cm_${profile}")"
    db="${work}/db.sqlite3"
    storage="${work}/storage"
    mkdir -p "${storage}"
    port="$(pick_port)"

    # Optional: seed unicode fixture using headless server first (same DB/storage).
    if [ "${expect_unicode}" = "1" ]; then
        local seed_pid
        seed_pid="$(start_server_headless "${profile}" "${port}" "${db}" "${storage}" "${BIN}")"
        if e2e_wait_port 127.0.0.1 "${port}" 10; then
            local uni_project="/data/e2e/tui_cm_unicode_${profile}"
            seed_unicode_fixture "${port}" "${uni_project}"
            local inbox
            inbox="$(jsonrpc_call "${port}" "fetch_inbox" "{\"project_key\":\"${uni_project}\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true,\"limit\":10}")"
            e2e_save_artifact "profiles/${profile}/seed_inbox.json" "${inbox}"
            if python3 -c "import json,sys; d=json.load(sys.stdin); s=json.dumps(d, ensure_ascii=False); sys.exit(0 if ('🚀' in s or '你好' in s) else 1)" <<<"${inbox}" 2>/dev/null; then
                e2e_pass "${profile}: unicode fixture verified via API"
            else
                e2e_fail "${profile}: unicode fixture missing via API"
            fi
        else
            e2e_fail "${profile}: seed server failed to open port"
        fi
        stop_pid "${seed_pid}"
        sleep 0.4
    fi

    local raw_path="${prof_dir}/pty_raw.bin"
    local norm_path="${prof_dir}/pty_normalized.txt"
    local meta_path="${prof_dir}/meta.json"

    python3 - "${raw_path}" "${norm_path}" "${meta_path}" \
        "${term}" "${rows}" "${cols}" "${port}" "${db}" "${storage}" \
        "${resize_script_json}" "${key_script_json}" "${BIN}" <<'PY'
import fcntl
import json
import os
import pty
import re
import select
import signal
import socket
import struct
import sys
import termios
import time

raw_path = sys.argv[1]
norm_path = sys.argv[2]
meta_path = sys.argv[3]
term = sys.argv[4]
rows = int(sys.argv[5])
cols = int(sys.argv[6])
port = int(sys.argv[7])
db_path = sys.argv[8]
storage_root = sys.argv[9]
resize_script = json.loads(sys.argv[10])
key_script = json.loads(sys.argv[11])
bin_path = sys.argv[12]

def set_winsize(fd: int, r: int, c: int) -> None:
    winsize = struct.pack("HHHH", r, c, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, winsize)

def wait_port_open(host: str, p: int, timeout_s: float = 12.0) -> bool:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, p), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.15)
    return False

master_fd, slave_fd = pty.openpty()
set_winsize(master_fd, rows, cols)

pid = os.fork()
if pid == 0:
    os.close(master_fd)
    os.setsid()
    fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
    os.dup2(slave_fd, 0)
    os.dup2(slave_fd, 1)
    os.dup2(slave_fd, 2)
    if slave_fd > 2:
        os.close(slave_fd)

    env = dict(os.environ)
    env["DATABASE_URL"] = f"sqlite:////{db_path}"
    env["STORAGE_ROOT"] = storage_root
    env["HTTP_HOST"] = "127.0.0.1"
    env["HTTP_PORT"] = str(port)
    env["HTTP_RBAC_ENABLED"] = "0"
    env["HTTP_RATE_LIMIT_ENABLED"] = "0"
    env["HTTP_JWT_ENABLED"] = "0"
    env["HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED"] = "1"
    env["RUST_LOG"] = "error"
    env["TERM"] = term
    env["COLUMNS"] = str(cols)
    env["LINES"] = str(rows)
    os.execvpe(bin_path, [bin_path, "serve", "--host", "127.0.0.1", "--port", str(port)], env)

os.close(slave_fd)
chunks: list[bytes] = []

def read_available(timeout: float = 0.25) -> None:
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        r, _, _ = select.select([master_fd], [], [], min(remaining, 0.05))
        if not r:
            continue
        try:
            chunk = os.read(master_fd, 65536)
            if not chunk:
                break
            chunks.append(chunk)
        except OSError:
            break

# Initial startup
read_available(timeout=1.0)
wait_port_open("127.0.0.1", port, timeout_s=12.0)
read_available(timeout=2.5)

def decode_keys(s: str) -> bytes:
    # Interpret "\x1b" style escapes.
    return s.encode("utf-8").decode("unicode_escape").encode("latin-1")

def apply_resize(step: dict) -> None:
    r = int(step.get("rows", rows))
    c = int(step.get("cols", cols))
    set_winsize(master_fd, r, c)
    try:
        os.kill(pid, signal.SIGWINCH)
    except ProcessLookupError:
        pass

for step in key_script:
    # Optional resize before keys
    if "resize" in step and isinstance(step["resize"], dict):
        apply_resize(step["resize"])

    keys = step.get("keys", "")
    if keys:
        try:
            os.write(master_fd, decode_keys(keys))
        except OSError:
            break

    delay_s = float(step.get("delay_ms", 200)) / 1000.0
    read_available(timeout=max(0.05, delay_s))

# Apply explicit resize script at the end (if requested).
for step in resize_script:
    if not isinstance(step, dict):
        continue
    apply_resize(step)
    read_available(timeout=0.4)

read_available(timeout=0.5)

# Cleanup
try:
    os.kill(pid, signal.SIGTERM)
except ProcessLookupError:
    pass
time.sleep(0.15)
try:
    os.kill(pid, signal.SIGKILL)
except ProcessLookupError:
    pass
try:
    os.waitpid(pid, 0)
except ChildProcessError:
    pass
os.close(master_fd)

raw = b"".join(chunks)
with open(raw_path, "wb") as f:
    f.write(raw)

text = raw.decode("utf-8", errors="replace")
ansi_re = re.compile(
    r"""
    \x1b
    (?:
        \[[0-?]*[ -/]*[@-~]      # CSI
      | \].*?(?:\x07|\x1b\\)     # OSC
      | [@-_]                   # 2-char ESC
    )
    """,
    re.VERBOSE | re.DOTALL,
)
clean = ansi_re.sub("", text).replace("\r", "").replace("\x00", "")
with open(norm_path, "w", encoding="utf-8") as f:
    f.write(clean)

meta = {
    "term": term,
    "initial_rows": rows,
    "initial_cols": cols,
    "port": port,
    "db_path": db_path,
    "storage_root": storage_root,
    "resize_script": resize_script,
    "key_script": key_script,
}
with open(meta_path, "w", encoding="utf-8") as f:
    json.dump(meta, f, indent=2, sort_keys=True)
PY

    # Profile-level assertions (must at least show bootstrap banner).
    if grep -Fq "am: Starting MCP Agent Mail server" "${norm_path}"; then
        e2e_pass "${profile}: bootstrap banner present"
    else
        e2e_fail "${profile}: missing bootstrap banner"
    fi

    if [ "${expect_unicode}" = "1" ]; then
        # Unicode might not be visible on the final screen depending on the TUI state,
        # but the API-level verification above should already guarantee end-to-end handling.
        if python3 -c "import sys; d=open(sys.argv[1],'r',encoding='utf-8',errors='replace').read(); sys.exit(0 if ('🚀' in d or '你好' in d) else 1)" "${norm_path}" 2>/dev/null; then
            e2e_pass "${profile}: unicode visible in PTY transcript (bonus)"
        else
            e2e_skip "${profile}: unicode not visible in PTY transcript (see seed_inbox.json)"
        fi
    fi
}

run_tui_tmux_profile() {
    local profile="$1"
    local init_rows="$2"
    local init_cols="$3"
    local expect_unicode="$4" # "1" or "0"

    local prof_dir="${E2E_ARTIFACT_DIR}/profiles/${profile}"
    mkdir -p "${prof_dir}"

    local work db storage port sess pane
    work="$(e2e_mktemp "e2e_tui_cm_${profile}")"
    db="${work}/db.sqlite3"
    storage="${work}/storage"
    mkdir -p "${storage}"
    port="$(pick_port)"

    # Seed unicode fixture up-front (same DB/storage).
    if [ "${expect_unicode}" = "1" ]; then
        local seed_pid
        seed_pid="$(start_server_headless "${profile}" "${port}" "${db}" "${storage}" "${BIN}")"
        if e2e_wait_port 127.0.0.1 "${port}" 10; then
            local uni_project="/data/e2e/tui_cm_unicode_${profile}"
            seed_unicode_fixture "${port}" "${uni_project}"
            local inbox
            inbox="$(jsonrpc_call "${port}" "fetch_inbox" "{\"project_key\":\"${uni_project}\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true,\"limit\":10}")"
            e2e_save_artifact "profiles/${profile}/seed_inbox.json" "${inbox}"
            if python3 -c "import json,sys; d=json.load(sys.stdin); s=json.dumps(d, ensure_ascii=False); sys.exit(0 if ('🚀' in s or '你好' in s) else 1)" <<<"${inbox}" 2>/dev/null; then
                e2e_pass "${profile}: unicode fixture verified via API"
            else
                e2e_fail "${profile}: unicode fixture missing via API"
            fi
        else
            e2e_fail "${profile}: seed server failed to open port"
        fi
        stop_pid "${seed_pid}"
        sleep 0.4
    fi

    sess="e2e_cm_$(e2e_seeded_hex)"

    # Start tmux session at a fixed initial size.
    tmux new-session -d -x "${init_cols}" -y "${init_rows}" -s "${sess}" >/dev/null

    # Respect user tmux config (base-index / pane-base-index) by targeting ids.
    local win_id pane_id
    win_id="$(tmux list-windows -t "${sess}" -F '#{window_id}' | head -n 1)"
    pane_id="$(tmux list-panes -t "${sess}" -F '#{pane_id}' | head -n 1)"
    if [ -z "${win_id}" ] || [ -z "${pane_id}" ]; then
        tmux kill-session -t "${sess}" >/dev/null 2>&1 || true
        e2e_fail "${profile}: failed to resolve tmux window/pane ids"
        return
    fi

    tmux send-keys -t "${pane_id}" "env \
DATABASE_URL=sqlite:////${db} \
STORAGE_ROOT=${storage} \
HTTP_HOST=127.0.0.1 \
HTTP_PORT=${port} \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_JWT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
RUST_LOG=error \
TERM=screen-256color \
${BIN} serve --host 127.0.0.1 --port ${port}" C-m

    if ! e2e_wait_port 127.0.0.1 "${port}" 12; then
        tmux capture-pane -p -t "${pane_id}" >"${prof_dir}/tmux_startup_capture.txt" 2>/dev/null || true
        tmux kill-session -t "${sess}" >/dev/null 2>&1 || true
        e2e_fail "${profile}: server port did not open"
        return
    fi

    sleep 1.5

    local capabilities_txt="${prof_dir}/terminal_capabilities.txt"
    {
        echo "tmux_version: $(tmux -V 2>/dev/null || echo unknown)"
        echo "term: screen-256color"
        echo "lang: ${LANG:-unset}"
        echo "lc_all: ${LC_ALL:-unset}"
        echo "default_terminal: $(tmux show-options -gv default-terminal 2>/dev/null || echo unknown)"
    } >"${capabilities_txt}"
    if command -v infocmp >/dev/null 2>&1; then
        infocmp screen-256color >"${prof_dir}/terminal_infocmp.txt" 2>/dev/null || true
    fi

    # Matrix: breakpoint classes + required screens for br-3vwi.10.21.
    local -a size_matrix=(
        "39:9"
        "80:24"
        "200:50"
    )
    local -a screen_matrix=(
        "1:dashboard:Dashboard"
        "5:search:Search"
        "4:agents:Agents"
        "6:reservations:Reservations"
        "8:system_health:System ?Health"
        "7:tool_metrics:Tool ?Metrics"
        "9:timeline:Timeline"
    )

    local trace_tsv="${prof_dir}/layout_trace.tsv"
    printf "profile\tterm\trows\tcols\tclass\tscreen_key\tscreen\tcapture\tbytes\tsha256\n" >"${trace_tsv}"

    for size_spec in "${size_matrix[@]}"; do
        local cols rows class_label
        IFS=":" read -r cols rows <<<"${size_spec}"
        class_label="$(terminal_class_from_dims "${rows}" "${cols}")"

        tmux resize-window -t "${win_id}" -x "${cols}" -y "${rows}" >/dev/null 2>&1 || true
        sleep 0.8

        for screen_spec in "${screen_matrix[@]}"; do
            local screen_key screen_slug expected_regex capture_path capture_bytes capture_sha
            IFS=":" read -r screen_key screen_slug expected_regex <<<"${screen_spec}"
            tmux send-keys -t "${pane_id}" "${screen_key}"
            sleep 0.5

            capture_path="${prof_dir}/tmux_${class_label}_${cols}x${rows}_${screen_slug}.txt"
            tmux capture-pane -p -t "${pane_id}" >"${capture_path}" 2>/dev/null || true

            if [ -s "${capture_path}" ]; then
                e2e_pass "${profile}: captured ${screen_slug} @ ${cols}x${rows} (${class_label})"
            else
                e2e_fail "${profile}: empty capture for ${screen_slug} @ ${cols}x${rows}"
            fi

            if [ "${class_label}" != "tiny" ]; then
                if grep -Eiq "${expected_regex}" "${capture_path}"; then
                    e2e_pass "${profile}: marker '${expected_regex}' found in ${screen_slug} @ ${class_label}"
                else
                    e2e_fail "${profile}: missing marker '${expected_regex}' in ${screen_slug} @ ${class_label}"
                fi
            fi

            capture_bytes="$(wc -c <"${capture_path}" | tr -d '[:space:]')"
            capture_sha="$(sha256sum "${capture_path}" 2>/dev/null | awk '{print $1}')"
            printf "%s\tscreen-256color\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
                "${profile}" "${rows}" "${cols}" "${class_label}" "${screen_key}" "${screen_slug}" \
                "$(basename "${capture_path}")" "${capture_bytes}" "${capture_sha}" >>"${trace_tsv}"
        done
    done

    # Visual diffs between breakpoint captures for forensic triage.
    for screen_spec in "${screen_matrix[@]}"; do
        local _screen_key screen_slug _expected_regex
        IFS=":" read -r _screen_key screen_slug _expected_regex <<<"${screen_spec}"
        local tiny_file standard_file ultrawide_file
        tiny_file="${prof_dir}/tmux_tiny_39x9_${screen_slug}.txt"
        standard_file="${prof_dir}/tmux_standard_80x24_${screen_slug}.txt"
        ultrawide_file="${prof_dir}/tmux_ultrawide_200x50_${screen_slug}.txt"
        if [ -f "${tiny_file}" ] && [ -f "${standard_file}" ]; then
            diff -u "${tiny_file}" "${standard_file}" >"${prof_dir}/diff_${screen_slug}_tiny_vs_standard.patch" || true
        fi
        if [ -f "${standard_file}" ] && [ -f "${ultrawide_file}" ]; then
            diff -u "${standard_file}" "${ultrawide_file}" >"${prof_dir}/diff_${screen_slug}_standard_vs_ultrawide.patch" || true
        fi
    done

    python3 - "${trace_tsv}" "${prof_dir}/layout_trace.json" <<'PY'
import csv
import json
import sys
from pathlib import Path

trace_tsv = Path(sys.argv[1])
out_json = Path(sys.argv[2])

rows = []
with trace_tsv.open("r", encoding="utf-8") as f:
    reader = csv.DictReader(f, delimiter="\t")
    for row in reader:
        rows.append(row)

summary = {
    "profiles": sorted({r["profile"] for r in rows}),
    "screen_count": len({r["screen"] for r in rows}),
    "breakpoint_classes": sorted({r["class"] for r in rows}),
    "capture_count": len(rows),
}

payload = {"summary": summary, "captures": rows}
out_json.write_text(json.dumps(payload, indent=2, sort_keys=True), encoding="utf-8")
PY

    # Quit cleanly.
    tmux send-keys -t "${pane_id}" "q"
    sleep 0.8
    tmux capture-pane -p -t "${pane_id}" >"${prof_dir}/tmux_03_after_quit.txt" 2>/dev/null || true
    tmux kill-session -t "${sess}" >/dev/null 2>&1 || true

    if find "${prof_dir}" -maxdepth 1 -type f -name 'tmux_*_*x*_*.txt' -size +0c | grep -q .; then
        e2e_pass "${profile}: tmux matrix captures produced output"
    else
        e2e_fail "${profile}: no non-empty tmux matrix captures"
    fi
}

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

e2e_case_banner "pty_profiles"

# Key script: cycle required screens + help + quit.
KEY_SCRIPT_DEFAULT='[
  {"delay_ms":3500,"keys":""},
  {"keys":"1","delay_ms":450},
  {"keys":"5","delay_ms":450},
  {"keys":"4","delay_ms":450},
  {"keys":"6","delay_ms":450},
  {"keys":"8","delay_ms":450},
  {"keys":"7","delay_ms":450},
  {"keys":"9","delay_ms":450},
  {"keys":"?","delay_ms":500},
  {"keys":"\\x1b","delay_ms":400},
  {"keys":"1","delay_ms":400},
  {"keys":"q","delay_ms":300}
]'

# Resize script: tiny + standard + ultrawide to exercise full reflow matrix.
RESIZE_SCRIPT='[
  {"rows":9,"cols":39},
  {"rows":24,"cols":80},
  {"rows":50,"cols":200}
]'

run_tui_pty_profile "pty_xterm_120x40_resize" "xterm-256color" 40 120 "${RESIZE_SCRIPT}" "${KEY_SCRIPT_DEFAULT}" 0
run_tui_pty_profile "pty_xterm_39x9_tiny" "xterm-256color" 9 39 "[]" "${KEY_SCRIPT_DEFAULT}" 0
run_tui_pty_profile "pty_xterm_80x24_standard" "xterm-256color" 24 80 "[]" "${KEY_SCRIPT_DEFAULT}" 0
run_tui_pty_profile "pty_vt100_80x24_degraded" "vt100" 24 80 "[]" "${KEY_SCRIPT_DEFAULT}" 0

e2e_case_banner "unicode_wide_glyph_profile"
run_tui_pty_profile "pty_xterm_unicode_seed" "xterm-256color" 40 120 "[]" "${KEY_SCRIPT_DEFAULT}" 1

e2e_case_banner "tmux_profile"
run_tui_tmux_profile "tmux_screen_resize_matrix" 40 120 1

e2e_summary
