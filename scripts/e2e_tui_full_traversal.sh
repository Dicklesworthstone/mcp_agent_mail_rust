#!/usr/bin/env bash
# e2e_tui_full_traversal.sh — Deterministic full-screen traversal repro harness.
#
# Canonical reproduction harness for the TUI lag/flashing incident (br-legjy).
# Tabs through all 15 screens in fixed order with realistic dataset sizes,
# emitting machine-readable perf artifacts for every transition.
#
# Run via (authoritative):
#   am e2e run --project . tui_full_traversal
# Direct:
#   bash scripts/e2e_tui_full_traversal.sh
#
# Artifacts:
#   tests/artifacts/tui_full_traversal/<timestamp>/
#     traversal_results.json   — machine-readable per-screen activation latencies
#     traversal_gate_verdict.json — p95/p99 budget verdict + failing modes
#     pressure_resize_regression_report.json — event-pressure/resize-storm PTY regression metrics + budgets
#     flash_detection_report.json — frame-diff/repaint-churn flash detection verdict + failing scenarios
#     soak_regression_report.json — multi-minute soak telemetry + drift/budget verdict
#     baseline_profile_summary.json — baseline CPU/thread/syscall/redraw profile
#     cross_layer_attribution_report.json — ranked attribution map + next-track order
#     forward_transcript.txt   — normalized PTY output (Tab forward)
#     backward_transcript.txt  — normalized PTY output (Shift+Tab backward)
#     jump_transcript.txt      — normalized PTY output (direct number keys)
#     pressure_transcript.txt  — normalized PTY output (rapid mixed input)
#     resize_storm_transcript.txt — normalized PTY output (deterministic resize storm)
#     soak_transcript.txt      — normalized PTY output (multi-minute mixed soak workload)
#     seed_data.json           — data fixtures used for seeding

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_full_traversal}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Full-Screen Traversal Repro Harness (br-legjy.1.1 + br-legjy.6.1 + br-legjy.3.4 + br-legjy.6.3)"

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
FIXTURE_PROFILE="${E2E_FIXTURE_PROFILE:-medium}"
CAPTURE_BASELINE_PROFILE="${E2E_CAPTURE_BASELINE_PROFILE:-1}"
BASELINE_PROFILE_STRICT="${E2E_BASELINE_PROFILE_STRICT:-0}"
TRAVERSAL_BUDGET_QUIESCE_P95_MS="${TRAVERSAL_BUDGET_QUIESCE_P95_MS:-200}"
TRAVERSAL_BUDGET_QUIESCE_P99_MS="${TRAVERSAL_BUDGET_QUIESCE_P99_MS:-500}"
PRESSURE_BUDGET_FIRST_BYTE_P95_MS="${PRESSURE_BUDGET_FIRST_BYTE_P95_MS:-75}"
PRESSURE_BUDGET_QUIESCE_P95_MS="${PRESSURE_BUDGET_QUIESCE_P95_MS:-260}"
PRESSURE_BUDGET_QUIESCE_P99_MS="${PRESSURE_BUDGET_QUIESCE_P99_MS:-700}"
RESIZE_BUDGET_FIRST_BYTE_P95_MS="${RESIZE_BUDGET_FIRST_BYTE_P95_MS:-90}"
RESIZE_BUDGET_QUIESCE_P95_MS="${RESIZE_BUDGET_QUIESCE_P95_MS:-320}"
RESIZE_BUDGET_REPAINT_BURST_MAX="${RESIZE_BUDGET_REPAINT_BURST_MAX:-14}"
FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX="${FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX:-0.20}"
FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX="${FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX:-0.75}"
FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX="${FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX:-60}"
SOAK_DURATION_SECONDS="${SOAK_DURATION_SECONDS:-180}"
SOAK_STEP_DELAY_MS="${SOAK_STEP_DELAY_MS:-500}"
SOAK_BUDGET_FIRST_BYTE_P95_MS="${SOAK_BUDGET_FIRST_BYTE_P95_MS:-90}"
SOAK_BUDGET_QUIESCE_P95_MS="${SOAK_BUDGET_QUIESCE_P95_MS:-260}"
SOAK_BUDGET_QUIESCE_P99_MS="${SOAK_BUDGET_QUIESCE_P99_MS:-700}"
SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX="${SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX:-30}"
SOAK_BUDGET_CPU_DRIFT_PCT_MAX="${SOAK_BUDGET_CPU_DRIFT_PCT_MAX:-35}"
SOAK_BUDGET_WAKE_DRIFT_PCT_MAX="${SOAK_BUDGET_WAKE_DRIFT_PCT_MAX:-40}"
SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX="${SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX:-60}"
SOAK_BUDGET_LOW_VISIBILITY_FRACTION="${SOAK_BUDGET_LOW_VISIBILITY_FRACTION:-0.15}"
SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX="${SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX:-0.05}"

for cmd in python3 curl; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

if ! python3 -c "import pyte" >/dev/null 2>&1; then
    e2e_log "python3 pyte not found; skipping suite"
    e2e_skip "pyte required for visible terminal-state validation"
    e2e_summary
    exit 0
fi

e2e_fatal() {
    local msg="$1"
    e2e_fail "${msg}"
    e2e_summary || true
    exit 1
}

case "${FIXTURE_PROFILE}" in
    small|medium|large) ;;
    *)
        e2e_fatal "Invalid E2E_FIXTURE_PROFILE='${FIXTURE_PROFILE}'. Expected: small|medium|large"
        ;;
esac
export E2E_FIXTURE_PROFILE="${FIXTURE_PROFILE}"
e2e_log "Using fixture profile: ${FIXTURE_PROFILE}"

case "${CAPTURE_BASELINE_PROFILE}" in
    0|1) ;;
    *)
        e2e_fatal "Invalid E2E_CAPTURE_BASELINE_PROFILE='${CAPTURE_BASELINE_PROFILE}'. Expected: 0|1"
        ;;
esac

case "${BASELINE_PROFILE_STRICT}" in
    0|1) ;;
    *)
        e2e_fatal "Invalid E2E_BASELINE_PROFILE_STRICT='${BASELINE_PROFILE_STRICT}'. Expected: 0|1"
        ;;
esac

case "${TRAVERSAL_BUDGET_QUIESCE_P95_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid TRAVERSAL_BUDGET_QUIESCE_P95_MS='${TRAVERSAL_BUDGET_QUIESCE_P95_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${TRAVERSAL_BUDGET_QUIESCE_P99_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid TRAVERSAL_BUDGET_QUIESCE_P99_MS='${TRAVERSAL_BUDGET_QUIESCE_P99_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${PRESSURE_BUDGET_FIRST_BYTE_P95_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid PRESSURE_BUDGET_FIRST_BYTE_P95_MS='${PRESSURE_BUDGET_FIRST_BYTE_P95_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${PRESSURE_BUDGET_QUIESCE_P95_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid PRESSURE_BUDGET_QUIESCE_P95_MS='${PRESSURE_BUDGET_QUIESCE_P95_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${PRESSURE_BUDGET_QUIESCE_P99_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid PRESSURE_BUDGET_QUIESCE_P99_MS='${PRESSURE_BUDGET_QUIESCE_P99_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${RESIZE_BUDGET_FIRST_BYTE_P95_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid RESIZE_BUDGET_FIRST_BYTE_P95_MS='${RESIZE_BUDGET_FIRST_BYTE_P95_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${RESIZE_BUDGET_QUIESCE_P95_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid RESIZE_BUDGET_QUIESCE_P95_MS='${RESIZE_BUDGET_QUIESCE_P95_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${RESIZE_BUDGET_REPAINT_BURST_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid RESIZE_BUDGET_REPAINT_BURST_MAX='${RESIZE_BUDGET_REPAINT_BURST_MAX}'. Expected numeric ratio."
        ;;
esac
case "${FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX='${FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX}'. Expected numeric ratio."
        ;;
esac
case "${FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX='${FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX}'. Expected numeric ratio."
        ;;
esac
case "${FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX='${FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX}'. Expected numeric ratio."
        ;;
esac
case "${SOAK_DURATION_SECONDS}" in
    ''|*[!0-9]*)
        e2e_fatal "Invalid SOAK_DURATION_SECONDS='${SOAK_DURATION_SECONDS}'. Expected integer seconds."
        ;;
esac
case "${SOAK_STEP_DELAY_MS}" in
    ''|*[!0-9]*)
        e2e_fatal "Invalid SOAK_STEP_DELAY_MS='${SOAK_STEP_DELAY_MS}'. Expected integer milliseconds."
        ;;
esac
case "${SOAK_BUDGET_FIRST_BYTE_P95_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_FIRST_BYTE_P95_MS='${SOAK_BUDGET_FIRST_BYTE_P95_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${SOAK_BUDGET_QUIESCE_P95_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_QUIESCE_P95_MS='${SOAK_BUDGET_QUIESCE_P95_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${SOAK_BUDGET_QUIESCE_P99_MS}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_QUIESCE_P99_MS='${SOAK_BUDGET_QUIESCE_P99_MS}'. Expected numeric milliseconds."
        ;;
esac
case "${SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX='${SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX}'. Expected numeric percentage."
        ;;
esac
case "${SOAK_BUDGET_CPU_DRIFT_PCT_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_CPU_DRIFT_PCT_MAX='${SOAK_BUDGET_CPU_DRIFT_PCT_MAX}'. Expected numeric percentage."
        ;;
esac
case "${SOAK_BUDGET_WAKE_DRIFT_PCT_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_WAKE_DRIFT_PCT_MAX='${SOAK_BUDGET_WAKE_DRIFT_PCT_MAX}'. Expected numeric percentage."
        ;;
esac
case "${SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX='${SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX}'. Expected numeric ratio."
        ;;
esac
case "${SOAK_BUDGET_LOW_VISIBILITY_FRACTION}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_LOW_VISIBILITY_FRACTION='${SOAK_BUDGET_LOW_VISIBILITY_FRACTION}'. Expected numeric ratio."
        ;;
esac
case "${SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX}" in
    ''|*[!0-9.]*|*.*.*)
        e2e_fatal "Invalid SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX='${SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX}'. Expected numeric ratio."
        ;;
esac
if [ "${SOAK_DURATION_SECONDS}" -lt 120 ]; then
    e2e_fatal "SOAK_DURATION_SECONDS must be >=120 (multi-minute minimum), got '${SOAK_DURATION_SECONDS}'"
fi
if [ "${SOAK_STEP_DELAY_MS}" -lt 100 ]; then
    e2e_fatal "SOAK_STEP_DELAY_MS must be >=100ms, got '${SOAK_STEP_DELAY_MS}'"
fi

e2e_log "Baseline profiling capture enabled: ${CAPTURE_BASELINE_PROFILE} (strict=${BASELINE_PROFILE_STRICT})"
e2e_log "Traversal quiesce budgets: p95<=${TRAVERSAL_BUDGET_QUIESCE_P95_MS}ms, p99<=${TRAVERSAL_BUDGET_QUIESCE_P99_MS}ms"
e2e_log "Pressure budgets: first_byte_p95<=${PRESSURE_BUDGET_FIRST_BYTE_P95_MS}ms, quiesce_p95<=${PRESSURE_BUDGET_QUIESCE_P95_MS}ms, quiesce_p99<=${PRESSURE_BUDGET_QUIESCE_P99_MS}ms"
e2e_log "Resize-storm budgets: first_byte_p95<=${RESIZE_BUDGET_FIRST_BYTE_P95_MS}ms, quiesce_p95<=${RESIZE_BUDGET_QUIESCE_P95_MS}ms, repaint_burst<=${RESIZE_BUDGET_REPAINT_BURST_MAX}x"
e2e_log "Flash budgets: empty_frame_ratio<=${FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX}, frame_bounce_ratio<=${FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX}, repaint_ops_per_kb<=${FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX}"
e2e_log "Soak setup: duration=${SOAK_DURATION_SECONDS}s, step_delay=${SOAK_STEP_DELAY_MS}ms"
e2e_log "Soak budgets: first_byte_p95<=${SOAK_BUDGET_FIRST_BYTE_P95_MS}ms, quiesce_p95<=${SOAK_BUDGET_QUIESCE_P95_MS}ms, quiesce_p99<=${SOAK_BUDGET_QUIESCE_P99_MS}ms, latency_drift<=${SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX}%, cpu_drift<=${SOAK_BUDGET_CPU_DRIFT_PCT_MAX}%, wake_drift<=${SOAK_BUDGET_WAKE_DRIFT_PCT_MAX}%, repaint_ops_per_kb<=${SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX}, low_visibility<=${SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX} at <${SOAK_BUDGET_LOW_VISIBILITY_FRACTION} of p90 cells"

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
# Screen metadata — canonical tab order (must match ALL_SCREEN_IDS)
# ────────────────────────────────────────────────────────────────────
SCREEN_NAMES=(
    "Dashboard"
    "Messages"
    "Threads"
    "Agents"
    "Search"
    "Reservations"
    "Tool Metrics"
    "System Health"
    "Timeline"
    "Projects"
    "Contacts"
    "Explorer"
    "Analytics"
    "Attachments"
    "Archive Browser"
)
SCREEN_COUNT=${#SCREEN_NAMES[@]}

# Tab bar short labels (for grep-based verification in 80-col terminal)
SCREEN_SHORT_LABELS=(
    "Dash"
    "Msg"
    "Threads"
    "Agents"
    "Find"
    "Reserv"
    "Tools"
    "Health"
    "Time"
    "Proj"
    "Links"
    "Explore"
    "Insight"
    "Attach"
    "Archive"
)

# Direct-jump keys: 1-9 for screens 1-9, 0 for screen 10, !@#$% for 11-15
JUMP_KEYS=("1" "2" "3" "4" "5" "6" "7" "8" "9" "0" "!" "@" "#" "\$" "%")
BACKTAB_KEY_JSON="\\u001b[Z"

# ────────────────────────────────────────────────────────────────────
# Enhanced PTY interaction with per-keystroke timing
# ────────────────────────────────────────────────────────────────────
# Like run_pty_interaction but captures timestamps for each keystroke
# and writes a machine-readable timing JSON alongside the transcript.
run_timed_pty_interaction() {
    local label="$1"
    local output_file="$2"
    local timing_file="$3"
    local keystroke_script="$4"
    shift 4

    local raw_output="${E2E_ARTIFACT_DIR}/pty_${label}_raw.txt"
    local pty_stderr="${E2E_ARTIFACT_DIR}/pty_${label}_stderr.txt"
    e2e_log "PTY timed interaction (${label}): running ${*}"

    python3 - "${raw_output}" "${timing_file}" "${keystroke_script}" "$@" <<'PYEOF' 2>"${pty_stderr}"
import codecs
import datetime
import hashlib
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import time
import fcntl
import termios
import shutil
import pyte

output_file = sys.argv[1]
timing_file = sys.argv[2]
keystroke_script = json.loads(sys.argv[3])
cmd = sys.argv[4:]

# Open a PTY
master_fd, slave_fd = pty.openpty()

# Optional baseline profiler capture controls (per-run)
profile_capture = os.environ.get("E2E_PROFILE_CAPTURE", "0") == "1"
profile_dir = os.environ.get("E2E_PROFILE_DIR", "").strip()
profile_label = os.environ.get("E2E_PROFILE_LABEL", "profile")
if profile_capture and not profile_dir:
    profile_dir = os.path.join(os.path.dirname(timing_file), "baseline_profile")

# Set terminal size (80x24 is standard; 120x40 for more screen detail)
cols = int(os.environ.get("E2E_PTY_COLS", "120"))
rows = int(os.environ.get("E2E_PTY_ROWS", "40"))
winsize = struct.pack("HHHH", rows, cols, 0, 0)
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
    env["COLUMNS"] = str(cols)
    env["LINES"] = str(rows)
    os.execvpe(cmd[0], cmd, env)
else:
    # Parent: drive interaction with timing
    os.close(slave_fd)
    chunks = []
    timings = []
    terminal_screen = pyte.Screen(cols, rows)
    terminal_stream = pyte.Stream(terminal_screen)
    terminal_decoder = codecs.getincrementaldecoder("utf-8")("replace")
    profile_processes = []
    profile_meta = {
        "enabled": profile_capture,
        "label": profile_label,
        "capture_dir": profile_dir if profile_capture else None,
        "child_pid": pid,
        "captured_at_utc": datetime.datetime.utcnow().replace(microsecond=0).isoformat() + "Z",
        "tools": {
            "pidstat_available": shutil.which("pidstat") is not None,
            "strace_available": shutil.which("strace") is not None,
        },
        "tool_runs": [],
    }

    # Quiescence gap: if no output arrives within this window after the last
    # byte, we consider the screen render "done".  Kept tight for profiling.
    QUIESCE_GAP_S = float(os.environ.get("E2E_PTY_QUIESCE_MS", "80")) / 1000.0
    INITIAL_RENDER_MAX_WAIT_S = float(os.environ.get("E2E_PTY_INITIAL_RENDER_MAX_WAIT_MS", "12000")) / 1000.0

    def start_profile_tool(name, cmd_args, stdout_path):
        stderr_path = f"{stdout_path}.stderr.txt"
        stdout_fh = open(stdout_path, "w", encoding="utf-8")
        stderr_fh = open(stderr_path, "w", encoding="utf-8")
        proc = subprocess.Popen(cmd_args, stdout=stdout_fh, stderr=stderr_fh)
        profile_processes.append({
            "name": name,
            "proc": proc,
            "stdout_fh": stdout_fh,
            "stderr_fh": stderr_fh,
            "stdout_path": stdout_path,
            "stderr_path": stderr_path,
            "cmd": cmd_args,
        })
        profile_meta["tool_runs"].append({
            "name": name,
            "command": cmd_args,
            "stdout_path": stdout_path,
            "stderr_path": stderr_path,
            "started": True,
        })

    if profile_capture:
        os.makedirs(profile_dir, exist_ok=True)
        if profile_meta["tools"]["pidstat_available"]:
            start_profile_tool(
                "pidstat_process",
                ["pidstat", "-u", "-h", "-p", str(pid), "1"],
                os.path.join(profile_dir, f"{profile_label}_pidstat_process.txt"),
            )
            start_profile_tool(
                "pidstat_threads",
                ["pidstat", "-u", "-h", "-t", "-p", str(pid), "1"],
                os.path.join(profile_dir, f"{profile_label}_pidstat_threads.txt"),
            )
            start_profile_tool(
                "pidstat_wake",
                ["pidstat", "-w", "-h", "-t", "-p", str(pid), "1"],
                os.path.join(profile_dir, f"{profile_label}_pidstat_wake.txt"),
            )
        if profile_meta["tools"]["strace_available"]:
            start_profile_tool(
                "strace",
                ["strace", "-f", "-tt", "-T", "-p", str(pid), "-o", os.path.join(profile_dir, f"{profile_label}_strace.log")],
                os.path.join(profile_dir, f"{profile_label}_strace_runner_stdout.txt"),
            )

    def capture_chunk(chunk):
        chunks.append(chunk)
        try:
            terminal_stream.feed(terminal_decoder.decode(chunk))
        except Exception:
            # Preserve the raw capture even if a terminal-specific extension
            # is not understood by the emulator.
            pass

    def visible_terminal_state():
        display = terminal_screen.display
        nonblank_cells = sum(1 for row in display for char in row if not char.isspace())
        nonblank_lines = sum(1 for row in display if row.strip())
        total_cells = max(1, terminal_screen.columns * terminal_screen.lines)
        return {
            "terminal_nonblank_cells": nonblank_cells,
            "terminal_nonblank_lines": nonblank_lines,
            "terminal_occupancy_ratio": round(nonblank_cells / total_cells, 6),
            "terminal_columns": terminal_screen.columns,
            "terminal_lines": terminal_screen.lines,
        }

    def read_available(timeout=0.3):
        """Read all available output until timeout, return bytes read."""
        deadline = time.monotonic() + timeout
        got = 0
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
                    capture_chunk(chunk)
                    got += len(chunk)
                except OSError:
                    break
        return got

    ALT_SCREEN_ENTER_BYTES = b"\x1b[?1049h"

    def read_with_latency(max_wait=2.0, min_capture_ms=0.0, require_alt_screen=False):
        """Read output tracking first-byte and quiescence timing.

        Returns dict with:
          first_byte_ms  - time from call to first output byte (None if no output)
          quiesce_ms     - time from call to output quiescence
          render_ms      - time from first byte to quiescence (actual render)
          total_bytes    - bytes received during this call
          saw_alt_screen - whether alt-screen enter was observed in this window
        """
        t0 = time.monotonic()
        deadline = t0 + max_wait
        got = 0
        first_byte_t = None
        last_byte_t = None
        saw_alt_screen = False

        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            # After we have some output, use the quiescence gap as our
            # select timeout so we detect when output stops flowing.
            if first_byte_t is not None:
                wait = min(remaining, QUIESCE_GAP_S)
            else:
                wait = min(remaining, 0.05)
            ready, _, _ = select.select([master_fd], [], [], wait)
            if ready:
                try:
                    chunk = os.read(master_fd, 65536)
                    if not chunk:
                        break
                    capture_chunk(chunk)
                    now = time.monotonic()
                    got += len(chunk)
                    if first_byte_t is None:
                        first_byte_t = now
                    last_byte_t = now
                    if ALT_SCREEN_ENTER_BYTES in chunk:
                        saw_alt_screen = True
                except OSError:
                    break
            else:
                # select timed out with no data
                if first_byte_t is not None:
                    now = time.monotonic()
                    # During startup we can see brief banner output before the
                    # fullscreen renderer enters alt-screen. Keep waiting (up to
                    # max_wait) when requested so we don't truncate captures to
                    # startup text only.
                    if require_alt_screen and not saw_alt_screen and now < deadline:
                        continue
                    if min_capture_ms > 0 and ((now - first_byte_t) * 1000.0) < min_capture_ms and now < deadline:
                        continue
                    # We had output and now it stopped — quiesced.
                    break

        now = time.monotonic()
        return {
            "first_byte_ms": round((first_byte_t - t0) * 1000, 2) if first_byte_t else None,
            "quiesce_ms": round((now - t0) * 1000, 2),
            "render_ms": round(((last_byte_t or now) - (first_byte_t or t0)) * 1000, 2) if first_byte_t else 0,
            "total_bytes": got,
            "saw_alt_screen": saw_alt_screen,
        }

    ansi_step_re = re.compile(r"\x1b(?:\[[0-9;?]*[ -/]*[@-~]|\].*?(?:\x07|\x1b\\)|[\x40-\x5f]|\([\x20-\x7e]|\)[\x20-\x7e])")

    # Initial read: wait for TUI to render
    t_start = time.monotonic()
    init = read_with_latency(
        max_wait=INITIAL_RENDER_MAX_WAIT_S,
        min_capture_ms=1000.0,
        require_alt_screen=True,
    )
    timings.append({
        "step": "initial_render",
        "first_byte_ms": init["first_byte_ms"],
        "quiesce_ms": init["quiesce_ms"],
        "render_ms": init["render_ms"],
        "output_bytes": init["total_bytes"],
        "saw_alt_screen": init["saw_alt_screen"],
        **visible_terminal_state(),
    })

    def apply_resize(step):
        resize = step.get("resize")
        if not isinstance(resize, dict):
            return None
        try:
            step_rows = int(resize.get("rows", rows))
            step_cols = int(resize.get("cols", cols))
        except (TypeError, ValueError):
            return None
        step_rows = max(8, min(step_rows, 120))
        step_cols = max(24, min(step_cols, 320))
        winsize_step = struct.pack("HHHH", step_rows, step_cols, 0, 0)
        fcntl.ioctl(master_fd, termios.TIOCSWINSZ, winsize_step)
        terminal_screen.resize(lines=step_rows, columns=step_cols)
        try:
            os.kill(pid, signal.SIGWINCH)
        except ProcessLookupError:
            pass
        return {"rows": step_rows, "cols": step_cols}

    # Execute keystroke script with timing
    for i, step in enumerate(keystroke_script):
        max_wait_s = step.get("delay_ms", 200) / 1000.0
        keys = step.get("keys", "")
        step_label = step.get("label", f"step_{i}")
        resize_applied = None

        t_before = time.monotonic()
        chunk_start_idx = len(chunks)
        resize_applied = apply_resize(step)

        if keys:
            # Decode escape sequences in key strings
            keys_bytes = keys.encode("utf-8").decode("unicode_escape").encode("latin-1")
            try:
                os.write(master_fd, keys_bytes)
            except OSError:
                break

        step_min_capture_ms = float(step.get("min_capture_ms", 0) or 0)
        r = read_with_latency(max_wait=max_wait_s, min_capture_ms=step_min_capture_ms)
        t_after = time.monotonic()
        segment_raw = b"".join(chunks[chunk_start_idx:])
        segment_text = segment_raw.decode("utf-8", errors="replace")
        segment_clean = ansi_step_re.sub("", segment_text).replace("\r", "").replace("\x00", "")
        frame_hash = hashlib.sha1(segment_clean.encode("utf-8")).hexdigest()[:16] if segment_clean else "empty"
        frame_cursor_home_ops = len(re.findall(r"\x1b\[[0-9;]*H", segment_text))
        frame_clear_ops = len(re.findall(r"\x1b\[[0-9;]*2J", segment_text))
        frame_erase_line_ops = len(re.findall(r"\x1b\[[0-9;]*K", segment_text))
        frame_nonempty_lines = sum(1 for line in segment_clean.splitlines() if line.strip())

        if resize_applied and keys:
            input_kind = "resize+keys"
        elif resize_applied:
            input_kind = "resize_only"
        elif keys:
            input_kind = "keys_only"
        else:
            input_kind = "noop"

        timings.append({
            "step": step_label,
            "keys": keys,
            "input_kind": input_kind,
            "resize": resize_applied,
            "max_wait_ms": step.get("delay_ms", 200),
            "min_capture_ms": step_min_capture_ms,
            "wall_ms": round((t_after - t_before) * 1000, 2),
            "first_byte_ms": r["first_byte_ms"],
            "quiesce_ms": r["quiesce_ms"],
            "render_ms": r["render_ms"],
            "output_bytes_delta": r["total_bytes"],
            "frame_hash": frame_hash,
            "frame_chars": len(segment_clean),
            "frame_nonempty_lines": frame_nonempty_lines,
            "frame_cursor_home_ops": frame_cursor_home_ops,
            "frame_clear_ops": frame_clear_ops,
            "frame_erase_line_ops": frame_erase_line_ops,
            "saw_alt_screen": r["saw_alt_screen"],
            **visible_terminal_state(),
        })

    # Final read
    read_available(timeout=0.5)
    t_end = time.monotonic()
    timings.append({
        "step": "total",
        "wall_ms": round((t_end - t_start) * 1000, 2),
        "total_output_bytes": sum(len(c) for c in chunks),
    })

    # Stop profiling tools before tearing down child.
    for entry in profile_processes:
        proc = entry["proc"]
        if proc.poll() is not None:
            continue
        if entry["name"] == "strace":
            try:
                proc.send_signal(signal.SIGINT)
            except ProcessLookupError:
                pass
        else:
            proc.terminate()
    for entry in profile_processes:
        proc = entry["proc"]
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2)
        entry["stdout_fh"].close()
        entry["stderr_fh"].close()
        entry["returncode"] = proc.returncode

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
    raw_text = output.decode("utf-8", errors="replace")
    ansi_metrics = {
        "raw_output_bytes": len(output),
        "clear_screen_ops": len(re.findall(r"\x1b\[[0-9;]*2J", raw_text)),
        "erase_line_ops": len(re.findall(r"\x1b\[[0-9;]*K", raw_text)),
        "cursor_home_ops": len(re.findall(r"\x1b\[[0-9;]*H", raw_text)),
        "alt_screen_enter_ops": raw_text.count("\x1b[?1049h"),
        "alt_screen_exit_ops": raw_text.count("\x1b[?1049l"),
    }

    # Summarize syscall profile evidence from strace output.
    def summarize_strace(path):
        summary = {
            "path": path,
            "line_count": 0,
            "syscall_counts": {},
            "wait_syscalls": {
                "total_count": 0,
                "timedout_count": 0,
                "short_wait_le_5ms": 0,
            },
            "zero_timeout_polls": 0,
            "write_syscalls": {
                "count": 0,
                "bytes_returned": 0,
            },
        }
        if not path or not os.path.exists(path):
            return summary

        wait_names = {
            "futex", "poll", "ppoll", "epoll_wait", "epoll_pwait",
            "select", "pselect6", "nanosleep", "clock_nanosleep",
        }

        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                m = re.match(r"^\d+\s+\d{2}:\d{2}:\d{2}\.\d+\s+([a-zA-Z0-9_]+)\(", line)
                if not m:
                    continue
                name = m.group(1)
                summary["line_count"] += 1
                summary["syscall_counts"][name] = summary["syscall_counts"].get(name, 0) + 1

                duration = None
                d = re.search(r"<([0-9]+\.[0-9]+)>$", line)
                if d:
                    try:
                        duration = float(d.group(1))
                    except ValueError:
                        duration = None

                if name in wait_names:
                    summary["wait_syscalls"]["total_count"] += 1
                    if "ETIMEDOUT" in line:
                        summary["wait_syscalls"]["timedout_count"] += 1
                    if duration is not None and duration <= 0.005:
                        summary["wait_syscalls"]["short_wait_le_5ms"] += 1

                if name == "poll" and re.search(r"poll\([^,]+,\s*[^,]+,\s*0\)", line):
                    summary["zero_timeout_polls"] += 1
                if name == "ppoll" and "tv_sec=0" in line and "tv_nsec=0" in line:
                    summary["zero_timeout_polls"] += 1
                if name in ("epoll_wait", "epoll_pwait") and re.search(r",\s*0\)\s*=", line):
                    summary["zero_timeout_polls"] += 1

                if name in ("write", "writev"):
                    summary["write_syscalls"]["count"] += 1
                    r = re.search(r"=\s*(-?\d+)", line)
                    if r:
                        n = int(r.group(1))
                        if n > 0:
                            summary["write_syscalls"]["bytes_returned"] += n

        summary["top_syscalls"] = sorted(
            summary["syscall_counts"].items(),
            key=lambda kv: kv[1],
            reverse=True,
        )[:12]
        return summary

    strace_log_path = ""
    for entry in profile_processes:
        if entry["name"] == "strace":
            # Strace writes real trace to the -o path in its command.
            try:
                idx = entry["cmd"].index("-o")
                strace_log_path = entry["cmd"][idx + 1]
            except Exception:
                strace_log_path = ""

    strace_summary = summarize_strace(strace_log_path)
    profile_meta["strace_summary"] = strace_summary
    profile_meta["tool_runs"] = [
        {
            "name": entry["name"],
            "command": entry["cmd"],
            "stdout_path": entry["stdout_path"],
            "stderr_path": entry["stderr_path"],
            "returncode": entry.get("returncode"),
        }
        for entry in profile_processes
    ]

    if profile_capture and profile_dir:
        meta_path = os.path.join(profile_dir, f"{profile_label}_profile_meta.json")
        with open(meta_path, "w", encoding="utf-8") as f:
            json.dump(profile_meta, f, indent=2)
        profile_meta["meta_path"] = meta_path

    # Strip ANSI escape sequences
    text = raw_text
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

    with open(timing_file, "w") as f:
        json.dump({
            "timings": timings,
            "ansi_metrics": ansi_metrics,
            "profile": profile_meta,
        }, f, indent=2)

    sys.exit(0)
PYEOF

    local rc=$?
    if [ -s "${pty_stderr}" ]; then
        e2e_save_artifact "pty_${label}_stderr.txt" "$(cat "${pty_stderr}")"
    fi
    if [ $rc -ne 0 ]; then
        e2e_log "PTY timed interaction (${label}) failed (rc=${rc})"
    fi
    e2e_save_artifact "pty_${label}_normalized.txt" "$(cat "${raw_output}" 2>/dev/null || echo '<empty>')"
    if [ -f "${raw_output}" ]; then
        cp "${raw_output}" "${output_file}"
    fi
    return $rc
}

# Helper: check transcript contains either canonical label or fallback short label
assert_transcript_contains_any() {
    local label="$1"
    local file="$2"
    local needle_primary="$3"
    local needle_fallback="${4:-}"
    if grep -qF "${needle_primary}" "${file}" 2>/dev/null || \
        { [ -n "${needle_fallback}" ] && grep -qF "${needle_fallback}" "${file}" 2>/dev/null; }; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}: neither '${needle_primary}' nor '${needle_fallback}' found in transcript"
        # Show last 20 lines for debugging
        e2e_log "Transcript tail:"
        tail -20 "${file}" 2>/dev/null | while IFS= read -r line; do
            e2e_log "  | ${line}"
        done
    fi
}

BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

# Common environment setup for a fresh server
setup_server_env() {
    local label="$1"
    local work_dir
    work_dir="$(e2e_mktemp "e2e_tui_traversal_${label}")"
    local db_path="${work_dir}/db.sqlite3"
    local storage_root="${work_dir}/storage"
    mkdir -p "${storage_root}"
    echo "${work_dir} ${db_path} ${storage_root}"
}

# ────────────────────────────────────────────────────────────────────
# Seed realistic test data to stress real hotpaths
# ────────────────────────────────────────────────────────────────────
# Creates: 1 project, 5 agents, 30 messages across 6 threads,
# 3 file reservations, contacts — enough data to fill all screens
# with realistic content for profiling.
seed_realistic_data() {
    local port="$1"
    local fixture_profile="${2:-medium}"
    local url="http://127.0.0.1:${port}/mcp/"

    local project_key="/tmp/e2e-traversal-project"
    SEED_CALL_SEQ="${SEED_CALL_SEQ:-0}"

    e2e_log "Seeding realistic test data..."

    local agent_count=5
    local thread_count=6
    local replies_per_thread=4
    case "${fixture_profile}" in
        small)
            agent_count=3
            thread_count=3
            replies_per_thread=2
            ;;
        medium)
            agent_count=5
            thread_count=6
            replies_per_thread=4
            ;;
        large)
            agent_count=8
            thread_count=10
            replies_per_thread=6
            ;;
        *)
            e2e_fatal "Unknown fixture profile '${fixture_profile}'"
            ;;
    esac

    # 1. Ensure project
    SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
    e2e_rpc_call "seed_project" "${url}" "ensure_project" \
        "{\"human_key\":\"${project_key}\"}" >/dev/null 2>&1 || true

    # 2. Register agents with different programs/models
    local all_agents=("RedFox" "BlueLake" "GreenPeak" "GoldCastle" "SwiftHawk" "IvoryWolf" "AmberFinch" "TealComet")
    local all_programs=("claude-code" "codex-cli" "gemini-cli" "claude-code" "codex-cli" "gemini-cli" "claude-code" "codex-cli")
    local all_models=("opus-4.6" "gpt-5" "gemini-2.5-pro" "sonnet-4.6" "gpt-5-codex" "gemini-2.5-flash" "opus-4.6" "gpt-5-codex")
    local agents=("${all_agents[@]:0:${agent_count}}")
    local programs=("${all_programs[@]:0:${agent_count}}")
    local models=("${all_models[@]:0:${agent_count}}")

    for i in "${!agents[@]}"; do
        SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
        e2e_rpc_call "seed_agent_${agents[$i]}" "${url}" "register_agent" \
            "{\"project_key\":\"${project_key}\",\"program\":\"${programs[$i]}\",\"model\":\"${models[$i]}\",\"name\":\"${agents[$i]}\",\"task_description\":\"E2E traversal test agent ${i}\"}" >/dev/null 2>&1 || true
    done

    # 3. Send messages across multiple threads
    local all_threads=("FEAT-001" "BUG-042" "REFACTOR-7" "DOCS-12" "PERF-99" "OPS-3" "UI-88" "SEARCH-51" "AUTH-17" "TOOLS-61")
    local all_subjects=(
        "Implement user authentication module"
        "Fix race condition in connection pool"
        "Refactor query builder to use prepared statements"
        "Update API documentation for v2 endpoints"
        "Optimize hot-loop in message dispatcher"
        "Deploy monitoring dashboards"
        "Polish tab focus transitions for TUI controls"
        "Investigate search ranking drift in mixed corpora"
        "Stabilize JWT claims parsing on malformed payloads"
        "Harden tool argument normalization and edge-path validation"
    )
    local threads=("${all_threads[@]:0:${thread_count}}")
    local subjects=("${all_subjects[@]:0:${thread_count}}")

    local msg_idx=0
    for t in "${!threads[@]}"; do
        local thread="${threads[$t]}"
        local subj="${subjects[$t]}"
        # Initial message
        local from_idx=$((t % ${#agents[@]}))
        local to_idx=$(((t + 1) % ${#agents[@]}))
        SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
        e2e_rpc_call "seed_msg_${msg_idx}" "${url}" "send_message" \
            "{\"project_key\":\"${project_key}\",\"sender_name\":\"${agents[$from_idx]}\",\"to\":[\"${agents[$to_idx]}\"],\"subject\":\"[${thread}] ${subj}\",\"body_md\":\"Starting work on ${subj}. This is message ${msg_idx} in thread ${thread}.\",\"thread_id\":\"${thread}\",\"importance\":\"normal\"}" >/dev/null 2>&1 || true
        msg_idx=$((msg_idx + 1))

        # Configurable number of replies per thread
        for r in $(seq 1 ${replies_per_thread}); do
            local r_from=$((($from_idx + r) % ${#agents[@]}))
            local r_to=$((($to_idx + r) % ${#agents[@]}))
            SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
            e2e_rpc_call "seed_msg_${msg_idx}" "${url}" "send_message" \
                "{\"project_key\":\"${project_key}\",\"sender_name\":\"${agents[$r_from]}\",\"to\":[\"${agents[$r_to]}\"],\"subject\":\"Re: [${thread}] ${subj}\",\"body_md\":\"Reply ${r}: Progress update on ${subj}. Benchmark results look promising with ${r}0% improvement.\",\"thread_id\":\"${thread}\",\"importance\":\"normal\"}" >/dev/null 2>&1 || true
            msg_idx=$((msg_idx + 1))
        done
    done

    # 4. Create file reservations
    SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
    e2e_rpc_call "seed_reservation_1" "${url}" "file_reservation_paths" \
        "{\"project_key\":\"${project_key}\",\"agent_name\":\"${agents[0]}\",\"file_paths\":[\"crates/mcp-agent-mail-core/src/**\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"FEAT-001\"}" >/dev/null 2>&1 || true

    SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
    e2e_rpc_call "seed_reservation_2" "${url}" "file_reservation_paths" \
        "{\"project_key\":\"${project_key}\",\"agent_name\":\"${agents[1]}\",\"file_paths\":[\"crates/mcp-agent-mail-db/src/**\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"BUG-042\"}" >/dev/null 2>&1 || true

    SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
    e2e_rpc_call "seed_reservation_3" "${url}" "file_reservation_paths" \
        "{\"project_key\":\"${project_key}\",\"agent_name\":\"${agents[2]}\",\"file_paths\":[\"crates/mcp-agent-mail-tools/src/**\"],\"ttl_seconds\":1800,\"exclusive\":false,\"reason\":\"DOCS-12\"}" >/dev/null 2>&1 || true

    # 5. Create contacts
    SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
    e2e_rpc_call "seed_contact_1" "${url}" "request_contact" \
        "{\"project_key\":\"${project_key}\",\"from_agent\":\"${agents[0]}\",\"to_agent\":\"${agents[1]}\",\"reason\":\"Need to coordinate on DB changes\"}" >/dev/null 2>&1 || true

    SEED_CALL_SEQ=$((SEED_CALL_SEQ + 1))
    e2e_rpc_call "seed_contact_accept_1" "${url}" "respond_contact" \
        "{\"project_key\":\"${project_key}\",\"from_agent\":\"${agents[1]}\",\"to_agent\":\"${agents[0]}\",\"accept\":true}" >/dev/null 2>&1 || true

    # Save seed summary as artifact
    e2e_save_artifact "seed_data.json" "$(cat <<SEEDJSON
{
  "project_key": "${project_key}",
  "fixture_profile": "${fixture_profile}",
  "agents": ${#agents[@]},
  "messages": ${msg_idx},
  "threads": ${#threads[@]},
  "replies_per_thread": ${replies_per_thread},
  "reservations": 3,
  "contacts": 1,
  "fixture_matrix": {
    "small": {"agents": 3, "threads": 3, "replies_per_thread": 2},
    "medium": {"agents": 5, "threads": 6, "replies_per_thread": 4},
    "large": {"agents": 8, "threads": 10, "replies_per_thread": 6}
  },
  "agent_names": $(printf '%s\n' "${agents[@]}" | python3 -c "import sys,json; print(json.dumps([l.strip() for l in sys.stdin]))")
}
SEEDJSON
)"

    e2e_log "Seeded (${fixture_profile}): ${#agents[@]} agents, ${msg_idx} messages, ${#threads[@]} threads, 3 reservations"
}

# Wait for HTTP server to become reachable
wait_for_server() {
    local port="$1"
    local max_wait="${2:-15}"
    local url="http://127.0.0.1:${port}/mcp/"

    e2e_log "Waiting for server on port ${port}..."
    local i=0
    while [ $i -lt "$max_wait" ]; do
        if curl -sS -o /dev/null -w '' --connect-timeout 1 \
            -X POST "${url}" \
            -H "content-type: application/json" \
            --data '{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"health_check","arguments":{}}}' 2>/dev/null; then
            e2e_log "Server ready on port ${port}"
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    e2e_log "Server did not start within ${max_wait}s"
    return 1
}

# ────────────────────────────────────────────────────────────────────
# Case 1: Forward traversal — Tab through all 15 screens
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "forward_traversal_all_15_screens"

read -r WORK1 DB1 STORAGE1 <<< "$(setup_server_env "forward")"
PORT1="$(pick_port)"

# Seed data via HTTP first (start headless server, seed, stop, then start TUI)
HEADLESS_LOG="${E2E_ARTIFACT_DIR}/logs/headless_server.log"
mkdir -p "$(dirname "${HEADLESS_LOG}")"

DATABASE_URL="sqlite:////${DB1}" \
STORAGE_ROOT="${STORAGE1}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PORT1}" \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
TUI_ENABLED=0 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT1}" --no-tui &
HEADLESS_PID=$!

if wait_for_server "${PORT1}" 15; then
    seed_realistic_data "${PORT1}" "${FIXTURE_PROFILE}"
else
    e2e_fatal "Headless server failed to start for data seeding"
fi

kill "${HEADLESS_PID}" 2>/dev/null || true
wait "${HEADLESS_PID}" 2>/dev/null || true
sleep 1

# Now run TUI with PTY interaction — Tab through all 15 screens
TRANSCRIPT1="${E2E_ARTIFACT_DIR}/forward_transcript.txt"
TIMING1="${E2E_ARTIFACT_DIR}/forward_timing.json"

# Build keystroke script: wait for render, then Tab 15 times (full cycle + wrap), then quit
KEYS1='['
KEYS1+='{"delay_ms": 2000, "keys": "", "label": "initial_render"}'
for i in $(seq 1 ${SCREEN_COUNT}); do
    screen_idx=$((i % SCREEN_COUNT))
    KEYS1+=",{\"delay_ms\": 600, \"keys\": \"\\t\", \"label\": \"tab_to_${SCREEN_NAMES[$screen_idx]// /_}\"}"
done
# One more Tab to verify wrap-around back to Dashboard
KEYS1+=",{\"delay_ms\": 600, \"keys\": \"\\t\", \"label\": \"tab_wrap_to_Messages\"}"
KEYS1+=",{\"delay_ms\": 300, \"keys\": \"q\", \"label\": \"quit\"}"
KEYS1+=']'

if ! run_timed_pty_interaction "forward" "${TRANSCRIPT1}" "${TIMING1}" "${KEYS1}" \
    env \
    DATABASE_URL="sqlite:////${DB1}" \
    STORAGE_ROOT="${STORAGE1}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT1}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT1}"; then
    e2e_fatal "forward traversal PTY interaction failed"
fi

# Verify all 15 screens appeared in the transcript
for i in "${!SCREEN_NAMES[@]}"; do
    assert_transcript_contains_any \
        "forward: screen ${i}: ${SCREEN_NAMES[$i]}" \
        "${TRANSCRIPT1}" \
        "${SCREEN_NAMES[$i]}" \
        "${SCREEN_SHORT_LABELS[$i]}"
done

# Verify timing artifact was written
if [ -f "${TIMING1}" ] && python3 -c "import json; d=json.load(open('${TIMING1}')); assert len(d['timings']) > 0" 2>/dev/null; then
    e2e_pass "forward: timing artifact valid JSON with entries"
else
    e2e_fail "forward: timing artifact missing or invalid"
fi


# ────────────────────────────────────────────────────────────────────
# Case 2: Backward traversal — Shift+Tab through all 15 screens
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "backward_traversal_all_15_screens"

read -r WORK2 DB2 STORAGE2 <<< "$(setup_server_env "backward")"
PORT2="$(pick_port)"

# Seed via headless
DATABASE_URL="sqlite:////${DB2}" \
STORAGE_ROOT="${STORAGE2}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PORT2}" \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
TUI_ENABLED=0 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT2}" --no-tui &
HEADLESS_PID2=$!

if wait_for_server "${PORT2}" 15; then
    seed_realistic_data "${PORT2}" "${FIXTURE_PROFILE}"
else
    e2e_fatal "Headless server failed to start for backward traversal seeding"
fi

kill "${HEADLESS_PID2}" 2>/dev/null || true
wait "${HEADLESS_PID2}" 2>/dev/null || true
sleep 1

TRANSCRIPT2="${E2E_ARTIFACT_DIR}/backward_transcript.txt"
TIMING2="${E2E_ARTIFACT_DIR}/backward_timing.json"

# Shift+Tab (ESC [ Z) goes backward: Dashboard -> ArchiveBrowser -> Attachments -> ...
KEYS2='['
KEYS2+='{"delay_ms": 2000, "keys": "", "label": "initial_render"}'
for i in $(seq 1 ${SCREEN_COUNT}); do
    rev_idx=$(( (SCREEN_COUNT - i) % SCREEN_COUNT ))
    KEYS2+=",{\"delay_ms\": 600, \"keys\": \"${BACKTAB_KEY_JSON}\", \"label\": \"backtab_to_${SCREEN_NAMES[$rev_idx]// /_}\"}"
done
KEYS2+=",{\"delay_ms\": 300, \"keys\": \"q\", \"label\": \"quit\"}"
KEYS2+=']'

if ! run_timed_pty_interaction "backward" "${TRANSCRIPT2}" "${TIMING2}" "${KEYS2}" \
    env \
    DATABASE_URL="sqlite:////${DB2}" \
    STORAGE_ROOT="${STORAGE2}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT2}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT2}"; then
    e2e_fatal "backward traversal PTY interaction failed"
fi

# Verify all screens visited backward
for i in "${!SCREEN_NAMES[@]}"; do
    assert_transcript_contains_any \
        "backward: screen ${i}: ${SCREEN_NAMES[$i]}" \
        "${TRANSCRIPT2}" \
        "${SCREEN_NAMES[$i]}" \
        "${SCREEN_SHORT_LABELS[$i]}"
done

if [ -f "${TIMING2}" ] && python3 -c "import json; d=json.load(open('${TIMING2}')); assert len(d['timings']) > 0" 2>/dev/null; then
    e2e_pass "backward: timing artifact valid"
else
    e2e_fail "backward: timing artifact missing or invalid"
fi


# ────────────────────────────────────────────────────────────────────
# Case 3: Direct jump — Number keys hit all 15 screens
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "direct_jump_all_15_screens"

read -r WORK3 DB3 STORAGE3 <<< "$(setup_server_env "jump")"
PORT3="$(pick_port)"

# Seed via headless
DATABASE_URL="sqlite:////${DB3}" \
STORAGE_ROOT="${STORAGE3}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PORT3}" \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
TUI_ENABLED=0 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT3}" --no-tui &
HEADLESS_PID3=$!

if wait_for_server "${PORT3}" 15; then
    seed_realistic_data "${PORT3}" "${FIXTURE_PROFILE}"
else
    e2e_fatal "Headless server failed to start for jump traversal seeding"
fi

kill "${HEADLESS_PID3}" 2>/dev/null || true
wait "${HEADLESS_PID3}" 2>/dev/null || true
sleep 1

TRANSCRIPT3="${E2E_ARTIFACT_DIR}/jump_transcript.txt"
TIMING3="${E2E_ARTIFACT_DIR}/jump_timing.json"

# Build keystroke script for direct jumps: 1,2,3,...,9,0,!,@,#,$,%
KEYS3='['
KEYS3+='{"delay_ms": 2000, "keys": "", "label": "initial_render"}'
for i in "${!JUMP_KEYS[@]}"; do
    key="${JUMP_KEYS[$i]}"
    screen_name="${SCREEN_NAMES[$i]// /_}"
    KEYS3+=",{\"delay_ms\": 600, \"keys\": \"${key}\", \"label\": \"jump_to_${screen_name}\"}"
done
KEYS3+=",{\"delay_ms\": 300, \"keys\": \"q\", \"label\": \"quit\"}"
KEYS3+=']'

if ! run_timed_pty_interaction "jump" "${TRANSCRIPT3}" "${TIMING3}" "${KEYS3}" \
    env \
    DATABASE_URL="sqlite:////${DB3}" \
    STORAGE_ROOT="${STORAGE3}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT3}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORT3}"; then
    e2e_fatal "direct jump PTY interaction failed"
fi

# Verify all screens appeared
for i in "${!SCREEN_NAMES[@]}"; do
    assert_transcript_contains_any \
        "jump: screen ${i}: ${SCREEN_NAMES[$i]}" \
        "${TRANSCRIPT3}" \
        "${SCREEN_NAMES[$i]}" \
        "${SCREEN_SHORT_LABELS[$i]}"
done

if [ -f "${TIMING3}" ] && python3 -c "import json; d=json.load(open('${TIMING3}')); assert len(d['timings']) > 0" 2>/dev/null; then
    e2e_pass "jump: timing artifact valid"
else
    e2e_fail "jump: timing artifact missing or invalid"
fi


# ────────────────────────────────────────────────────────────────────
# Case 4: Aggregate perf report
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "aggregate_perf_report"

# Build the aggregate traversal_results.json from all three timing files
RESULTS_FILE="${E2E_ARTIFACT_DIR}/traversal_results.json"

python3 - "${TIMING1}" "${TIMING2}" "${TIMING3}" "${RESULTS_FILE}" <<'PYEOF'
import sys, json, os

forward_file, backward_file, jump_file, output_file = sys.argv[1:5]

def load_timings(path):
    try:
        with open(path) as f:
            return json.load(f).get("timings", [])
    except (FileNotFoundError, json.JSONDecodeError):
        return []

forward = load_timings(forward_file)
backward = load_timings(backward_file)
jump = load_timings(jump_file)

def extract_screen_latencies(timings, prefix):
    """Extract per-screen activation latencies from timing data.

    Each entry now includes:
      first_byte_ms - time from keystroke to first PTY output byte
      render_ms     - time from first byte to output quiescence (actual render)
      quiesce_ms    - time from keystroke to output quiescence (total activation)
      wall_ms       - total wall clock time for this step
      output_bytes_delta - bytes produced during this step
    """
    results = []
    for t in timings:
        label = t.get("step", "")
        if label.startswith(prefix):
            screen_name = label[len(prefix):].replace("_", " ")
            results.append({
                "screen": screen_name,
                "first_byte_ms": t.get("first_byte_ms"),
                "render_ms": t.get("render_ms", 0),
                "quiesce_ms": t.get("quiesce_ms", 0),
                "wall_ms": t.get("wall_ms", 0),
                "output_bytes_delta": t.get("output_bytes_delta", 0),
            })
    return results

forward_latencies = extract_screen_latencies(forward, "tab_to_")
backward_latencies = extract_screen_latencies(backward, "backtab_to_")
jump_latencies = extract_screen_latencies(jump, "jump_to_")

# Compute summary statistics over a named field
def stats(latencies, field="quiesce_ms"):
    if not latencies:
        return {"count": 0}
    ms_values = sorted(l.get(field, 0) or 0 for l in latencies)
    n = len(ms_values)
    return {
        "count": n,
        "min_ms": ms_values[0],
        "max_ms": ms_values[-1],
        "mean_ms": round(sum(ms_values) / n, 2),
        "median_ms": ms_values[n // 2],
        "p95_ms": ms_values[int(n * 0.95)] if n >= 2 else ms_values[-1],
        "p99_ms": ms_values[int(n * 0.99)] if n >= 2 else ms_values[-1],
    }

all_latencies = forward_latencies + backward_latencies + jump_latencies

report = {
    "harness": "tui_full_traversal",
    "bead": "br-legjy.1.1",
    "fixture_profile": os.environ.get("E2E_FIXTURE_PROFILE", "medium"),
    "screen_count": 15,
    "forward": {
        "latencies": forward_latencies,
        "summary_quiesce": stats(forward_latencies, "quiesce_ms"),
        "summary_first_byte": stats(forward_latencies, "first_byte_ms"),
        "summary_render": stats(forward_latencies, "render_ms"),
    },
    "backward": {
        "latencies": backward_latencies,
        "summary_quiesce": stats(backward_latencies, "quiesce_ms"),
        "summary_first_byte": stats(backward_latencies, "first_byte_ms"),
        "summary_render": stats(backward_latencies, "render_ms"),
    },
    "jump": {
        "latencies": jump_latencies,
        "summary_quiesce": stats(jump_latencies, "quiesce_ms"),
        "summary_first_byte": stats(jump_latencies, "first_byte_ms"),
        "summary_render": stats(jump_latencies, "render_ms"),
    },
    "overall": {
        "quiesce": stats(all_latencies, "quiesce_ms"),
        "first_byte": stats(all_latencies, "first_byte_ms"),
        "render": stats(all_latencies, "render_ms"),
    },
}

with open(output_file, "w") as f:
    json.dump(report, f, indent=2)

# Print summary to stdout
print(f"\n=== Traversal Perf Summary (quiesce_ms = activation latency) ===")
for mode in ["forward", "backward", "jump"]:
    s = report[mode]["summary_quiesce"]
    fb = report[mode]["summary_first_byte"]
    if s["count"] > 0:
        print(f"  {mode:10s}: {s['count']:2d} screens | "
              f"first_byte={fb['mean_ms']:6.1f}ms | "
              f"quiesce mean={s['mean_ms']:6.1f}ms p95={s['p95_ms']:6.1f}ms p99={s['p99_ms']:6.1f}ms max={s['max_ms']:6.1f}ms")
o = report["overall"]["quiesce"]
fb = report["overall"]["first_byte"]
if o["count"] > 0:
    print(f"  {'overall':10s}: {o['count']:2d} screens | "
          f"first_byte={fb['mean_ms']:6.1f}ms | "
          f"quiesce mean={o['mean_ms']:6.1f}ms p95={o['p95_ms']:6.1f}ms p99={o['p99_ms']:6.1f}ms max={o['max_ms']:6.1f}ms")
PYEOF

if [ -f "${RESULTS_FILE}" ]; then
    e2e_pass "aggregate: traversal_results.json written"
    e2e_save_artifact "traversal_results_copy.json" "$(cat "${RESULTS_FILE}")"
else
    e2e_fail "aggregate: traversal_results.json not created"
fi

# Validate the report has all expected fields
if python3 -c "
import json
r = json.load(open('${RESULTS_FILE}'))
assert r['screen_count'] == 15
assert r['forward']['summary_quiesce']['count'] >= 14
assert r['backward']['summary_quiesce']['count'] >= 14
assert r['jump']['summary_quiesce']['count'] >= 14
" 2>/dev/null; then
    e2e_pass "aggregate: report has all 3 traversal modes with >= 14 screens each"
else
    e2e_fail "aggregate: report missing data"
fi

# ────────────────────────────────────────────────────────────────────
# Case 4b: Traversal latency budget gate (F1)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "traversal_latency_budget_gate"

GATE_FILE="${E2E_ARTIFACT_DIR}/traversal_gate_verdict.json"
if python3 - "${RESULTS_FILE}" "${GATE_FILE}" "${TRAVERSAL_BUDGET_QUIESCE_P95_MS}" "${TRAVERSAL_BUDGET_QUIESCE_P99_MS}" <<'PYEOF'
import datetime
import json
import sys

results_path, gate_path, p95_budget_s, p99_budget_s = sys.argv[1:5]
p95_budget = float(p95_budget_s)
p99_budget = float(p99_budget_s)

with open(results_path, "r", encoding="utf-8") as f:
    report = json.load(f)

samples = []
failures = []

def check(detail: str, summary: dict, required_min_count: int) -> None:
    count = int(summary.get("count", 0) or 0)
    p50 = float(summary.get("median_ms", 0) or 0)
    p95 = float(summary.get("p95_ms", 0) or 0)
    p99 = float(summary.get("p99_ms", 0) or 0)
    max_ms = float(summary.get("max_ms", 0) or 0)

    reasons = []
    if count < required_min_count:
        reasons.append(f"insufficient_samples:{count}<{required_min_count}")
    if count > 0 and not (p50 <= p95 <= p99 <= max_ms):
        reasons.append("percentile_order_violation")

    within_p95 = count > 0 and p95 <= p95_budget
    within_p99 = count > 0 and p99 <= p99_budget
    within_budget = within_p95 and within_p99 and not reasons

    sample = {
        "surface": "screen_activation_quiesce",
        "detail": detail,
        "count": count,
        "p50_ms": round(p50, 2),
        "p95_ms": round(p95, 2),
        "p99_ms": round(p99, 2),
        "max_ms": round(max_ms, 2),
        "budget_p95_ms": p95_budget,
        "budget_p99_ms": p99_budget,
        "within_p95_budget": within_p95,
        "within_p99_budget": within_p99,
        "within_budget": within_budget,
        "reasons": reasons,
    }
    samples.append(sample)
    if not within_budget:
        failures.append(sample)

check("forward", report.get("forward", {}).get("summary_quiesce", {}), 14)
check("backward", report.get("backward", {}).get("summary_quiesce", {}), 14)
check("jump", report.get("jump", {}).get("summary_quiesce", {}), 14)
check("overall", report.get("overall", {}).get("quiesce", {}), 42)

gate = {
    "schema_version": "tui_traversal_gate.v1",
    "harness": "tui_full_traversal",
    "bead": "br-legjy.6.1",
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "budgets_ms": {
        "quiesce_p95_max": p95_budget,
        "quiesce_p99_max": p99_budget,
    },
    "samples": samples,
    "all_within_budget": len(failures) == 0,
    "failing_samples": [
        {
            "detail": sample["detail"],
            "p95_ms": sample["p95_ms"],
            "p99_ms": sample["p99_ms"],
            "reasons": sample["reasons"],
        }
        for sample in failures
    ],
    "repro": {
        "command": "bash tests/e2e/test_tui_full_traversal.sh",
        "env": {
            "TRAVERSAL_BUDGET_QUIESCE_P95_MS": p95_budget,
            "TRAVERSAL_BUDGET_QUIESCE_P99_MS": p99_budget,
        },
    },
}

with open(gate_path, "w", encoding="utf-8") as f:
    json.dump(gate, f, indent=2)
    f.write("\n")

print("=== Traversal Budget Gate ===")
for sample in samples:
    verdict = "PASS" if sample["within_budget"] else "FAIL"
    print(
        f"  {sample['detail']:8s} {verdict} "
        f"(p95={sample['p95_ms']:.2f}ms<= {p95_budget:.2f}ms, "
        f"p99={sample['p99_ms']:.2f}ms<= {p99_budget:.2f}ms)"
    )
if failures:
    print(f"  failing_modes={','.join(s['detail'] for s in failures)}")

sys.exit(0 if len(failures) == 0 else 1)
PYEOF
then
    e2e_pass "traversal gate: all modes within quiesce p95/p99 budgets"
else
    e2e_fail "traversal gate: budgets exceeded; see ${GATE_FILE} and rerun 'am e2e run --project . tui_full_traversal'"
fi

if [ -f "${GATE_FILE}" ]; then
    e2e_save_artifact "traversal_gate_verdict_copy.json" "$(cat "${GATE_FILE}")"
else
    e2e_fail "traversal gate: verdict artifact missing"
fi

if python3 - "${GATE_FILE}" <<'PYEOF'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    gate = json.load(f)

assert gate["schema_version"] == "tui_traversal_gate.v1"
assert gate["bead"] == "br-legjy.6.1"
assert isinstance(gate["all_within_budget"], bool)
assert len(gate["samples"]) == 4
for sample in gate["samples"]:
    for key in (
        "surface",
        "detail",
        "count",
        "p50_ms",
        "p95_ms",
        "p99_ms",
        "max_ms",
        "budget_p95_ms",
        "budget_p99_ms",
        "within_budget",
    ):
        assert key in sample
PYEOF
then
    e2e_pass "traversal gate: verdict artifact schema complete"
else
    e2e_fail "traversal gate: verdict artifact schema incomplete"
fi


# ────────────────────────────────────────────────────────────────────
# Case 4c: Event-pressure + resize-storm PTY regressions (C4)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "event_pressure_resize_regressions"

# Pressure scenario: rapid mixed navigation input cadence.
read -r WORKP DBP STORAGEP <<< "$(setup_server_env "pressure")"
PORTP="$(pick_port)"

DATABASE_URL="sqlite:////${DBP}" \
STORAGE_ROOT="${STORAGEP}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PORTP}" \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
TUI_ENABLED=0 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORTP}" --no-tui &
HEADLESS_PIDP=$!

if wait_for_server "${PORTP}" 15; then
    seed_realistic_data "${PORTP}" "${FIXTURE_PROFILE}"
else
    e2e_fatal "Headless server failed to start for pressure scenario seeding"
fi

kill "${HEADLESS_PIDP}" 2>/dev/null || true
wait "${HEADLESS_PIDP}" 2>/dev/null || true
sleep 1

TRANSCRIPTP="${E2E_ARTIFACT_DIR}/pressure_transcript.txt"
TIMINGP="${E2E_ARTIFACT_DIR}/pressure_timing.json"

KEYSP='['
KEYSP+='{"delay_ms": 1800, "keys": "", "label": "initial_render"}'
for i in $(seq 1 60); do
    if [ $((i % 10)) -eq 0 ]; then
        KEYS_LABEL="pressure_jump_to_Search_${i}"
        KEYS_VALUE="5"
    elif [ $((i % 4)) -eq 0 ]; then
        KEYS_LABEL="pressure_backtab_${i}"
        KEYS_VALUE="${BACKTAB_KEY_JSON}"
    else
        KEYS_LABEL="pressure_tab_${i}"
        KEYS_VALUE="\\t"
    fi
    KEYS_DELAY=140
    KEYSP+=",{\"delay_ms\": ${KEYS_DELAY}, \"keys\": \"${KEYS_VALUE}\", \"label\": \"${KEYS_LABEL}\"}"
done
KEYSP+=",{\"delay_ms\": 300, \"keys\": \"q\", \"label\": \"quit\"}"
KEYSP+=']'

if ! run_timed_pty_interaction "pressure" "${TRANSCRIPTP}" "${TIMINGP}" "${KEYSP}" \
    env \
    DATABASE_URL="sqlite:////${DBP}" \
    STORAGE_ROOT="${STORAGEP}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORTP}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORTP}"; then
    e2e_fatal "pressure traversal PTY interaction failed"
fi

if python3 - "${TIMINGP}" <<'PYEOF'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as f:
    doc = json.load(f)
pressure_steps = [t for t in doc.get("timings", []) if str(t.get("step", "")).startswith("pressure_")]
assert len(pressure_steps) >= 50
PYEOF
then
    e2e_pass "pressure: timing artifact valid with >=50 pressure steps"
else
    e2e_fail "pressure: timing artifact missing expected pressure steps"
fi

# Resize-storm scenario: deterministic alternating resize + navigation cadence.
read -r WORKR DBR STORAGER <<< "$(setup_server_env "resize_storm")"
PORTR="$(pick_port)"

DATABASE_URL="sqlite:////${DBR}" \
STORAGE_ROOT="${STORAGER}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PORTR}" \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
TUI_ENABLED=0 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORTR}" --no-tui &
HEADLESS_PIDR=$!

if wait_for_server "${PORTR}" 15; then
    seed_realistic_data "${PORTR}" "${FIXTURE_PROFILE}"
else
    e2e_fatal "Headless server failed to start for resize-storm scenario seeding"
fi

kill "${HEADLESS_PIDR}" 2>/dev/null || true
wait "${HEADLESS_PIDR}" 2>/dev/null || true
sleep 1

TRANSCRIPTR="${E2E_ARTIFACT_DIR}/resize_storm_transcript.txt"
TIMINGR="${E2E_ARTIFACT_DIR}/resize_storm_timing.json"

KEYSR='['
KEYSR+='{"delay_ms": 1800, "keys": "", "label": "initial_render"}'
for i in $(seq 1 24); do
    rows=$((18 + (i % 6) * 5))
    cols=$((70 + (i % 7) * 12))
    if [ $((i % 3)) -eq 0 ]; then
        key_value="${BACKTAB_KEY_JSON}"
        key_label="backtab"
    else
        key_value="\\t"
        key_label="tab"
    fi
    KEYSR+=",{\"delay_ms\": 170, \"keys\": \"${key_value}\", \"label\": \"resize_${i}_${rows}x${cols}_${key_label}\", \"resize\": {\"rows\": ${rows}, \"cols\": ${cols}}}"
done
KEYSR+=",{\"delay_ms\": 300, \"keys\": \"q\", \"label\": \"quit\"}"
KEYSR+=']'

if ! run_timed_pty_interaction "resize_storm" "${TRANSCRIPTR}" "${TIMINGR}" "${KEYSR}" \
    env \
    DATABASE_URL="sqlite:////${DBR}" \
    STORAGE_ROOT="${STORAGER}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORTR}" \
    HTTP_RBAC_ENABLED=0 \
    HTTP_RATE_LIMIT_ENABLED=0 \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORTR}"; then
    e2e_fatal "resize-storm PTY interaction failed"
fi

if python3 - "${TIMINGR}" <<'PYEOF'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as f:
    doc = json.load(f)
steps = [t for t in doc.get("timings", []) if str(t.get("step", "")).startswith("resize_")]
resize_steps = [t for t in steps if isinstance(t.get("resize"), dict)]
assert len(steps) >= 20
assert len(resize_steps) >= 20
PYEOF
then
    e2e_pass "resize-storm: timing artifact valid with >=20 resize steps"
else
    e2e_fail "resize-storm: timing artifact missing expected resize steps"
fi

PRESSURE_RESIZE_REPORT="${E2E_ARTIFACT_DIR}/pressure_resize_regression_report.json"
if python3 - "${TIMINGP}" "${TIMINGR}" "${PRESSURE_RESIZE_REPORT}" \
    "${PRESSURE_BUDGET_FIRST_BYTE_P95_MS}" \
    "${PRESSURE_BUDGET_QUIESCE_P95_MS}" \
    "${PRESSURE_BUDGET_QUIESCE_P99_MS}" \
    "${RESIZE_BUDGET_FIRST_BYTE_P95_MS}" \
    "${RESIZE_BUDGET_QUIESCE_P95_MS}" \
    "${RESIZE_BUDGET_REPAINT_BURST_MAX}" <<'PYEOF'
import datetime
import json
import statistics
import sys

(
    pressure_path,
    resize_path,
    report_path,
    pressure_fb_budget_s,
    pressure_q95_budget_s,
    pressure_q99_budget_s,
    resize_fb_budget_s,
    resize_q95_budget_s,
    resize_burst_budget_s,
) = sys.argv[1:10]

pressure_fb_budget = float(pressure_fb_budget_s)
pressure_q95_budget = float(pressure_q95_budget_s)
pressure_q99_budget = float(pressure_q99_budget_s)
resize_fb_budget = float(resize_fb_budget_s)
resize_q95_budget = float(resize_q95_budget_s)
resize_burst_budget = float(resize_burst_budget_s)

with open(pressure_path, "r", encoding="utf-8") as f:
    pressure_doc = json.load(f)
with open(resize_path, "r", encoding="utf-8") as f:
    resize_doc = json.load(f)

def percentile(values, p):
    if not values:
        return 0.0
    vals = sorted(values)
    idx = min(len(vals) - 1, max(0, int((len(vals) - 1) * p)))
    return float(vals[idx])

def summarize_steps(steps):
    first = [float(s["first_byte_ms"]) for s in steps if s.get("first_byte_ms") is not None]
    quiesce = [float(s.get("quiesce_ms", 0) or 0) for s in steps]
    delta = [float(s.get("output_bytes_delta", 0) or 0) for s in steps]
    median_delta = statistics.median(delta) if delta else 0.0
    burst_factor = (max(delta) / median_delta) if delta and median_delta > 0 else (max(delta) if delta else 0.0)
    return {
        "count": len(steps),
        "first_byte_p95_ms": round(percentile(first, 0.95), 2),
        "quiesce_p95_ms": round(percentile(quiesce, 0.95), 2),
        "quiesce_p99_ms": round(percentile(quiesce, 0.99), 2),
        "output_bytes_p95": round(percentile(delta, 0.95), 2),
        "output_bytes_median": round(median_delta, 2),
        "output_bytes_max": round(max(delta), 2) if delta else 0.0,
        "repaint_burst_factor": round(float(burst_factor), 3),
    }

pressure_steps = [s for s in pressure_doc.get("timings", []) if str(s.get("step", "")).startswith("pressure_")]
resize_steps = [s for s in resize_doc.get("timings", []) if str(s.get("step", "")).startswith("resize_")]
resize_event_count = sum(1 for s in resize_steps if isinstance(s.get("resize"), dict))

pressure_summary = summarize_steps(pressure_steps)
resize_summary = summarize_steps(resize_steps)

pressure_reasons = []
if pressure_summary["count"] < 50:
    pressure_reasons.append("insufficient_pressure_samples")
if pressure_summary["first_byte_p95_ms"] > pressure_fb_budget:
    pressure_reasons.append("pressure_first_byte_p95_exceeded")
if pressure_summary["quiesce_p95_ms"] > pressure_q95_budget:
    pressure_reasons.append("pressure_quiesce_p95_exceeded")
if pressure_summary["quiesce_p99_ms"] > pressure_q99_budget:
    pressure_reasons.append("pressure_quiesce_p99_exceeded")

resize_reasons = []
if resize_summary["count"] < 20:
    resize_reasons.append("insufficient_resize_samples")
if resize_event_count < 20:
    resize_reasons.append("insufficient_resize_events")
if resize_summary["first_byte_p95_ms"] > resize_fb_budget:
    resize_reasons.append("resize_first_byte_p95_exceeded")
if resize_summary["quiesce_p95_ms"] > resize_q95_budget:
    resize_reasons.append("resize_quiesce_p95_exceeded")
if resize_summary["repaint_burst_factor"] > resize_burst_budget:
    resize_reasons.append("resize_repaint_burst_exceeded")

samples = [
    {
        "scenario": "event_pressure",
        "summary": pressure_summary,
        "within_budget": len(pressure_reasons) == 0,
        "reasons": pressure_reasons,
        "budgets": {
            "first_byte_p95_ms_max": pressure_fb_budget,
            "quiesce_p95_ms_max": pressure_q95_budget,
            "quiesce_p99_ms_max": pressure_q99_budget,
        },
        "ansi_metrics": pressure_doc.get("ansi_metrics", {}),
    },
    {
        "scenario": "resize_storm",
        "summary": {
            **resize_summary,
            "resize_event_count": resize_event_count,
        },
        "within_budget": len(resize_reasons) == 0,
        "reasons": resize_reasons,
        "budgets": {
            "first_byte_p95_ms_max": resize_fb_budget,
            "quiesce_p95_ms_max": resize_q95_budget,
            "repaint_burst_factor_max": resize_burst_budget,
        },
        "ansi_metrics": resize_doc.get("ansi_metrics", {}),
    },
]

failing = [s for s in samples if not s["within_budget"]]
report = {
    "schema_version": "tui_pressure_resize_gate.v1",
    "harness": "tui_full_traversal",
    "bead": "br-legjy.3.4",
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "samples": samples,
    "all_within_budget": len(failing) == 0,
    "failing_samples": [
        {"scenario": s["scenario"], "reasons": s["reasons"]} for s in failing
    ],
    "repro": {
        "command": "bash tests/e2e/test_tui_full_traversal.sh",
        "env": {
            "PRESSURE_BUDGET_FIRST_BYTE_P95_MS": pressure_fb_budget,
            "PRESSURE_BUDGET_QUIESCE_P95_MS": pressure_q95_budget,
            "PRESSURE_BUDGET_QUIESCE_P99_MS": pressure_q99_budget,
            "RESIZE_BUDGET_FIRST_BYTE_P95_MS": resize_fb_budget,
            "RESIZE_BUDGET_QUIESCE_P95_MS": resize_q95_budget,
            "RESIZE_BUDGET_REPAINT_BURST_MAX": resize_burst_budget,
        },
    },
}

with open(report_path, "w", encoding="utf-8") as f:
    json.dump(report, f, indent=2)
    f.write("\n")

print("=== Event Pressure + Resize Storm Regression Gate ===")
for sample in samples:
    verdict = "PASS" if sample["within_budget"] else "FAIL"
    summary = sample["summary"]
    print(
        f"  {sample['scenario']:14s} {verdict} "
        f"(count={summary.get('count', 0)}, first_p95={summary.get('first_byte_p95_ms', 0):.2f}ms, "
        f"quiesce_p95={summary.get('quiesce_p95_ms', 0):.2f}ms, "
        f"quiesce_p99={summary.get('quiesce_p99_ms', 0):.2f}ms)"
    )
    if sample["scenario"] == "resize_storm":
        print(
            f"    resize_events={summary.get('resize_event_count', 0)} "
            f"repaint_burst={summary.get('repaint_burst_factor', 0):.3f}x"
        )
if failing:
    print("  failing=" + ",".join(s["scenario"] for s in failing))

sys.exit(0 if len(failing) == 0 else 1)
PYEOF
then
    e2e_pass "pressure/resize regression gate: all scenarios within budgets"
else
    e2e_fail "pressure/resize regression gate: budgets exceeded; see ${PRESSURE_RESIZE_REPORT} and rerun 'am e2e run --project . tui_full_traversal'"
fi

if [ -f "${PRESSURE_RESIZE_REPORT}" ]; then
    e2e_save_artifact "pressure_resize_regression_report_copy.json" "$(cat "${PRESSURE_RESIZE_REPORT}")"
else
    e2e_fail "pressure/resize regression gate: report artifact missing"
fi

if python3 - "${PRESSURE_RESIZE_REPORT}" <<'PYEOF'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    report = json.load(f)

assert report["schema_version"] == "tui_pressure_resize_gate.v1"
assert report["bead"] == "br-legjy.3.4"
assert isinstance(report["all_within_budget"], bool)
assert len(report["samples"]) == 2
for sample in report["samples"]:
    for key in ("scenario", "summary", "within_budget", "reasons", "budgets", "ansi_metrics"):
        assert key in sample
PYEOF
then
    e2e_pass "pressure/resize regression gate: report schema complete"
else
    e2e_fail "pressure/resize regression gate: report schema incomplete"
fi


# ────────────────────────────────────────────────────────────────────
# Case 4d: Deterministic flash-detection gate (F2)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "flash_detection_gate"

FLASH_REPORT="${E2E_ARTIFACT_DIR}/flash_detection_report.json"
if python3 - "${TIMINGP}" "${TIMINGR}" "${FLASH_REPORT}" \
    "${FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX}" \
    "${FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX}" \
    "${FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX}" <<'PYEOF'
import datetime
import json
import sys

timing_pressure, timing_resize, report_path, empty_budget_s, bounce_budget_s, repaint_budget_s = sys.argv[1:7]
empty_budget = float(empty_budget_s)
bounce_budget = float(bounce_budget_s)
repaint_budget = float(repaint_budget_s)

with open(timing_pressure, "r", encoding="utf-8") as f:
    pressure_doc = json.load(f)
with open(timing_resize, "r", encoding="utf-8") as f:
    resize_doc = json.load(f)

def analyze(doc, prefix):
    steps = [s for s in doc.get("timings", []) if str(s.get("step", "")).startswith(prefix)]
    n = len(steps)
    hashes = [str(s.get("frame_hash", "empty") or "empty") for s in steps]
    frame_chars = [int(s.get("frame_chars", 0) or 0) for s in steps]
    visible_cells = [int(s.get("terminal_nonblank_cells", 0) or 0) for s in steps]
    visible_lines = [int(s.get("terminal_nonblank_lines", 0) or 0) for s in steps]
    cursor_home = [int(s.get("frame_cursor_home_ops", 0) or 0) for s in steps]
    clear_ops = [int(s.get("frame_clear_ops", 0) or 0) for s in steps]
    erase_ops = [int(s.get("frame_erase_line_ops", 0) or 0) for s in steps]
    bytes_out = [float(s.get("output_bytes_delta", 0) or 0) for s in steps]

    transitions = sum(1 for i in range(1, n) if hashes[i] != hashes[i - 1])
    # A-B-A pattern can indicate unstable redraw oscillation.
    bounce = sum(1 for i in range(2, n) if hashes[i] == hashes[i - 2] and hashes[i] != hashes[i - 1])
    # Evaluate the emulated terminal state, not merely bytes emitted during
    # this step. A healthy incremental frame can emit zero bytes while the
    # screen remains populated; conversely a repaint full of spaces emits many
    # bytes while leaving the operator with a blank display.
    empty_frames = sum(1 for cells, lines in zip(visible_cells, visible_lines) if cells < 5 or lines < 2)

    repaint_ops_total = sum(cursor_home) + sum(clear_ops) + sum(erase_ops)
    bytes_total = sum(bytes_out)
    repaint_ops_per_kb = repaint_ops_total / max(bytes_total / 1024.0, 1.0)

    metrics = {
        "step_count": n,
        "frame_transitions": transitions,
        "frame_bounce_events": bounce,
        "frame_bounce_ratio": round(bounce / max(n - 2, 1), 4),
        "empty_frame_count": empty_frames,
        "empty_frame_ratio": round(empty_frames / max(n, 1), 4),
        "terminal_nonblank_cells_min": min(visible_cells, default=0),
        "terminal_nonblank_lines_min": min(visible_lines, default=0),
        "repaint_ops_total": repaint_ops_total,
        "repaint_ops_per_kb": round(repaint_ops_per_kb, 4),
        "bytes_total": round(bytes_total, 2),
    }

    reasons = []
    if n < 20:
        reasons.append("insufficient_step_count")
    if metrics["empty_frame_ratio"] > empty_budget:
        reasons.append("empty_frame_ratio_exceeded")
    if metrics["frame_bounce_ratio"] > bounce_budget:
        reasons.append("frame_bounce_ratio_exceeded")
    if metrics["repaint_ops_per_kb"] > repaint_budget:
        reasons.append("repaint_ops_per_kb_exceeded")

    return {
        "scenario": prefix.rstrip("_"),
        "metrics": metrics,
        "within_budget": len(reasons) == 0,
        "reasons": reasons,
        "budgets": {
            "empty_frame_ratio_max": empty_budget,
            "frame_bounce_ratio_max": bounce_budget,
            "repaint_ops_per_kb_max": repaint_budget,
        },
        "ansi_metrics": doc.get("ansi_metrics", {}),
    }

samples = [
    analyze(pressure_doc, "pressure_"),
    analyze(resize_doc, "resize_"),
]
failing = [s for s in samples if not s["within_budget"]]

report = {
    "schema_version": "tui_flash_detection_gate.v1",
    "harness": "tui_full_traversal",
    "bead": "br-legjy.6.2",
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "samples": samples,
    "all_within_budget": len(failing) == 0,
    "failing_samples": [
        {"scenario": s["scenario"], "reasons": s["reasons"]} for s in failing
    ],
    "repro": {
        "command": "bash tests/e2e/test_tui_full_traversal.sh",
        "env": {
            "FLASH_BUDGET_EMPTY_FRAME_RATIO_MAX": empty_budget,
            "FLASH_BUDGET_FRAME_BOUNCE_RATIO_MAX": bounce_budget,
            "FLASH_BUDGET_REPAINT_OPS_PER_KB_MAX": repaint_budget,
        },
    },
}

with open(report_path, "w", encoding="utf-8") as f:
    json.dump(report, f, indent=2)
    f.write("\n")

print("=== Flash Detection Gate ===")
for sample in samples:
    m = sample["metrics"]
    verdict = "PASS" if sample["within_budget"] else "FAIL"
    print(
        f"  {sample['scenario']:8s} {verdict} "
        f"(steps={m['step_count']}, empty_ratio={m['empty_frame_ratio']:.4f}, "
        f"bounce_ratio={m['frame_bounce_ratio']:.4f}, repaint_ops_per_kb={m['repaint_ops_per_kb']:.4f})"
    )
if failing:
    print("  failing=" + ",".join(s["scenario"] for s in failing))

sys.exit(0 if len(failing) == 0 else 1)
PYEOF
then
    e2e_pass "flash detection gate: all scenarios within budgets"
else
    e2e_fail "flash detection gate: budgets exceeded; see ${FLASH_REPORT} and rerun 'am e2e run --project . tui_full_traversal'"
fi

if [ -f "${FLASH_REPORT}" ]; then
    e2e_save_artifact "flash_detection_report_copy.json" "$(cat "${FLASH_REPORT}")"
else
    e2e_fail "flash detection gate: report artifact missing"
fi

if python3 - "${FLASH_REPORT}" <<'PYEOF'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    report = json.load(f)

assert report["schema_version"] == "tui_flash_detection_gate.v1"
assert report["bead"] == "br-legjy.6.2"
assert isinstance(report["all_within_budget"], bool)
assert len(report["samples"]) == 2
for sample in report["samples"]:
    for key in ("scenario", "metrics", "within_budget", "reasons", "budgets", "ansi_metrics"):
        assert key in sample
PYEOF
then
    e2e_pass "flash detection gate: report schema complete"
else
    e2e_fail "flash detection gate: report schema incomplete"
fi


# ────────────────────────────────────────────────────────────────────
# Case 4e: Multi-minute soak gate with drift telemetry (F3)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "multi_minute_soak_gate"

read -r WORKS DBS STORAGES <<< "$(setup_server_env "soak")"
PORTS="$(pick_port)"

DATABASE_URL="sqlite:////${DBS}" \
STORAGE_ROOT="${STORAGES}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PORTS}" \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
TUI_ENABLED=0 \
    "${BIN}" serve --host 127.0.0.1 --port "${PORTS}" --no-tui &
HEADLESS_PIDS=$!

if wait_for_server "${PORTS}" 15; then
    seed_realistic_data "${PORTS}" "${FIXTURE_PROFILE}"
else
    e2e_fatal "Headless server failed to start for soak scenario seeding"
fi

kill "${HEADLESS_PIDS}" 2>/dev/null || true
wait "${HEADLESS_PIDS}" 2>/dev/null || true
sleep 1

TRANSCRIPTS="${E2E_ARTIFACT_DIR}/soak_transcript.txt"
TIMINGS="${E2E_ARTIFACT_DIR}/soak_timing.json"
SOAK_REPORT="${E2E_ARTIFACT_DIR}/soak_regression_report.json"
SOAK_PROFILE_DIR="${E2E_ARTIFACT_DIR}/soak_profile"
mkdir -p "${SOAK_PROFILE_DIR}"

SOAK_TOTAL_STEPS=$(( (SOAK_DURATION_SECONDS * 1000 + SOAK_STEP_DELAY_MS - 1) / SOAK_STEP_DELAY_MS ))
SOAK_MIN_STEPS=$(( SOAK_TOTAL_STEPS * 9 / 10 ))
if [ "${SOAK_MIN_STEPS}" -lt 60 ]; then
    SOAK_MIN_STEPS=60
fi
e2e_log "Soak scenario: ${SOAK_DURATION_SECONDS}s target, ${SOAK_TOTAL_STEPS} scheduled steps, minimum accepted steps=${SOAK_MIN_STEPS}"

KEYSS='['
KEYSS+='{"delay_ms": 2200, "keys": "", "label": "initial_render"}'
for i in $(seq 1 "${SOAK_TOTAL_STEPS}"); do
    if [ $((i % 20)) -eq 0 ]; then
        key_value="5"
        key_label="jump_search"
    elif [ $((i % 11)) -eq 0 ]; then
        key_value="${BACKTAB_KEY_JSON}"
        key_label="backtab"
    elif [ $((i % 7)) -eq 0 ]; then
        key_value="0"
        key_label="jump_projects"
    elif [ $((i % 5)) -eq 0 ]; then
        key_value=""
        key_label="idle"
    else
        key_value="\\t"
        key_label="tab"
    fi

    if [ $((i % 37)) -eq 0 ]; then
        rows=$((20 + (i % 8) * 3))
        cols=$((88 + (i % 9) * 6))
        KEYSS+=",{\"delay_ms\": ${SOAK_STEP_DELAY_MS}, \"min_capture_ms\": ${SOAK_STEP_DELAY_MS}, \"keys\": \"${key_value}\", \"label\": \"soak_${key_label}_${i}\", \"resize\": {\"rows\": ${rows}, \"cols\": ${cols}}}"
    else
        KEYSS+=",{\"delay_ms\": ${SOAK_STEP_DELAY_MS}, \"min_capture_ms\": ${SOAK_STEP_DELAY_MS}, \"keys\": \"${key_value}\", \"label\": \"soak_${key_label}_${i}\"}"
    fi
done
KEYSS+=",{\"delay_ms\": 300, \"keys\": \"q\", \"label\": \"quit\"}"
KEYSS+=']'

if ! E2E_PROFILE_CAPTURE=1 \
    E2E_PROFILE_DIR="${SOAK_PROFILE_DIR}" \
    E2E_PROFILE_LABEL="soak" \
    run_timed_pty_interaction "soak" "${TRANSCRIPTS}" "${TIMINGS}" "${KEYSS}" \
        env \
        DATABASE_URL="sqlite:////${DBS}" \
        STORAGE_ROOT="${STORAGES}" \
        HTTP_HOST="127.0.0.1" \
        HTTP_PORT="${PORTS}" \
        HTTP_RBAC_ENABLED=0 \
        HTTP_RATE_LIMIT_ENABLED=0 \
        HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
        "${BIN}" serve --host 127.0.0.1 --port "${PORTS}"; then
    e2e_fatal "soak PTY interaction failed"
fi

if python3 - "${TIMINGS}" "${SOAK_MIN_STEPS}" <<'PYEOF'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as f:
    doc = json.load(f)
steps = [t for t in doc.get("timings", []) if str(t.get("step", "")).startswith("soak_")]
min_steps = int(sys.argv[2])
assert len(steps) >= min_steps
PYEOF
then
    e2e_pass "soak: timing artifact contains required minimum number of soak steps"
else
    e2e_fail "soak: timing artifact does not contain enough soak steps"
fi

if python3 - "${TIMINGS}" "${SOAK_REPORT}" "${SOAK_MIN_STEPS}" "${SOAK_DURATION_SECONDS}" \
    "${SOAK_BUDGET_FIRST_BYTE_P95_MS}" "${SOAK_BUDGET_QUIESCE_P95_MS}" "${SOAK_BUDGET_QUIESCE_P99_MS}" \
    "${SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX}" "${SOAK_BUDGET_CPU_DRIFT_PCT_MAX}" "${SOAK_BUDGET_WAKE_DRIFT_PCT_MAX}" \
    "${SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX}" "${SOAK_BUDGET_LOW_VISIBILITY_FRACTION}" \
    "${SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX}" "${SOAK_PROFILE_DIR}" <<'PYEOF'
import datetime
import json
import re
import sys
from pathlib import Path

(
    timing_path,
    report_path,
    min_steps_s,
    duration_s,
    first_p95_budget_s,
    quiesce_p95_budget_s,
    quiesce_p99_budget_s,
    latency_drift_budget_s,
    cpu_drift_budget_s,
    wake_drift_budget_s,
    repaint_budget_s,
    low_visibility_fraction_s,
    low_visibility_ratio_budget_s,
    profile_dir_s,
) = sys.argv[1:15]

min_steps = int(min_steps_s)
duration_seconds = int(duration_s)
first_p95_budget = float(first_p95_budget_s)
quiesce_p95_budget = float(quiesce_p95_budget_s)
quiesce_p99_budget = float(quiesce_p99_budget_s)
latency_drift_budget = float(latency_drift_budget_s)
cpu_drift_budget = float(cpu_drift_budget_s)
wake_drift_budget = float(wake_drift_budget_s)
repaint_budget = float(repaint_budget_s)
low_visibility_fraction = float(low_visibility_fraction_s)
low_visibility_ratio_budget = float(low_visibility_ratio_budget_s)
profile_dir = Path(profile_dir_s)

with open(timing_path, "r", encoding="utf-8") as f:
    timing_doc = json.load(f)

steps = [s for s in timing_doc.get("timings", []) if str(s.get("step", "")).startswith("soak_")]
total_entry = next((s for s in timing_doc.get("timings", []) if s.get("step") == "total"), {})

def percentile(values, p):
    if not values:
        return 0.0
    vals = sorted(values)
    idx = min(len(vals) - 1, max(0, int((len(vals) - 1) * p)))
    return float(vals[idx])

def mean(values):
    if not values:
        return 0.0
    return float(sum(values) / len(values))

def drift_pct(start, end):
    if start <= 0:
        return 0.0
    return ((end - start) / start) * 100.0

def parse_pidstat_process(path: Path):
    series = []
    if not path.exists():
        return series
    with path.open("r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not re.match(r"^\d{2}:\d{2}:\d{2}\s+(AM|PM)\s+", line):
                continue
            parts = line.split()
            if len(parts) < 9:
                continue
            try:
                series.append(float(parts[8]))
            except ValueError:
                continue
    return series

def parse_pidstat_wake(path: Path):
    series = []
    if not path.exists():
        return series
    with path.open("r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not re.match(r"^\d{2}:\d{2}:\d{2}\s+(AM|PM)\s+", line):
                continue
            parts = line.split()
            if len(parts) < 7:
                continue
            try:
                cs = float(parts[5])
                ncs = float(parts[6])
            except ValueError:
                continue
            series.append(cs + ncs)
    return series

first = [float(s["first_byte_ms"]) for s in steps if s.get("first_byte_ms") is not None]
quiesce_raw = [float(s.get("quiesce_ms", 0) or 0) for s in steps]
min_capture = [float(s.get("min_capture_ms", 0) or 0) for s in steps]
# For soak we intentionally enforce dwell windows via min_capture_ms.
# Normalize quiesce by removing that floor so budgets reflect real render quiescence.
quiesce = [max(0.0, q - m) for q, m in zip(quiesce_raw, min_capture)]
render = [float(s.get("render_ms", 0) or 0) for s in steps]
bytes_out = [float(s.get("output_bytes_delta", 0) or 0) for s in steps]
cursor_home = [int(s.get("frame_cursor_home_ops", 0) or 0) for s in steps]
clear_ops = [int(s.get("frame_clear_ops", 0) or 0) for s in steps]
erase_ops = [int(s.get("frame_erase_line_ops", 0) or 0) for s in steps]
visible_cells = [int(s.get("terminal_nonblank_cells", 0) or 0) for s in steps]
visible_lines = [int(s.get("terminal_nonblank_lines", 0) or 0) for s in steps]

count = len(steps)
window = max(1, count // 3)
start_q = mean(quiesce[:window])
end_q = mean(quiesce[-window:]) if quiesce else 0.0
latency_drift = drift_pct(start_q, end_q)
latency_growth = max(0.0, latency_drift)

cpu_series = parse_pidstat_process(profile_dir / "soak_pidstat_process.txt")
wake_series = parse_pidstat_wake(profile_dir / "soak_pidstat_wake.txt")

cpu_start = mean(cpu_series[: max(1, len(cpu_series) // 3)])
cpu_end = mean(cpu_series[-max(1, len(cpu_series) // 3):]) if cpu_series else 0.0
cpu_drift = drift_pct(cpu_start, cpu_end)
cpu_growth = max(0.0, cpu_drift)

wake_start = mean(wake_series[: max(1, len(wake_series) // 3)])
wake_end = mean(wake_series[-max(1, len(wake_series) // 3):]) if wake_series else 0.0
wake_drift = drift_pct(wake_start, wake_end)
wake_growth = max(0.0, wake_drift)

repaint_ops_total = sum(cursor_home) + sum(clear_ops) + sum(erase_ops)
bytes_total = sum(bytes_out)
repaint_ops_per_kb = repaint_ops_total / max(bytes_total / 1024.0, 1.0)
visible_cells_p90 = percentile(visible_cells, 0.90)
low_visibility_threshold = max(5.0, visible_cells_p90 * low_visibility_fraction)
low_visibility_count = sum(
    1
    for cells, lines in zip(visible_cells, visible_lines)
    if cells < low_visibility_threshold or lines < 2
)
low_visibility_ratio = low_visibility_count / max(len(visible_cells), 1)

duration_ms_actual = float(total_entry.get("wall_ms", 0) or 0)
duration_ms_expected = float(duration_seconds * 1000)

summary = {
    "step_count": count,
    "duration_ms_actual": round(duration_ms_actual, 2),
    "duration_ms_expected": round(duration_ms_expected, 2),
    "first_byte_p50_ms": round(percentile(first, 0.50), 2),
    "first_byte_p95_ms": round(percentile(first, 0.95), 2),
    "first_byte_p99_ms": round(percentile(first, 0.99), 2),
    "quiesce_p50_ms": round(percentile(quiesce, 0.50), 2),
    "quiesce_p95_ms": round(percentile(quiesce, 0.95), 2),
    "quiesce_p99_ms": round(percentile(quiesce, 0.99), 2),
    "quiesce_raw_p95_ms": round(percentile(quiesce_raw, 0.95), 2),
    "quiesce_raw_p99_ms": round(percentile(quiesce_raw, 0.99), 2),
    "render_p50_ms": round(percentile(render, 0.50), 2),
    "render_p95_ms": round(percentile(render, 0.95), 2),
    "latency_drift_pct": round(latency_drift, 2),
    "latency_growth_pct": round(latency_growth, 2),
    "cpu_samples": len(cpu_series),
    "cpu_drift_pct": round(cpu_drift, 2),
    "cpu_growth_pct": round(cpu_growth, 2),
    "cpu_p95_percent": round(percentile(cpu_series, 0.95), 2),
    "wake_samples": len(wake_series),
    "wake_drift_pct": round(wake_drift, 2),
    "wake_growth_pct": round(wake_growth, 2),
    "wake_p95_per_s": round(percentile(wake_series, 0.95), 2),
    "repaint_ops_total": repaint_ops_total,
    "repaint_ops_per_kb": round(repaint_ops_per_kb, 4),
    "bytes_total": round(bytes_total, 2),
    "terminal_nonblank_cells_p10": round(percentile(visible_cells, 0.10), 2),
    "terminal_nonblank_cells_p50": round(percentile(visible_cells, 0.50), 2),
    "terminal_nonblank_cells_p90": round(visible_cells_p90, 2),
    "terminal_nonblank_cells_min": min(visible_cells, default=0),
    "terminal_nonblank_lines_min": min(visible_lines, default=0),
    "low_visibility_threshold_cells": round(low_visibility_threshold, 2),
    "low_visibility_count": low_visibility_count,
    "low_visibility_ratio": round(low_visibility_ratio, 4),
}

reasons = []
if count < min_steps:
    reasons.append("insufficient_step_count")
if duration_ms_actual < duration_ms_expected * 0.90:
    reasons.append("insufficient_duration")
if summary["first_byte_p95_ms"] > first_p95_budget:
    reasons.append("first_byte_p95_exceeded")
if summary["quiesce_p95_ms"] > quiesce_p95_budget:
    reasons.append("quiesce_p95_exceeded")
if summary["quiesce_p99_ms"] > quiesce_p99_budget:
    reasons.append("quiesce_p99_exceeded")
if summary["latency_growth_pct"] > latency_drift_budget:
    reasons.append("latency_drift_exceeded")
if len(cpu_series) == 0:
    reasons.append("missing_cpu_telemetry")
elif summary["cpu_growth_pct"] > cpu_drift_budget:
    reasons.append("cpu_drift_exceeded")
if len(wake_series) == 0:
    reasons.append("missing_wake_telemetry")
elif summary["wake_growth_pct"] > wake_drift_budget:
    reasons.append("wake_drift_exceeded")
if summary["repaint_ops_per_kb"] > repaint_budget:
    reasons.append("repaint_churn_exceeded")
if summary["low_visibility_ratio"] > low_visibility_ratio_budget:
    reasons.append("low_terminal_visibility_exceeded")

sample = {
    "scenario": "multi_minute_soak",
    "summary": summary,
    "within_budget": len(reasons) == 0,
    "reasons": reasons,
    "budgets": {
        "min_step_count": min_steps,
        "duration_ms_min": round(duration_ms_expected * 0.90, 2),
        "first_byte_p95_ms_max": first_p95_budget,
        "quiesce_p95_ms_max": quiesce_p95_budget,
        "quiesce_p99_ms_max": quiesce_p99_budget,
        "latency_growth_pct_max": latency_drift_budget,
        "cpu_growth_pct_max": cpu_drift_budget,
        "wake_growth_pct_max": wake_drift_budget,
        "repaint_ops_per_kb_max": repaint_budget,
        "low_visibility_fraction_of_p90": low_visibility_fraction,
        "low_visibility_ratio_max": low_visibility_ratio_budget,
    },
    "ansi_metrics": timing_doc.get("ansi_metrics", {}),
}

report = {
    "schema_version": "tui_soak_gate.v1",
    "harness": "tui_full_traversal",
    "bead": "br-legjy.6.3",
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "samples": [sample],
    "all_within_budget": sample["within_budget"],
    "failing_samples": [] if sample["within_budget"] else [{"scenario": sample["scenario"], "reasons": sample["reasons"]}],
    "artifacts": {
        "timing_file": timing_path,
        "profile_dir": str(profile_dir),
    },
    "repro": {
        "command": "bash tests/e2e/test_tui_full_traversal.sh",
        "env": {
            "SOAK_DURATION_SECONDS": duration_seconds,
            "SOAK_STEP_DELAY_MS": int(summary["duration_ms_expected"] / max(count, 1)) if count > 0 else 0,
            "SOAK_BUDGET_FIRST_BYTE_P95_MS": first_p95_budget,
            "SOAK_BUDGET_QUIESCE_P95_MS": quiesce_p95_budget,
            "SOAK_BUDGET_QUIESCE_P99_MS": quiesce_p99_budget,
            "SOAK_BUDGET_LATENCY_DRIFT_PCT_MAX": latency_drift_budget,
            "SOAK_BUDGET_CPU_DRIFT_PCT_MAX": cpu_drift_budget,
            "SOAK_BUDGET_WAKE_DRIFT_PCT_MAX": wake_drift_budget,
            "SOAK_BUDGET_REPAINT_OPS_PER_KB_MAX": repaint_budget,
            "SOAK_BUDGET_LOW_VISIBILITY_FRACTION": low_visibility_fraction,
            "SOAK_BUDGET_LOW_VISIBILITY_RATIO_MAX": low_visibility_ratio_budget,
        },
    },
}

with open(report_path, "w", encoding="utf-8") as f:
    json.dump(report, f, indent=2)
    f.write("\n")

print("=== Multi-Minute Soak Gate ===")
verdict = "PASS" if sample["within_budget"] else "FAIL"
print(
    f"  soak {verdict} "
    f"(steps={summary['step_count']}, duration_ms={summary['duration_ms_actual']:.2f}, "
    f"first_p95={summary['first_byte_p95_ms']:.2f}, quiesce_norm_p95={summary['quiesce_p95_ms']:.2f}, "
    f"quiesce_norm_p99={summary['quiesce_p99_ms']:.2f}, quiesce_raw_p95={summary['quiesce_raw_p95_ms']:.2f}, "
    f"latency_growth={summary['latency_growth_pct']:.2f}%, "
    f"cpu_growth={summary['cpu_growth_pct']:.2f}%, wake_growth={summary['wake_growth_pct']:.2f}%, "
    f"visible_p10={summary['terminal_nonblank_cells_p10']:.0f}, "
    f"low_visibility={summary['low_visibility_ratio']:.4f})"
)
if reasons:
    print("  failing=" + ",".join(reasons))

sys.exit(0 if sample["within_budget"] else 1)
PYEOF
then
    e2e_pass "soak gate: sustained mixed workload stayed within budgets"
else
    e2e_fail "soak gate: sustained mixed workload exceeded budgets; see ${SOAK_REPORT} and rerun 'am e2e run --project . tui_full_traversal'"
fi

if [ -f "${SOAK_REPORT}" ]; then
    e2e_save_artifact "soak_regression_report_copy.json" "$(cat "${SOAK_REPORT}")"
else
    e2e_fail "soak gate: report artifact missing"
fi

if python3 - "${SOAK_REPORT}" <<'PYEOF'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    report = json.load(f)

assert report["schema_version"] == "tui_soak_gate.v1"
assert report["bead"] == "br-legjy.6.3"
assert isinstance(report["all_within_budget"], bool)
assert len(report["samples"]) == 1
sample = report["samples"][0]
for key in ("scenario", "summary", "within_budget", "reasons", "budgets", "ansi_metrics"):
    assert key in sample
for key in (
    "step_count",
    "duration_ms_actual",
    "first_byte_p95_ms",
    "quiesce_p95_ms",
    "quiesce_p99_ms",
    "latency_growth_pct",
    "cpu_growth_pct",
    "wake_growth_pct",
    "repaint_ops_per_kb",
    "terminal_nonblank_cells_p10",
    "terminal_nonblank_cells_p90",
    "low_visibility_ratio",
):
    assert key in sample["summary"]
PYEOF
then
    e2e_pass "soak gate: report schema complete"
else
    e2e_fail "soak gate: report schema incomplete"
fi


# ────────────────────────────────────────────────────────────────────
# Case 5: Baseline profiling capture (CPU/thread/syscall/redraw churn)
# ────────────────────────────────────────────────────────────────────
if [ "${CAPTURE_BASELINE_PROFILE}" = "1" ]; then
    e2e_case_banner "baseline_profile_capture"

    PORT4="$(pick_port)"
    PROFILE_DIR="${E2E_ARTIFACT_DIR}/baseline_profile"
    mkdir -p "${PROFILE_DIR}"

    TRANSCRIPT4="${E2E_ARTIFACT_DIR}/baseline_profile_transcript.txt"
    TIMING4="${E2E_ARTIFACT_DIR}/baseline_profile_timing.json"

    KEYS4='['
    KEYS4+='{"delay_ms": 2000, "keys": "", "label": "initial_render"}'
    for i in $(seq 1 ${SCREEN_COUNT}); do
        screen_idx=$((i % SCREEN_COUNT))
        KEYS4+=",{\"delay_ms\": 700, \"keys\": \"\\t\", \"label\": \"tab_to_${SCREEN_NAMES[$screen_idx]// /_}\"}"
    done
    KEYS4+=",{\"delay_ms\": 400, \"keys\": \"q\", \"label\": \"quit\"}"
    KEYS4+=']'

    if ! E2E_PROFILE_CAPTURE=1 \
        E2E_PROFILE_DIR="${PROFILE_DIR}" \
        E2E_PROFILE_LABEL="baseline_forward" \
        run_timed_pty_interaction "baseline_profile" "${TRANSCRIPT4}" "${TIMING4}" "${KEYS4}" \
            env \
            DATABASE_URL="sqlite:////${DB1}" \
            STORAGE_ROOT="${STORAGE1}" \
            HTTP_HOST="127.0.0.1" \
            HTTP_PORT="${PORT4}" \
            HTTP_RBAC_ENABLED=0 \
            HTTP_RATE_LIMIT_ENABLED=0 \
            HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
            "${BIN}" serve --host 127.0.0.1 --port "${PORT4}"; then
        e2e_fatal "baseline profiling traversal PTY interaction failed"
    fi

    PROFILE_META="${PROFILE_DIR}/baseline_forward_profile_meta.json"
    PIDSTAT_PROCESS="${PROFILE_DIR}/baseline_forward_pidstat_process.txt"
    PIDSTAT_THREADS="${PROFILE_DIR}/baseline_forward_pidstat_threads.txt"
    PIDSTAT_WAKE="${PROFILE_DIR}/baseline_forward_pidstat_wake.txt"
    STRACE_LOG="${PROFILE_DIR}/baseline_forward_strace.log"
    PROFILE_SUMMARY="${E2E_ARTIFACT_DIR}/baseline_profile_summary.json"

    python3 - "${TIMING4}" "${PROFILE_META}" "${PIDSTAT_PROCESS}" "${PIDSTAT_THREADS}" "${PIDSTAT_WAKE}" "${STRACE_LOG}" "${PROFILE_SUMMARY}" "${E2E_ARTIFACT_DIR}" <<'PYEOF'
import datetime
import json
import os
import re
import sys

timing_path, profile_meta_path, pidstat_proc_path, pidstat_threads_path, pidstat_wake_path, strace_path, out_path, artifact_dir = sys.argv[1:9]

def q(values):
    if not values:
        return {"samples": 0}
    vals = sorted(values)
    n = len(vals)
    def pct(p):
        return vals[min(n - 1, max(0, int((n - 1) * p)))]
    return {
        "samples": n,
        "min": round(vals[0], 4),
        "max": round(vals[-1], 4),
        "mean": round(sum(vals) / n, 4),
        "p50": round(pct(0.50), 4),
        "p95": round(pct(0.95), 4),
        "p99": round(pct(0.99), 4),
    }

def load_json(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}

def parse_pidstat_process(path):
    cpu, usr, sysc, wait = [], [], [], []
    if not os.path.exists(path):
        return {"available": False, "samples": 0}
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not re.match(r"^\d{2}:\d{2}:\d{2}\s+(AM|PM)\s+", line):
                continue
            parts = line.split()
            if len(parts) < 11:
                continue
            try:
                usr.append(float(parts[4]))
                sysc.append(float(parts[5]))
                wait.append(float(parts[7]))
                cpu.append(float(parts[8]))
            except ValueError:
                continue
    return {
        "available": True,
        "cpu_percent": q(cpu),
        "usr_percent": q(usr),
        "system_percent": q(sysc),
        "wait_percent": q(wait),
        "samples": len(cpu),
    }

def parse_pidstat_threads(path):
    if not os.path.exists(path):
        return {"available": False, "samples": 0, "threads_observed": 0}
    by_tid = {}
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not re.match(r"^\d{2}:\d{2}:\d{2}\s+(AM|PM)\s+", line):
                continue
            parts = line.split()
            if len(parts) < 12:
                continue
            tgid = parts[3]
            tid = parts[4]
            if tid == "-":
                continue
            try:
                usr = float(parts[5])
                sysc = float(parts[6])
                wait = float(parts[8])
                cpu = float(parts[9])
            except ValueError:
                continue
            slot = by_tid.setdefault(tid, {"tgid": tgid, "cpu": [], "usr": [], "sys": [], "wait": []})
            slot["cpu"].append(cpu)
            slot["usr"].append(usr)
            slot["sys"].append(sysc)
            slot["wait"].append(wait)

    all_cpu = []
    for item in by_tid.values():
        all_cpu.extend(item["cpu"])

    top_threads = []
    for tid, item in by_tid.items():
        if not item["cpu"]:
            continue
        top_threads.append({
            "tid": tid,
            "tgid": item["tgid"],
            "mean_cpu_percent": round(sum(item["cpu"]) / len(item["cpu"]), 4),
            "max_cpu_percent": round(max(item["cpu"]), 4),
            "mean_wait_percent": round(sum(item["wait"]) / len(item["wait"]), 4),
            "samples": len(item["cpu"]),
        })
    top_threads.sort(key=lambda x: x["mean_cpu_percent"], reverse=True)

    return {
        "available": True,
        "samples": len(all_cpu),
        "threads_observed": len(by_tid),
        "cpu_percent": q(all_cpu),
        "top_threads": top_threads[:12],
    }

def parse_pidstat_wake(path):
    cswch = []
    nvcswch = []
    by_tid = {}
    if not os.path.exists(path):
        return {"available": False, "samples": 0}
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not re.match(r"^\d{2}:\d{2}:\d{2}\s+(AM|PM)\s+", line):
                continue
            parts = line.split()
            if len(parts) < 8:
                continue
            tid = parts[4]
            if tid == "-":
                continue
            try:
                cs = float(parts[5])
                ncs = float(parts[6])
            except ValueError:
                continue
            cswch.append(cs)
            nvcswch.append(ncs)
            by_tid.setdefault(tid, []).append(cs + ncs)

    top_tid = sorted(
        ({"tid": tid, "mean_switches_per_s": round(sum(v)/len(v), 4), "max_switches_per_s": round(max(v), 4), "samples": len(v)} for tid, v in by_tid.items()),
        key=lambda x: x["mean_switches_per_s"],
        reverse=True,
    )[:12]
    return {
        "available": True,
        "samples": len(cswch),
        "voluntary_cswitch_per_s": q(cswch),
        "involuntary_cswitch_per_s": q(nvcswch),
        "top_threads_by_switch_rate": top_tid,
    }

def parse_strace(path):
    summary = {
        "available": os.path.exists(path),
        "line_count": 0,
        "syscall_counts": {},
        "wait_syscalls_total": 0,
        "wait_timedout_total": 0,
        "short_wait_le_5ms": 0,
        "zero_timeout_polls": 0,
        "write_calls": 0,
        "write_bytes_returned": 0,
    }
    if not os.path.exists(path):
        return summary
    wait_names = {
        "futex", "poll", "ppoll", "epoll_wait", "epoll_pwait",
        "select", "pselect6", "nanosleep", "clock_nanosleep",
    }
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            m = re.match(r"^\d+\s+\d{2}:\d{2}:\d{2}\.\d+\s+([a-zA-Z0-9_]+)\(", line)
            if not m:
                continue
            name = m.group(1)
            summary["line_count"] += 1
            summary["syscall_counts"][name] = summary["syscall_counts"].get(name, 0) + 1
            duration = None
            d = re.search(r"<([0-9]+\.[0-9]+)>$", line)
            if d:
                try:
                    duration = float(d.group(1))
                except ValueError:
                    duration = None
            if name in wait_names:
                summary["wait_syscalls_total"] += 1
                if "ETIMEDOUT" in line:
                    summary["wait_timedout_total"] += 1
                if duration is not None and duration <= 0.005:
                    summary["short_wait_le_5ms"] += 1
            if name == "poll" and re.search(r"poll\([^,]+,\s*[^,]+,\s*0\)", line):
                summary["zero_timeout_polls"] += 1
            if name == "ppoll" and "tv_sec=0" in line and "tv_nsec=0" in line:
                summary["zero_timeout_polls"] += 1
            if name in ("epoll_wait", "epoll_pwait") and re.search(r",\s*0\)\s*=", line):
                summary["zero_timeout_polls"] += 1
            if name in ("write", "writev"):
                summary["write_calls"] += 1
                r = re.search(r"=\s*(-?\d+)", line)
                if r:
                    n = int(r.group(1))
                    if n > 0:
                        summary["write_bytes_returned"] += n
    summary["top_syscalls"] = sorted(summary["syscall_counts"].items(), key=lambda kv: kv[1], reverse=True)[:12]
    return summary

timing_doc = load_json(timing_path)
profile_doc = load_json(profile_meta_path)
timings = timing_doc.get("timings", [])
screen_steps = [t for t in timings if str(t.get("step", "")).startswith("tab_to_")]
step_bytes = [float(t.get("output_bytes_delta", 0) or 0) for t in screen_steps]
quiesce = [float(t.get("quiesce_ms", 0) or 0) for t in screen_steps]
first_byte = [float(t.get("first_byte_ms", 0) or 0) for t in screen_steps if t.get("first_byte_ms") is not None]
render = [float(t.get("render_ms", 0) or 0) for t in screen_steps]

summary = {
    "harness": "tui_full_traversal",
    "bead": "br-legjy.1.2",
    "scenario_id": os.path.basename(artifact_dir.rstrip("/")),
    "captured_at_utc": datetime.datetime.utcnow().replace(microsecond=0).isoformat() + "Z",
    "timeline": {
        "screen_steps": len(screen_steps),
        "step_output_bytes": q(step_bytes),
        "first_byte_ms": q(first_byte),
        "quiesce_ms": q(quiesce),
        "render_ms": q(render),
    },
    "process_cpu": parse_pidstat_process(pidstat_proc_path),
    "thread_cpu": parse_pidstat_threads(pidstat_threads_path),
    "wake_behavior": parse_pidstat_wake(pidstat_wake_path),
    "syscall_profile": parse_strace(strace_path),
    "redraw_write_churn": {
        "ansi_metrics": timing_doc.get("ansi_metrics", {}),
        "timed_step_total_bytes": round(sum(step_bytes), 4),
    },
    "tool_paths": {
        "profile_meta": profile_meta_path,
        "pidstat_process": pidstat_proc_path,
        "pidstat_threads": pidstat_threads_path,
        "pidstat_wake": pidstat_wake_path,
        "strace_log": strace_path,
        "profile_meta_exists": os.path.exists(profile_meta_path),
        "pidstat_process_exists": os.path.exists(pidstat_proc_path),
        "pidstat_threads_exists": os.path.exists(pidstat_threads_path),
        "pidstat_wake_exists": os.path.exists(pidstat_wake_path),
        "strace_exists": os.path.exists(strace_path),
    },
    "raw_profile_meta": profile_doc,
    "repro": {
        "command": "E2E_CAPTURE_BASELINE_PROFILE=1 bash scripts/e2e_tui_full_traversal.sh",
        "notes": "Use the same fixture profile and terminal size vars for replay comparability.",
    },
}

with open(out_path, "w", encoding="utf-8") as f:
    json.dump(summary, f, indent=2)
PYEOF

    if [ -f "${PROFILE_SUMMARY}" ]; then
        e2e_pass "baseline profile: summary JSON written"
        e2e_save_artifact "baseline_profile_summary_copy.json" "$(cat "${PROFILE_SUMMARY}")"
    else
        e2e_fail "baseline profile: summary JSON missing"
    fi

    if python3 -c "
import json
s = json.load(open('${PROFILE_SUMMARY}'))
assert s['timeline']['screen_steps'] >= 14
if s['tool_paths']['pidstat_process_exists']:
    assert s['process_cpu']['samples'] > 0
if s['tool_paths']['pidstat_threads_exists']:
    assert s['thread_cpu']['samples'] > 0
if s['tool_paths']['strace_exists']:
    assert s['syscall_profile']['line_count'] > 0
" 2>/dev/null; then
        e2e_pass "baseline profile: core evidence present (timeline + available profiler outputs)"
    else
        e2e_fail "baseline profile: missing expected profiler evidence"
    fi

    if [ "${BASELINE_PROFILE_STRICT}" = "1" ]; then
        if python3 -c "
import json
s = json.load(open('${PROFILE_SUMMARY}'))
assert s['process_cpu']['samples'] > 0
assert s['thread_cpu']['samples'] > 0
assert s['syscall_profile']['line_count'] > 0
" 2>/dev/null; then
            e2e_pass "baseline profile strict gate: pidstat + strace evidence populated"
        else
            e2e_fail "baseline profile strict gate failed"
        fi
    fi

    # ────────────────────────────────────────────────────────────────
    # Case 6: Cross-layer bottleneck attribution report (A3)
    # ────────────────────────────────────────────────────────────────
    ATTRIBUTION_REPORT="${E2E_ARTIFACT_DIR}/cross_layer_attribution_report.json"
    python3 - "${PROFILE_SUMMARY}" "${RESULTS_FILE}" "${ATTRIBUTION_REPORT}" "${E2E_ARTIFACT_DIR}" <<'PYEOF'
import datetime
import json
import os
import sys

profile_summary_path, traversal_results_path, output_path, artifact_dir = sys.argv[1:5]

def load_json(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)

profile = load_json(profile_summary_path)
traversal = load_json(traversal_results_path)

syscalls = profile.get("syscall_profile", {})
threads = profile.get("thread_cpu", {})
wake = profile.get("wake_behavior", {})
timeline = profile.get("timeline", {})
redraw = profile.get("redraw_write_churn", {}).get("ansi_metrics", {})
forward = traversal.get("forward", {}).get("latencies", [])

def safe(v, default=0.0):
    try:
        return float(v)
    except Exception:
        return float(default)

wait_total = int(syscalls.get("wait_syscalls_total", 0) or 0)
short_wait = int(syscalls.get("short_wait_le_5ms", 0) or 0)
thread_count = int(threads.get("threads_observed", 0) or 0)
wake_p95 = safe(wake.get("voluntary_cswitch_per_s", {}).get("p95", 0))
cursor_home = int(redraw.get("cursor_home_ops", 0) or 0)
write_bytes = int(syscalls.get("write_bytes_returned", 0) or 0)
quiesce_mean = safe(timeline.get("quiesce_ms", {}).get("mean", 0))
first_byte_mean = safe(timeline.get("first_byte_ms", {}).get("mean", 0))

top_screens = sorted(
    (
        {
            "screen": row.get("screen"),
            "output_bytes_delta": int(row.get("output_bytes_delta", 0) or 0),
            "quiesce_ms": safe(row.get("quiesce_ms", 0)),
        }
        for row in forward
    ),
    key=lambda r: r["output_bytes_delta"],
    reverse=True,
)[:5]

def confidence(high, medium):
    if high:
        return "high"
    if medium:
        return "medium"
    return "low"

asupersync_conf = confidence(wait_total >= 700 and thread_count >= 30, wait_total >= 300 or thread_count >= 20)
frankentui_conf = confidence(cursor_home >= 500 and write_bytes >= 150000, cursor_home >= 200 or write_bytes >= 70000)
app_conf = confidence(len(top_screens) >= 3 and top_screens[0]["output_bytes_delta"] >= 40000, len(top_screens) >= 1)

bottlenecks = [
    {
        "rank": 1,
        "id": "runtime-wake-contention",
        "layer": "/dp/asupersync",
        "scope": "cross_project",
        "symptom": "Short-timeout wait churn and high thread fanout",
        "evidence": {
            "wait_syscalls_total": wait_total,
            "short_wait_le_5ms": short_wait,
            "threads_observed": thread_count,
            "wake_voluntary_p95_per_s": wake_p95,
        },
        "confidence": asupersync_conf,
        "expected_gain_band": "high",
        "implementation_risk": "medium",
        "mapped_next_bead": "br-legjy.2.1",
        "falsify_if": "Reducing timeout churn in /dp/asupersync does not materially change quiesce_ms distribution under identical traversal.",
    },
    {
        "rank": 2,
        "id": "event-loop-redraw-write-amplification",
        "layer": "/dp/frankentui",
        "scope": "cross_project",
        "symptom": "Heavy cursor-home/write activity during each screen activation",
        "evidence": {
            "cursor_home_ops": cursor_home,
            "write_bytes_returned": write_bytes,
            "first_byte_mean_ms": first_byte_mean,
            "quiesce_mean_ms": quiesce_mean,
        },
        "confidence": frankentui_conf,
        "expected_gain_band": "medium_high",
        "implementation_risk": "medium",
        "mapped_next_bead": "br-legjy.3.1",
        "falsify_if": "After reducing zero-work redraw/event-drain cost in /dp/frankentui, output-byte churn remains flat and quiesce_ms does not improve.",
    },
    {
        "rank": 3,
        "id": "screen-specific-data-volume",
        "layer": "local_app",
        "scope": "local_project",
        "symptom": "Uneven per-screen output load suggests app-level render/data shaping hotspots",
        "evidence": {
            "top_forward_screens_by_output_bytes": top_screens,
        },
        "confidence": app_conf,
        "expected_gain_band": "medium",
        "implementation_risk": "low_medium",
        "mapped_next_bead": "br-legjy.4.1",
        "falsify_if": "Local screen-level pruning/visibility scheduling changes do not reduce heavy-screen output_bytes_delta or quiesce tail latency.",
    },
]

report = {
    "harness": "tui_full_traversal",
    "bead": "br-legjy.1.3",
    "generated_at_utc": datetime.datetime.utcnow().replace(microsecond=0).isoformat() + "Z",
    "artifact_sources": {
        "baseline_profile_summary": profile_summary_path,
        "traversal_results": traversal_results_path,
        "scenario_id": os.path.basename(artifact_dir.rstrip("/")),
    },
    "layer_partition": {
        "/dp/asupersync": "Runtime scheduler + parking/wakeup behavior",
        "/dp/frankentui": "Event drain + render cadence + terminal write path",
        "local_app": "Screen-level data shaping and UI policy in mcp-agent-mail-server",
    },
    "ranked_bottlenecks": bottlenecks,
    "priority_sequence": [
        {"rank": 1, "target_bead": "br-legjy.2.1", "reason": "Dominant wait/wake contention signal."},
        {"rank": 2, "target_bead": "br-legjy.3.1", "reason": "Render/event-loop churn remains strong secondary cost."},
        {"rank": 3, "target_bead": "br-legjy.4.1", "reason": "Local visibility-aware scheduling and per-screen optimization."},
        {"rank": 4, "target_bead": "br-legjy.1.4", "reason": "Set hard budgets after attribution ordering is established."},
    ],
    "anti_patterns_to_avoid": [
        "Reducing concurrency or thread count solely to lower CPU without preserving correctness/throughput guarantees.",
        "Adding ad-hoc sleeps/timeouts to mask churn rather than fixing scheduler/event-loop root causes.",
        "Suppressing redraws globally (stale UI risk) instead of visibility-aware render policy.",
        "Chasing local app micro-optimizations before resolving cross-project dominant costs.",
    ],
    "falsification_hooks": [
        {
            "layer": "/dp/asupersync",
            "check": "wait_syscalls_total and short_wait_le_5ms should drop materially under identical traversal replay",
            "repro_command": "E2E_CAPTURE_BASELINE_PROFILE=1 bash tests/e2e/test_tui_full_traversal.sh",
        },
        {
            "layer": "/dp/frankentui",
            "check": "cursor_home_ops/write_bytes_returned and quiesce tail should decrease after event-drain changes",
            "repro_command": "E2E_CAPTURE_BASELINE_PROFILE=1 bash tests/e2e/test_tui_full_traversal.sh",
        },
        {
            "layer": "local_app",
            "check": "top heavy screens should show lower output_bytes_delta after visibility-aware scheduling",
            "repro_command": "E2E_CAPTURE_BASELINE_PROFILE=1 bash tests/e2e/test_tui_full_traversal.sh",
        },
    ],
    "verification_commands": [
        "bash -n scripts/e2e_tui_full_traversal.sh",
        "bash tests/e2e/test_tui_full_traversal.sh",
    ],
}

with open(output_path, "w", encoding="utf-8") as f:
    json.dump(report, f, indent=2)
PYEOF

    if python3 -c "
import json
r = json.load(open('${ATTRIBUTION_REPORT}'))
assert r['bead'] == 'br-legjy.1.3'
assert len(r['ranked_bottlenecks']) >= 3
assert r['ranked_bottlenecks'][0]['layer'] == '/dp/asupersync'
assert any(x['layer'] == '/dp/frankentui' for x in r['ranked_bottlenecks'])
assert any(x['layer'] == 'local_app' for x in r['ranked_bottlenecks'])
assert len(r['anti_patterns_to_avoid']) >= 3
" 2>/dev/null; then
        e2e_pass "attribution: cross_layer_attribution_report.json generated with ranked cross-layer mapping"
        e2e_save_artifact "cross_layer_attribution_report_copy.json" "$(cat "${ATTRIBUTION_REPORT}")"
    else
        e2e_fail "attribution: report missing required fields"
    fi
else
    e2e_skip "baseline profiling disabled (E2E_CAPTURE_BASELINE_PROFILE=0)"
fi


# ────────────────────────────────────────────────────────────────────
# Case 6: Incident gate triage digest artifact (F4 support)
# ────────────────────────────────────────────────────────────────────
e2e_case_banner "incident_gate_triage_digest"

TRIAGE_DIGEST="${E2E_ARTIFACT_DIR}/lag_flash_gate_triage.md"
if python3 - "${E2E_ARTIFACT_DIR}" "${TRIAGE_DIGEST}" <<'PYEOF'
import json
import sys
from pathlib import Path

artifact_dir = Path(sys.argv[1])
output_path = Path(sys.argv[2])

reports = [
    ("Traversal latency gate", "traversal_gate_verdict.json"),
    ("Pressure/resize regression gate", "pressure_resize_regression_report.json"),
    ("Flash detection gate", "flash_detection_report.json"),
    ("Multi-minute soak gate", "soak_regression_report.json"),
]

lines = [
    "# Lag/Flash Incident Gate Triage Digest",
    "",
    f"- Artifact directory: `{artifact_dir}`",
    "- Primary local repro: `am e2e run --project . tui_full_traversal`",
    "- Deep attribution repro: `E2E_CAPTURE_BASELINE_PROFILE=1 am e2e run --project . tui_full_traversal`",
    "",
]

for title, filename in reports:
    path = artifact_dir / filename
    lines.append(f"## {title}")
    lines.append(f"- Artifact: `{path}`")

    if not path.is_file():
        lines.append("- Status: missing artifact")
        lines.append("- Next step: rerun `am e2e run --project . tui_full_traversal` and inspect suite logs")
        lines.append("")
        continue

    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        lines.append("- Status: artifact unreadable")
        lines.append(f"- Parse error: `{exc}`")
        lines.append("")
        continue

    all_within_budget = report.get("all_within_budget")
    if all_within_budget is True:
        lines.append("- Status: PASS (`all_within_budget=true`)")
    elif all_within_budget is False:
        lines.append("- Status: FAIL (`all_within_budget=false`)")
    else:
        lines.append("- Status: UNKNOWN (missing `all_within_budget`)")

    failing_samples = report.get("failing_samples") or []
    if failing_samples:
        lines.append("- Failing samples:")
        for sample in failing_samples:
            lines.append(f"  - `{json.dumps(sample, sort_keys=True)}`")
    else:
        lines.append("- Failing samples: none")

    repro = report.get("repro") or {}
    repro_cmd = repro.get("command")
    if repro_cmd:
        lines.append(f"- Report repro command: `{repro_cmd}`")
    repro_env = repro.get("env") or {}
    if repro_env:
        lines.append("- Report repro env:")
        for key in sorted(repro_env):
            lines.append(f"  - `{key}={repro_env[key]}`")

    lines.append("")

output_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
PYEOF
then
    e2e_pass "incident gate triage: lag_flash_gate_triage.md generated"
    e2e_save_artifact "lag_flash_gate_triage_copy.md" "$(cat "${TRIAGE_DIGEST}")"
else
    e2e_fail "incident gate triage: failed to generate lag_flash_gate_triage.md"
fi


# ────────────────────────────────────────────────────────────────────
# Summary
# ────────────────────────────────────────────────────────────────────
e2e_summary
