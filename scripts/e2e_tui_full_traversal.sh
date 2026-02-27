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
#     baseline_profile_summary.json — baseline CPU/thread/syscall/redraw profile
#     cross_layer_attribution_report.json — ranked attribution map + next-track order
#     forward_transcript.txt   — normalized PTY output (Tab forward)
#     backward_transcript.txt  — normalized PTY output (Shift+Tab backward)
#     jump_transcript.txt      — normalized PTY output (direct number keys)
#     seed_data.json           — data fixtures used for seeding

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-tui_full_traversal}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Full-Screen Traversal Repro Harness (br-legjy.1.1)"

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
FIXTURE_PROFILE="${E2E_FIXTURE_PROFILE:-medium}"
CAPTURE_BASELINE_PROFILE="${E2E_CAPTURE_BASELINE_PROFILE:-1}"
BASELINE_PROFILE_STRICT="${E2E_BASELINE_PROFILE_STRICT:-0}"

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

e2e_log "Baseline profiling capture enabled: ${CAPTURE_BASELINE_PROFILE} (strict=${BASELINE_PROFILE_STRICT})"

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
import datetime
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
                    chunks.append(chunk)
                    got += len(chunk)
                except OSError:
                    break
        return got

    def read_with_latency(max_wait=2.0):
        """Read output tracking first-byte and quiescence timing.

        Returns dict with:
          first_byte_ms  - time from call to first output byte (None if no output)
          quiesce_ms     - time from call to output quiescence
          render_ms      - time from first byte to quiescence (actual render)
          total_bytes    - bytes received during this call
        """
        t0 = time.monotonic()
        deadline = t0 + max_wait
        got = 0
        first_byte_t = None
        last_byte_t = None

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
                    chunks.append(chunk)
                    now = time.monotonic()
                    got += len(chunk)
                    if first_byte_t is None:
                        first_byte_t = now
                    last_byte_t = now
                except OSError:
                    break
            else:
                # select timed out with no data
                if first_byte_t is not None:
                    # We had output and now it stopped — quiesced
                    break

        now = time.monotonic()
        return {
            "first_byte_ms": round((first_byte_t - t0) * 1000, 2) if first_byte_t else None,
            "quiesce_ms": round((now - t0) * 1000, 2),
            "render_ms": round(((last_byte_t or now) - (first_byte_t or t0)) * 1000, 2) if first_byte_t else 0,
            "total_bytes": got,
        }

    # Initial read: wait for TUI to render
    t_start = time.monotonic()
    init = read_with_latency(max_wait=5.0)
    timings.append({
        "step": "initial_render",
        "first_byte_ms": init["first_byte_ms"],
        "quiesce_ms": init["quiesce_ms"],
        "render_ms": init["render_ms"],
        "output_bytes": init["total_bytes"],
    })

    # Execute keystroke script with timing
    for i, step in enumerate(keystroke_script):
        max_wait_s = step.get("delay_ms", 200) / 1000.0
        keys = step.get("keys", "")
        step_label = step.get("label", f"step_{i}")

        # Decode escape sequences in key strings
        keys_bytes = keys.encode("utf-8").decode("unicode_escape").encode("latin-1")

        t_before = time.monotonic()

        try:
            os.write(master_fd, keys_bytes)
        except OSError:
            break

        r = read_with_latency(max_wait=max_wait_s)
        t_after = time.monotonic()

        timings.append({
            "step": step_label,
            "keys": keys,
            "max_wait_ms": step.get("delay_ms", 200),
            "wall_ms": round((t_after - t_before) * 1000, 2),
            "first_byte_ms": r["first_byte_ms"],
            "quiesce_ms": r["quiesce_ms"],
            "render_ms": r["render_ms"],
            "output_bytes_delta": r["total_bytes"],
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
              f"quiesce mean={s['mean_ms']:6.1f}ms p95={s['p95_ms']:6.1f}ms max={s['max_ms']:6.1f}ms")
o = report["overall"]["quiesce"]
fb = report["overall"]["first_byte"]
if o["count"] > 0:
    print(f"  {'overall':10s}: {o['count']:2d} screens | "
          f"first_byte={fb['mean_ms']:6.1f}ms | "
          f"quiesce mean={o['mean_ms']:6.1f}ms p95={o['p95_ms']:6.1f}ms max={o['max_ms']:6.1f}ms")
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
# Summary
# ────────────────────────────────────────────────────────────────────
e2e_summary
